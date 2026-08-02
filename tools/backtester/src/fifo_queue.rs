//! Simulated fills for one resting bid + one resting ask, matching
//! `mm-bot`'s own single-quote-per-side model (see `quote_cycle.rs`).
//!
//! **This is not a real order-book queue simulation.** A true FIFO fill
//! model needs to know queue position: how much size is ahead of ours at
//! the same tick, so a trade print only fills us after that size is
//! exhausted. The NDJSON input here doesn't carry order-book depth, only
//! trade prints and mark rate updates (see `event.rs`), so there's no way
//! to know queue position from this data. What this actually simulates:
//! "would this trade print have crossed our resting rate", filling our
//! whole resting size the instant a print crosses it. That's a real,
//! named simplification, not a rounding error: it's structurally
//! optimistic (a real order sitting behind other resting size at the
//! same tick would fill later or not at all), so PnL/fill-rate numbers
//! from this tool are an upper bound on real performance, not a
//! prediction. Good enough for comparing γ/κ settings against each other
//! on the same tape; not good enough to size real risk off of directly.

use oms_core::Side;
use tick_math::FixedX18;

#[derive(Debug, Clone, Copy)]
struct RestingOrder {
    rate: FixedX18,
    size: f64,
}

#[derive(Debug, Default)]
pub struct FifoBook {
    bid: Option<RestingOrder>,
    ask: Option<RestingOrder>,
}

/// One simulated fill, size is always positive, `side` says which leg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimFill {
    pub side: Side,
    pub rate: FixedX18,
    pub size: f64,
}

impl FifoBook {
    /// Replaces whatever's currently resting. Matches `mm-bot`'s
    /// requote-in-place behavior (`requote_side`), not a cancel+re-add
    /// with a fresh queue position, there's no queue position modeled
    /// here to lose anyway.
    pub fn set_bid(&mut self, rate: FixedX18, size: f64) {
        self.bid = Some(RestingOrder { rate, size });
    }

    pub fn set_ask(&mut self, rate: FixedX18, size: f64) {
        self.ask = Some(RestingOrder { rate, size });
    }

    #[allow(dead_code)] // part of FifoBook's public API, exercised by tests, not called from engine.rs's replay path yet
    pub fn cancel_bid(&mut self) {
        self.bid = None;
    }

    #[allow(dead_code)]
    pub fn cancel_ask(&mut self) {
        self.ask = None;
    }

    /// A trade prints at `trade_rate`. Fills our bid whole if
    /// `trade_rate <= bid_rate` (someone traded at least as favorably as
    /// what we were willing to pay), our ask whole if
    /// `trade_rate >= ask_rate`, same convention `quoting-engine::bounds`
    /// uses (LONG/bid needs `rate <= bound`, SHORT/ask needs
    /// `rate >= bound`). A filled leg is removed from the book, real
    /// resting orders don't survive their own fill either.
    pub fn on_trade(&mut self, trade_rate: FixedX18) -> Vec<SimFill> {
        let mut fills = Vec::new();

        if let Some(bid) = self.bid {
            if trade_rate <= bid.rate {
                fills.push(SimFill { side: Side::Long, rate: bid.rate, size: bid.size });
                self.bid = None;
            }
        }
        if let Some(ask) = self.ask {
            if trade_rate >= ask.rate {
                fills.push(SimFill { side: Side::Short, rate: ask.rate, size: ask.size });
                self.ask = None;
            }
        }

        fills
    }

    #[allow(dead_code)]
    pub fn resting_bid(&self) -> Option<(FixedX18, f64)> {
        self.bid.map(|o| (o.rate, o.size))
    }

    #[allow(dead_code)]
    pub fn resting_ask(&self) -> Option<(FixedX18, f64)> {
        self.ask.map(|o| (o.rate, o.size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trade_at_the_bid_rate_fills_it() {
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        let fills = book.on_trade(FixedX18::from_f64(0.05));
        assert_eq!(fills, vec![SimFill { side: Side::Long, rate: FixedX18::from_f64(0.05), size: 100.0 }]);
        assert!(book.resting_bid().is_none());
    }

    #[test]
    fn a_trade_below_the_bid_rate_still_fills_it() {
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        let fills = book.on_trade(FixedX18::from_f64(0.03));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, Side::Long);
    }

    #[test]
    fn a_trade_above_the_bid_rate_does_not_fill_it() {
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        assert!(book.on_trade(FixedX18::from_f64(0.06)).is_empty());
        assert!(book.resting_bid().is_some());
    }

    #[test]
    fn a_trade_at_the_ask_rate_fills_it() {
        let mut book = FifoBook::default();
        book.set_ask(FixedX18::from_f64(0.06), 50.0);
        let fills = book.on_trade(FixedX18::from_f64(0.06));
        assert_eq!(fills, vec![SimFill { side: Side::Short, rate: FixedX18::from_f64(0.06), size: 50.0 }]);
        assert!(book.resting_ask().is_none());
    }

    #[test]
    fn a_single_trade_can_never_cross_both_sides_when_bid_is_below_ask() {
        // trade_rate would need to be simultaneously <= bid.rate and
        // >= ask.rate, impossible whenever bid.rate < ask.rate (a valid
        // quote never crosses itself, QuotingEngine::quote itself rejects
        // bid >= ask before returning one, see CrossedAfterClamp)
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        book.set_ask(FixedX18::from_f64(0.055), 50.0);
        for probe in [0.0, 0.05, 0.052, 0.055, 0.1] {
            let fills = book.on_trade(FixedX18::from_f64(probe));
            assert!(fills.len() <= 1, "probe {probe} filled {} legs, expected at most 1", fills.len());
        }
    }

    #[test]
    fn set_bid_replaces_whatever_was_resting_without_a_fill() {
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        book.set_bid(FixedX18::from_f64(0.04), 200.0);
        assert_eq!(book.resting_bid(), Some((FixedX18::from_f64(0.04), 200.0)));
    }

    #[test]
    fn cancel_bid_removes_it_without_a_fill() {
        let mut book = FifoBook::default();
        book.set_bid(FixedX18::from_f64(0.05), 100.0);
        book.cancel_bid();
        assert!(book.resting_bid().is_none());
        assert!(book.on_trade(FixedX18::from_f64(0.01)).is_empty());
    }

    #[test]
    fn no_resting_orders_means_no_fills_regardless_of_trade_rate() {
        let mut book = FifoBook::default();
        assert!(book.on_trade(FixedX18::from_f64(0.0)).is_empty());
    }
}
