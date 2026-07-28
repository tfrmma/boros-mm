//! Pre-trade limit checks: would placing this order push the account past
//! its DV01/notional/health-ratio limits? Reuses `margin_sim::MarginEngine`
//! directly for the health-ratio projection instead of re-deriving margin
//! math here, that's already source-verified in `margin-sim`.

use std::collections::HashMap;

use margin_sim::{MarginAccount, MarginEngine, MarketId, MarketState, OpenOrder};

use crate::{dv01::position_dv01, error::RiskError, types::PreTradeLimits, RiskViolation};

/// Checks every configured limit and returns **all** violations found, not
/// just the first: the caller (e.g. quoting-engine's requoting loop)
/// should see the full picture before deciding how to adjust.
///
/// `market_states` must cover every market the account (after adding
/// `hypothetical_order`) has a position or resting order in, same data
/// the caller already handed to `engine` via `update_market_state`, passed
/// again here instead of added as a new accessor on `MarginEngine` (this
/// crate stays a consumer of `margin-sim`'s public API, not a reason to
/// grow it).
pub fn check_pre_trade(
    engine: &MarginEngine,
    account: &MarginAccount,
    hypothetical_order: OpenOrder,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    recent_order_count_in_window: u32,
) -> Result<(), Vec<RiskViolation>> {
    let mut violations = Vec::new();

    let mut with_order = account.clone();
    with_order.open_orders.push(hypothetical_order);

    // DV01 aggregation across the account's positions. Resting orders
    // don't carry their own DV01 here, an order isn't a position until
    // filled; its risk contribution is already captured by margin-sim's
    // worst-case PM/IM netting, which check_pre_trade also checks below
    // via the health-ratio projection.
    let mut net_dv01 = 0.0;
    let mut gross_dv01 = 0.0;
    let mut notional = 0.0;
    for pos in &with_order.positions {
        if let Some(market) = market_states.get(&pos.market_id) {
            let dv01 = position_dv01(pos.size, market.time_to_maturity_secs);
            net_dv01 += dv01;
            gross_dv01 += dv01.abs();
            notional += pos.size.to_f64().abs();
        }
    }

    if net_dv01.abs() > limits.max_net_dv01 {
        violations.push(RiskViolation::NetDv01Exceeded { would_be: net_dv01.abs(), cap: limits.max_net_dv01 });
    }
    if gross_dv01 > limits.max_gross_dv01 {
        violations.push(RiskViolation::GrossDv01Exceeded { would_be: gross_dv01, cap: limits.max_gross_dv01 });
    }
    if notional > limits.max_notional {
        violations.push(RiskViolation::NotionalExceeded { would_be: notional, cap: limits.max_notional });
    }
    if recent_order_count_in_window >= limits.max_orders_per_window {
        violations.push(RiskViolation::OrderRateThrottled { count_in_window: recent_order_count_in_window, cap: limits.max_orders_per_window });
    }

    match engine.compute_account_state(&with_order) {
        Ok(state) => {
            if state.health_ratio < limits.min_projected_health_ratio {
                violations.push(RiskViolation::ProjectedHealthRatioTooLow {
                    projected: state.health_ratio,
                    floor: limits.min_projected_health_ratio,
                });
            }
        }
        Err(e) => {
            // a margin calculation error (e.g. unknown market) is itself
            // grounds to refuse the trade, surface it via the violations
            // list instead of silently skipping the health-ratio check
            tracing::warn!(error = %e, "pre-trade margin projection failed, treating as a violation, not skipping the check");
            let _ = RiskError::from(e);
            violations.push(RiskViolation::ProjectedHealthRatioTooLow { projected: f64::NEG_INFINITY, floor: limits.min_projected_health_ratio });
        }
    }

    if violations.is_empty() { Ok(()) } else { Err(violations) }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use margin_sim::{MarginConfig, MarginMode, OrderSide, Position, SubaccountId, TokenId};
    use tick_math::FixedX18;

    fn test_setup() -> (MarginEngine, HashMap<MarketId, MarketState>) {
        let mut configs = HashMap::new();
        configs.insert(MarketId(1), MarginConfig {
            k_im: FixedX18::from_f64(0.10), k_mm: FixedX18::from_f64(0.05),
            k_i_thresh: FixedX18::from_f64(0.001), t_thresh: 24 * 3600,
            token_id: TokenId(0),
        });
        let mut markets = HashMap::new();
        markets.insert(MarketId(1), MarketState { mark_rate: FixedX18::from_f64(0.08), time_to_maturity_secs: tick_math::SECONDS_PER_YEAR / 2 });
        (MarginEngine::new(configs, markets.clone()), markets)
    }

    fn empty_account() -> MarginAccount {
        MarginAccount {
            subaccount_id: SubaccountId::DEFAULT, token_id: TokenId(0), margin_mode: MarginMode::Cross,
            cash: FixedX18::from_f64(100_000.0), positions: vec![], open_orders: vec![], last_settled_at: 0,
        }
    }

    fn generous_limits() -> PreTradeLimits {
        PreTradeLimits { max_net_dv01: 1_000_000.0, max_gross_dv01: 1_000_000.0, max_notional: 1_000_000_000.0, min_projected_health_ratio: 1.0, max_orders_per_window: 1000, throttle_window_secs: 60 }
    }

    #[test]
    fn passes_when_within_all_limits() {
        let (engine, markets) = test_setup();
        let account = empty_account();
        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(100.0), rate: FixedX18::from_f64(0.08) };
        assert!(check_pre_trade(&engine, &account, order, &markets, &generous_limits(), 0).is_ok());
    }

    #[test]
    fn net_dv01_violation_detected() {
        let (engine, markets) = test_setup();
        let mut account = empty_account();
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(1_000_000.0) });
        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(1.0), rate: FixedX18::from_f64(0.08) };

        let tight_limits = PreTradeLimits { max_net_dv01: 1.0, ..generous_limits() };
        let result = check_pre_trade(&engine, &account, order, &markets, &tight_limits, 0);
        assert!(matches!(result, Err(v) if v.iter().any(|x| matches!(x, RiskViolation::NetDv01Exceeded { .. }))));
    }

    #[test]
    fn throttle_violation_detected() {
        let (engine, markets) = test_setup();
        let account = empty_account();
        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(1.0), rate: FixedX18::from_f64(0.08) };

        let throttled_limits = PreTradeLimits { max_orders_per_window: 5, ..generous_limits() };
        let result = check_pre_trade(&engine, &account, order, &markets, &throttled_limits, 5);
        assert!(matches!(result, Err(v) if v.iter().any(|x| matches!(x, RiskViolation::OrderRateThrottled { .. }))));
    }

    #[test]
    fn low_health_ratio_floor_detected() {
        let (engine, markets) = test_setup();
        let mut account = empty_account();
        account.cash = FixedX18::from_f64(1.0); // almost no collateral
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(100_000.0) });
        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(1.0), rate: FixedX18::from_f64(0.08) };

        let strict_limits = PreTradeLimits { min_projected_health_ratio: 1000.0, ..generous_limits() };
        let result = check_pre_trade(&engine, &account, order, &markets, &strict_limits, 0);
        assert!(matches!(result, Err(v) if v.iter().any(|x| matches!(x, RiskViolation::ProjectedHealthRatioTooLow { .. }))));
    }

    #[test]
    fn multiple_violations_all_reported() {
        let (engine, markets) = test_setup();
        let mut account = empty_account();
        account.cash = FixedX18::from_f64(1.0);
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(1_000_000.0) });
        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(1.0), rate: FixedX18::from_f64(0.08) };

        let strict_limits = PreTradeLimits { max_net_dv01: 0.01, min_projected_health_ratio: 1000.0, max_orders_per_window: 0, ..generous_limits() };
        let result = check_pre_trade(&engine, &account, order, &markets, &strict_limits, 10);
        let violations = result.unwrap_err();
        assert!(violations.len() >= 3, "expected multiple simultaneous violations, got {violations:?}");
    }
}
