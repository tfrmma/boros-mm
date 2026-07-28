use std::collections::HashMap;

use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    book::{BookDepth, Level, OrderBook},
    event::{BookEvent, TickSize},
};

/// Key a book by market + tick_size + include_amm, not just market_id.
/// `boros/mod.rs` lets a market subscribe to `orderbook` and
/// `orderbook-include-amm` at the same time (they're independent
/// channels), and in principle to more than one `tick_size` granularity
/// too. Keying on `market_id` alone would let whichever update lands last
/// silently overwrite the other view, this keeps them apart instead.
type BookKey = (u32, TickSize, bool);

/// Maintains per-market L2 book state on top of the broadcast feed.
///
/// Call `drain()` in your main loop to process pending events before
/// reading BBO or depth. This is intentionally synchronous, your event
/// loop controls when state updates happen, not the feed task.
///
/// Every update from `boros/mod.rs` is a full snapshot (see book.rs), so
/// there's no seq-gap concept anymore, and no partial/inconsistent state
/// to invalidate on error, either a snapshot decodes or it's dropped and
/// the previous good snapshot stays in place until the next one arrives.
pub struct BookStateManager {
    books: HashMap<BookKey, OrderBook>,
    rx: broadcast::Receiver<BookEvent>,
}

impl BookStateManager {
    pub fn new(rx: broadcast::Receiver<BookEvent>) -> Self {
        Self { books: HashMap::new(), rx }
    }

    /// Drain all pending book events. Call this before reading any BBO/depth.
    /// Non-blocking, returns immediately if no events are queued.
    pub fn drain(&mut self) {
        use broadcast::error::TryRecvError;

        loop {
            match self.rx.try_recv() {
                Ok(ev) => self.apply(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(n)) => {
                    warn!(skipped = n, "book state lagged, missed snapshots are just gone, next one that arrives replaces state as usual");
                    // don't break, more events might be queued after the gap
                }
            }
        }
    }

    fn apply(&mut self, ev: BookEvent) {
        let key = (ev.market_id, ev.tick_size, ev.include_amm);
        let book = self.books.entry(key).or_insert_with(|| OrderBook::new(ev.market_id, ev.tick_size.value()));

        if let Err(e) = book.apply(&ev) {
            // a snapshot that doesn't decode is dropped, the book just
            // keeps whatever it had until a good one arrives, there's
            // nothing to invalidate since there's no partial-apply state
            // to have gotten corrupted (unlike the old delta model).
            warn!(market_id = ev.market_id, include_amm = ev.include_amm, "dropping malformed book snapshot: {e}");
        }
    }

    pub fn best_bid(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<Level> {
        self.books.get(&(market_id, tick_size, include_amm))?.best_bid()
    }

    pub fn best_ask(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<Level> {
        self.books.get(&(market_id, tick_size, include_amm))?.best_ask()
    }

    pub fn bbo(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<(Level, Level)> {
        self.books.get(&(market_id, tick_size, include_amm))?.bbo()
    }

    pub fn mid_rate(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<f64> {
        self.books.get(&(market_id, tick_size, include_amm))?.mid_rate()
    }

    pub fn spread(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<f64> {
        self.books.get(&(market_id, tick_size, include_amm))?.spread()
    }

    pub fn depth(&self, market_id: u32, tick_size: TickSize, include_amm: bool, n: usize) -> Option<BookDepth> {
        let book = self.books.get(&(market_id, tick_size, include_amm))?;
        if !book.is_initialized() {
            return None;
        }
        Some(book.depth(n))
    }

    pub fn block_number(&self, market_id: u32, tick_size: TickSize, include_amm: bool) -> Option<u64> {
        Some(self.books.get(&(market_id, tick_size, include_amm))?.block_number())
    }

    pub fn known_markets(&self) -> Vec<(u32, TickSize, bool)> {
        self.books.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{OrderbookSide, SyncStatus};

    fn event(market_id: u32, tick_size: f64, include_amm: bool, bid_ia: f64) -> BookEvent {
        BookEvent {
            market_id,
            tick_size: TickSize::new(tick_size).unwrap(),
            include_amm,
            long: OrderbookSide { ia: vec![bid_ia], sz: vec!["1000000000000000000".to_owned()] },
            short: OrderbookSide { ia: vec![], sz: vec![] },
            sync_status: SyncStatus { block_number: 1, timestamp: 1 },
        }
    }

    fn channel() -> (broadcast::Sender<BookEvent>, BookStateManager) {
        let (tx, rx) = broadcast::channel(16);
        (tx, BookStateManager::new(rx))
    }

    #[test]
    fn plain_and_amm_views_of_the_same_market_do_not_collide() {
        let (tx, mut mgr) = channel();
        let ts = TickSize::new(0.01).unwrap();
        tx.send(event(1, 0.01, false, 5.0)).unwrap();
        tx.send(event(1, 0.01, true, 9.0)).unwrap();
        mgr.drain();

        let plain = mgr.best_bid(1, ts, false).unwrap();
        let amm = mgr.best_bid(1, ts, true).unwrap();
        assert!((plain.rate - 0.05).abs() < 1e-12);
        assert!((amm.rate - 0.09).abs() < 1e-12);
    }

    #[test]
    fn different_tick_sizes_for_the_same_market_do_not_collide() {
        let (tx, mut mgr) = channel();
        tx.send(event(1, 0.01, false, 5.0)).unwrap(); // rate = 0.05
        tx.send(event(1, 0.001, false, 50.0)).unwrap(); // same real rate, different bucket, rate = 0.05
        mgr.drain();

        assert_eq!(mgr.known_markets().len(), 2, "should be two distinct book entries, not one overwriting the other");
    }

    #[test]
    fn unknown_market_returns_none_not_a_default_book() {
        let (_tx, mgr) = channel();
        assert!(mgr.bbo(999, TickSize::new(0.01).unwrap(), false).is_none());
    }

    #[test]
    fn malformed_snapshot_is_dropped_and_previous_good_state_survives() {
        let (tx, mut mgr) = channel();
        let ts = TickSize::new(0.01).unwrap();
        tx.send(event(1, 0.01, false, 5.0)).unwrap();
        mgr.drain();
        assert!((mgr.best_bid(1, ts, false).unwrap().rate - 0.05).abs() < 1e-12);

        let mut bad = event(1, 0.01, false, 7.0);
        bad.long.sz.push("1".to_owned()); // mismatched ia/sz lengths
        tx.send(bad).unwrap();
        mgr.drain();

        // still the old snapshot, not overwritten by the broken one
        assert!((mgr.best_bid(1, ts, false).unwrap().rate - 0.05).abs() < 1e-12);
    }
}
