use std::collections::HashMap;

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
