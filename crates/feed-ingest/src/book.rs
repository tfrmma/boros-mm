//! Every update is a full aggregated snapshot, bucketed by `TICK_SIZE`, up
//! to 50 levels a side, no delta and no sequence number (see event.rs's
//! `OrderbookUpdate` doc comment).
//!
//! Levels aren't assumed to arrive in any particular sort order (the doc
//! doesn't say either way), so this sorts on every snapshot instead of
//! trusting wire order. At <=50 levels a side and snapshot-only cadence
//! (not per-tick), that's cheap enough not to think about twice.

use tick_math::FixedX18;

use crate::error::FeedError;
use crate::event::{BookEvent, OrderbookSide};

/// One decoded, populated price level. `ia` is the raw bucket index as it
/// came over the wire, kept around for debugging, `rate` is the value
/// anyone actually wants (`ia * tick_size`).
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub ia: f64,
    pub rate: f64,
    pub size: FixedX18,
}

#[derive(Debug, Clone)]
pub struct BookDepth {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub block_number: u64,
    pub timestamp: u64,
}

/// One market's orderbook. Long = bids (see bounds.rs in quoting-engine for
/// the same bid=Long/ask=Short convention, sourced from oms-core's order
/// priority tests, not invented separately here).
pub struct OrderBook {
    market_id: u32,
    tick_size: f64,
    bids: Vec<Level>, // sorted best-first: highest rate first
    asks: Vec<Level>, // sorted best-first: lowest rate first
    block_number: u64,
    timestamp: u64,
    initialized: bool,
}

impl OrderBook {
    pub fn new(market_id: u32, tick_size: f64) -> Self {
        Self {
            market_id,
            tick_size,
            bids: Vec::new(),
            asks: Vec::new(),
            block_number: 0,
            timestamp: 0,
            initialized: false,
        }
    }

    /// Replace the book with a fresh snapshot. There's no delta to apply
    /// incrementally, this protocol doesn't have one, every message is the
    /// full state.
    pub fn apply(&mut self, event: &BookEvent) -> Result<(), FeedError> {
        self.bids = decode_side(&event.long, self.tick_size)?;
        self.asks = decode_side(&event.short, self.tick_size)?;
        self.bids.sort_by(|a, b| b.rate.total_cmp(&a.rate)); // highest first
        self.asks.sort_by(|a, b| a.rate.total_cmp(&b.rate)); // lowest first
        self.block_number = event.sync_status.block_number;
        self.timestamp = event.sync_status.timestamp;
        self.initialized = true;
        Ok(())
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.asks.first().copied()
    }

    pub fn bbo(&self) -> Option<(Level, Level)> {
        Some((self.best_bid()?, self.best_ask()?))
    }

    pub fn mid_rate(&self) -> Option<f64> {
        let (bid, ask) = self.bbo()?;
        Some((bid.rate + ask.rate) / 2.0)
    }

    pub fn spread(&self) -> Option<f64> {
        let (bid, ask) = self.bbo()?;
        Some((ask.rate - bid.rate).max(0.0)) // crossed shouldn't happen, don't return negative if it does
    }

    pub fn depth(&self, n: usize) -> BookDepth {
        BookDepth {
            bids: self.bids.iter().take(n).copied().collect(),
            asks: self.asks.iter().take(n).copied().collect(),
            block_number: self.block_number,
            timestamp: self.timestamp,
        }
    }

    pub fn market_id(&self) -> u32 {
        self.market_id
    }
    pub fn tick_size(&self) -> f64 {
        self.tick_size
    }
    pub fn block_number(&self) -> u64 {
        self.block_number
    }
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    pub fn bid_levels(&self) -> usize {
        self.bids.len()
    }
    pub fn ask_levels(&self) -> usize {
        self.asks.len()
    }
}

fn decode_side(side: &OrderbookSide, tick_size: f64) -> Result<Vec<Level>, FeedError> {
    if side.ia.len() != side.sz.len() {
        return Err(FeedError::Protocol(format!(
            "orderbook side: ia has {} entries, sz has {}, expected matching lengths",
            side.ia.len(),
            side.sz.len()
        )));
    }
    side.ia
        .iter()
        .zip(&side.sz)
        .filter(|(_, sz)| sz.as_str() != "0")
        .map(|(&ia, sz)| {
            crate::event::parse_fixed_x18_raw(sz).map(|size| Level { ia, rate: ia * tick_size, size })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SyncStatus;

    fn sz(raw: i128) -> String {
        raw.to_string()
    }

    fn snapshot(long: Vec<(f64, i128)>, short: Vec<(f64, i128)>) -> BookEvent {
        BookEvent {
            market_id: 1,
            tick_size: crate::event::TickSize::new(0.01).unwrap(),
            include_amm: false,
            long: OrderbookSide {
                ia: long.iter().map(|(ia, _)| *ia).collect(),
                sz: long.iter().map(|(_, s)| sz(*s)).collect(),
            },
            short: OrderbookSide {
                ia: short.iter().map(|(ia, _)| *ia).collect(),
                sz: short.iter().map(|(_, s)| sz(*s)).collect(),
            },
            sync_status: SyncStatus { block_number: 100, timestamp: 12345 },
        }
    }

    #[test]
    fn bbo_picks_highest_bid_lowest_ask_regardless_of_wire_order() {
        let mut book = OrderBook::new(1, 0.01);
        // out of order on the wire
        book.apply(&snapshot(
            vec![(3.0, 1_000_000_000_000_000_000), (8.0, 2_000_000_000_000_000_000), (5.0, 1_000_000_000_000_000_000)],
            vec![(12.0, 1_000_000_000_000_000_000), (9.0, 1_000_000_000_000_000_000), (15.0, 1_000_000_000_000_000_000)],
        )).unwrap();

        let (bid, ask) = book.bbo().expect("no bbo");
        assert!((bid.rate - 0.08).abs() < 1e-12, "best bid should be ia=8 (highest), got rate {}", bid.rate);
        assert!((ask.rate - 0.09).abs() < 1e-12, "best ask should be ia=9 (lowest), got rate {}", ask.rate);
    }

    #[test]
    fn zero_size_levels_are_dropped() {
        let mut book = OrderBook::new(1, 0.01);
        book.apply(&snapshot(vec![(5.0, 0), (3.0, 1_000_000_000_000_000_000)], vec![])).unwrap();
        assert_eq!(book.bid_levels(), 1);
        assert!((book.best_bid().unwrap().rate - 0.03).abs() < 1e-12);
    }

    #[test]
    fn snapshot_fully_replaces_previous_state_no_merging() {
        let mut book = OrderBook::new(1, 0.01);
        book.apply(&snapshot(vec![(3.0, 1_000_000_000_000_000_000)], vec![])).unwrap();
        assert_eq!(book.bid_levels(), 1);

        // a level from the first snapshot that's absent from the second
        // must be gone, not merged, there's no delta semantics here
        book.apply(&snapshot(vec![(7.0, 1_000_000_000_000_000_000)], vec![])).unwrap();
        assert_eq!(book.bid_levels(), 1);
        assert!((book.best_bid().unwrap().rate - 0.07).abs() < 1e-12);
    }

    #[test]
    fn depth_respects_n_and_sort_order() {
        let mut book = OrderBook::new(1, 0.01);
        book.apply(&snapshot(
            vec![(1.0, 1_000_000_000_000_000_000), (5.0, 1_000_000_000_000_000_000), (3.0, 1_000_000_000_000_000_000)],
            vec![(10.0, 1_000_000_000_000_000_000), (8.0, 1_000_000_000_000_000_000)],
        )).unwrap();

        let depth = book.depth(2);
        assert_eq!(depth.bids.len(), 2);
        assert!(depth.bids[0].rate > depth.bids[1].rate, "bids must be best-first (descending)");
        assert_eq!(depth.asks.len(), 2);
        assert!(depth.asks[0].rate < depth.asks[1].rate, "asks must be best-first (ascending)");
    }

    #[test]
    fn mismatched_ia_sz_lengths_is_a_protocol_error_not_a_panic() {
        let mut book = OrderBook::new(1, 0.01);
        let mut bad = snapshot(vec![(1.0, 1)], vec![]);
        bad.long.sz.push(sz(1)); // now ia has 1 entry, sz has 2
        assert!(matches!(book.apply(&bad), Err(FeedError::Protocol(_))));
    }

    #[test]
    fn empty_book_has_no_bbo_and_is_marked_uninitialized_before_first_apply() {
        let book = OrderBook::new(1, 0.01);
        assert!(!book.is_initialized());
        assert!(book.bbo().is_none());
    }

    #[test]
    fn spread_and_mid_are_consistent_with_bbo() {
        let mut book = OrderBook::new(1, 0.01);
        book.apply(&snapshot(vec![(4.0, 1_000_000_000_000_000_000)], vec![(6.0, 1_000_000_000_000_000_000)])).unwrap();
        assert!((book.mid_rate().unwrap() - 0.05).abs() < 1e-12);
        assert!((book.spread().unwrap() - 0.02).abs() < 1e-12);
    }
}
