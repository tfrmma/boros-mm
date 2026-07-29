use std::collections::HashMap;
use std::time::Instant;

use margin_sim::{MarginConfig, MarketState};
use oms_core::{OrderId, Side};
use tick_math::FixedX18;

use crate::config::MarketConfig;

/// One resting leg of an arb position (calendar spreads have up to three
/// simultaneous legs across different markets, cross-venue signals have
/// exactly one).
#[derive(Debug, Clone, Copy)]
pub struct RestingLeg {
    pub order_id: OrderId,
    pub side: Side,
    pub rate: FixedX18,
}

/// A calendar spread this bot entered and is still tracking for reversal.
/// `signal_cycle::run_calendar_scan` closes it (opposite-side IOC on each
/// leg) once the same maturity triple's deviation either drops back below
/// threshold or flips sign relative to `entry_deviation_positive`.
#[derive(Debug, Clone)]
pub struct ActiveCalendarSpread {
    pub legs: Vec<(u32, Side)>,
    pub entry_deviation_positive: bool,
}

/// Same idea as `ActiveCalendarSpread`, for a single cross-venue leg.
#[derive(Debug, Clone, Copy)]
pub struct ActiveCrossVenue {
    pub side: Side,
    pub entry_basis_positive: bool,
}

pub struct MarketRuntime {
    pub config: MarketConfig,
    pub margin_config: MarginConfig,
    pub market_state: MarketState,
    /// Rarely more than one at a time in this MVP (see main.rs's module
    /// doc, no automatic unwind means a leg mostly just sits here until
    /// an operator closes it), a `Vec` instead of a single `Option`
    /// because a market could theoretically be the mid of one butterfly
    /// AND a wing of another simultaneously, unlikely but not impossible.
    pub resting_legs: Vec<RestingLeg>,
}

impl MarketRuntime {
    pub fn new(config: MarketConfig, margin_config: MarginConfig, market_state: MarketState) -> Self {
        Self { config, margin_config, market_state, resting_legs: Vec::new() }
    }
}

/// Tracks when each signal was last acted on, so a basis that stays above
/// threshold for several scan ticks in a row doesn't get re-entered every
/// tick. Calendar spreads are keyed by the maturity triple (stable across
/// ticks even if which market_id currently occupies each maturity slot
/// were to change, unlikely but the maturity is the more meaningful key
/// for "is this the same spread opportunity"), cross-venue by market_id.
#[derive(Default)]
pub struct SignalCooldowns {
    calendar: HashMap<(u32, u32, u32), Instant>,
    cross_venue: HashMap<u32, Instant>,
}

impl SignalCooldowns {
    pub fn calendar_ready(&self, key: (u32, u32, u32), cooldown: std::time::Duration) -> bool {
        self.calendar.get(&key).is_none_or(|t| t.elapsed() >= cooldown)
    }

    pub fn mark_calendar(&mut self, key: (u32, u32, u32)) {
        self.calendar.insert(key, Instant::now());
    }

    pub fn cross_venue_ready(&self, market_id: u32, cooldown: std::time::Duration) -> bool {
        self.cross_venue.get(&market_id).is_none_or(|t| t.elapsed() >= cooldown)
    }

    pub fn mark_cross_venue(&mut self, market_id: u32) {
        self.cross_venue.insert(market_id, Instant::now());
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
    /// Uses `>=` (not `>`) so a zero-length window prunes immediately,
    /// same convention as `SignalCooldowns`.
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

#[derive(Debug, Clone, Default)]
pub struct AccountState {
    pub cash: FixedX18,
    pub positions: HashMap<u32, FixedX18>,
}

impl AccountState {
    pub fn position(&self, market_id: u32) -> FixedX18 {
        self.positions.get(&market_id).copied().unwrap_or(FixedX18::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_ready_true_when_never_marked() {
        let cooldowns = SignalCooldowns::default();
        assert!(cooldowns.calendar_ready((1, 2, 3), std::time::Duration::from_secs(60)));
    }

    #[test]
    fn calendar_not_ready_immediately_after_marking() {
        let mut cooldowns = SignalCooldowns::default();
        cooldowns.mark_calendar((1, 2, 3));
        assert!(!cooldowns.calendar_ready((1, 2, 3), std::time::Duration::from_secs(60)));
    }

    #[test]
    fn calendar_cooldown_keyed_by_maturity_triple_not_shared_across_keys() {
        let mut cooldowns = SignalCooldowns::default();
        cooldowns.mark_calendar((1, 2, 3));
        // different triple, same instant, must not inherit the other key's cooldown
        assert!(cooldowns.calendar_ready((4, 5, 6), std::time::Duration::from_secs(60)));
    }

    #[test]
    fn calendar_ready_again_once_cooldown_elapsed() {
        let mut cooldowns = SignalCooldowns::default();
        cooldowns.mark_calendar((1, 2, 3));
        // zero-length cooldown: elapsed() >= 0 is true essentially immediately
        assert!(cooldowns.calendar_ready((1, 2, 3), std::time::Duration::from_secs(0)));
    }

    #[test]
    fn cross_venue_ready_true_when_never_marked() {
        let cooldowns = SignalCooldowns::default();
        assert!(cooldowns.cross_venue_ready(42, std::time::Duration::from_secs(60)));
    }

    #[test]
    fn cross_venue_not_ready_immediately_after_marking() {
        let mut cooldowns = SignalCooldowns::default();
        cooldowns.mark_cross_venue(42);
        assert!(!cooldowns.cross_venue_ready(42, std::time::Duration::from_secs(60)));
    }

    #[test]
    fn cross_venue_cooldown_independent_of_calendar_cooldown() {
        let mut cooldowns = SignalCooldowns::default();
        cooldowns.mark_cross_venue(42);
        // calendar side keyed differently, marking cross_venue must not block it
        assert!(cooldowns.calendar_ready((42, 42, 42), std::time::Duration::from_secs(60)));
    }

    #[test]
    fn account_state_position_defaults_to_zero_for_unknown_market() {
        let account = AccountState::default();
        assert_eq!(account.position(999), FixedX18::ZERO);
    }

    #[test]
    fn account_state_position_returns_stored_size() {
        let mut account = AccountState::default();
        account.positions.insert(7, FixedX18::from_f64(12.5));
        assert_eq!(account.position(7), FixedX18::from_f64(12.5));
    }

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
        // zero-length window: any elapsed time is >= 0, so this entry is already stale
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(0)), 0);
    }

    #[test]
    fn order_rate_tracker_pruning_is_one_directional() {
        // once pruned, a placement doesn't come back on a later, wider window check
        let mut tracker = OrderRateTracker::default();
        tracker.record_placement();
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(0)), 0);
        assert_eq!(tracker.count_in_window(std::time::Duration::from_secs(3600)), 0);
    }
}
