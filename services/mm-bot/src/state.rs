use std::collections::HashMap;
use std::time::Instant;

use margin_sim::{MarginConfig, MarketState};
use oms_core::OrderTracker;
use tick_math::FixedX18;

use crate::config::MarketConfig;

/// Everything this bot tracks for one market: its static config, the
/// margin params fetched once at startup (refreshed on `reconcile`), the
/// live market state (mark rate / time-to-maturity, updated every quote
/// tick from `feed-ingest`), and which orders are currently resting.
pub struct MarketRuntime {
    pub config: MarketConfig,
    pub margin_config: MarginConfig,
    pub market_state: MarketState,
    pub tracker: OrderTracker,
    /// `(order_id, rate it was placed at)`. Rate kept alongside the id so
    /// the requote check (`did the new quote move enough to bother`) can
    /// compare against what's ACTUALLY resting, not what we last computed
    /// and assumed went through.
    pub resting_bid: Option<(oms_core::OrderId, FixedX18)>,
    pub resting_ask: Option<(oms_core::OrderId, FixedX18)>,
}

impl MarketRuntime {
    pub fn new(config: MarketConfig, margin_config: MarginConfig, market_state: MarketState) -> Self {
        let tracker = OrderTracker::new(config.tick_step);
        Self { config, margin_config, market_state, tracker, resting_bid: None, resting_ask: None }
    }
}

/// Account-wide state, refreshed on `reconcile_interval`, not every quote
/// tick, this is the expensive multi-endpoint REST round trip.
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    pub cash: FixedX18,
    /// `market_id -> net signed size`. `FixedX18::ZERO` for markets with
    /// no position instead of absent from the map, callers shouldn't
    /// need to distinguish "no position" from "zero position", they mean
    /// the same thing here.
    pub positions: HashMap<u32, FixedX18>,
}

impl AccountState {
    pub fn position(&self, market_id: u32) -> FixedX18 {
        self.positions.get(&market_id).copied().unwrap_or(FixedX18::ZERO)
    }
}

/// Sliding-window count of orders this bot has placed, backing
/// `check_pre_trade`'s throttle limit. A `VecDeque` of placement
/// timestamps, pruned lazily on read instead of on a timer, nothing
/// polls this while the bot is otherwise idle.
#[derive(Default)]
pub struct OrderRateTracker {
    placements: std::collections::VecDeque<Instant>,
}

impl OrderRateTracker {
    pub fn record_placement(&mut self) {
        self.placements.push_back(Instant::now());
    }

    /// Drops anything at or past `window` old, returns how many remain.
    /// Uses `>=` (not `>`) so a zero-length window prunes immediately.
    pub fn count_in_window(&mut self, window: std::time::Duration) -> u32 {
        let now = Instant::now();
        while let Some(&oldest) = self.placements.front() {
            if now.duration_since(oldest) >= window {
                self.placements.pop_front();
            } else {
                break;
            }
        }
        self.placements.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_rate_tracker_zero_when_nothing_recorded() {
        let mut tracker = OrderRateTracker::default();
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(60)), 0);
    }

    #[test]
    fn order_rate_tracker_counts_placements_within_a_real_window() {
        let mut tracker = OrderRateTracker::default();
        tracker.record_placement();
        tracker.record_placement();
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(60)), 2);
    }

    #[test]
    fn order_rate_tracker_prunes_entries_at_or_past_the_window_edge() {
        let mut tracker = OrderRateTracker::default();
        tracker.record_placement();
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(0)), 0);
    }
}
