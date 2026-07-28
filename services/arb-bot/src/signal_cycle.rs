//! One scan tick: detect signals, check cooldowns, pre-trade check, place
//! orders. Split from `main.rs` for the same reason as mm-bot's
//! `quote_cycle.rs`, this is the module with actual trading logic in it.

use std::collections::HashMap;
use std::time::Duration;

use arb_engine::{detect_cross_venue_signal, to_calendar_spread_trade, CrossVenueObservation};
use curve_engine::{Curve, CurvePoint, Zone};
use margin_sim::{MarginAccount, MarginEngine, MarginMode, MarketId, MarketState, OpenOrder as MarginOpenOrder, OrderSide as MarginSide, Position, SubaccountId, TokenId};
use oms_core::{Side, TimeInForce};
use risk_engine::{check_pre_trade, PreTradeLimits};
use rust_bridge::ExecutionClient;
use tick_math::{rate_to_tick, FixedX18, Rounding};

use crate::state::{AccountState, MarketRuntime, RestingLeg, SignalCooldowns};

#[derive(Debug, thiserror::Error)]
pub enum SignalCycleError {
    #[error(transparent)]
    Tick(#[from] tick_math::MathError),
    #[error(transparent)]
    Execution(#[from] rust_bridge::BridgeError),
}

fn build_zone(zone_name: &str, runtimes: &HashMap<u32, MarketRuntime>) -> Zone {
    Zone {
        name: zone_name.to_owned(),
        points: runtimes.values()
            .map(|r| CurvePoint {
                market_id: r.config.market_id,
                time_to_maturity_secs: r.market_state.time_to_maturity_secs,
                rate: r.market_state.mark_rate,
            })
            .collect(),
    }
}

fn find_market_for_maturity(runtimes: &HashMap<u32, MarketRuntime>, maturity_secs: u32) -> Option<u32> {
    runtimes.iter().find(|(_, r)| r.market_state.time_to_maturity_secs == maturity_secs).map(|(&id, _)| id)
}

fn build_margin_account(account: &AccountState, token_id: u32, runtimes: &HashMap<u32, MarketRuntime>) -> MarginAccount {
    MarginAccount {
        subaccount_id: SubaccountId::DEFAULT,
        token_id: TokenId(token_id),
        margin_mode: MarginMode::Cross,
        cash: account.cash,
        positions: runtimes.keys()
            .map(|&id| Position { market_id: MarketId(id), size: account.position(id) })
            .filter(|p| !p.size.is_zero())
            .collect(),
        open_orders: runtimes.values()
            .flat_map(|r| r.resting_legs.iter().map(move |leg| MarginOpenOrder {
                market_id: MarketId(r.config.market_id),
                side: match leg.side { Side::Long => MarginSide::Long, Side::Short => MarginSide::Short },
                size: FixedX18::from_f64(r.config.base_size),
                rate: leg.rate,
            }))
            .collect(),
        last_settled_at: 0,
    }
}

fn margin_side(s: Side) -> MarginSide {
    match s { Side::Long => MarginSide::Long, Side::Short => MarginSide::Short }
}

/// Check one hypothetical leg against a snapshot, return the leg's
/// intended (market_id, side, rate) if it passes, log and return `None`
/// if it doesn't. Doesn't place anything.
fn check_leg(
    market_id: u32,
    side: Side,
    size: f64,
    rate: FixedX18,
    margin_engine: &MarginEngine,
    account: &MarginAccount,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    recent_order_count: u32,
) -> bool {
    let hypothetical = MarginOpenOrder { market_id: MarketId(market_id), side: margin_side(side), size: FixedX18::from_f64(size), rate };
    match check_pre_trade(margin_engine, account, hypothetical, market_states, limits, recent_order_count) {
        Ok(()) => true,
        Err(violations) => {
            tracing::warn!(market_id, ?side, ?violations, "leg blocked by pre-trade check");
            false
        }
    }
}

async fn place_leg(
    market_id: u32,
    runtimes: &mut HashMap<u32, MarketRuntime>,
    execution: &mut ExecutionClient,
    side: Side,
    rate: FixedX18,
) -> Result<bool, SignalCycleError> {
    let (market_acc, tick_step, base_size) = {
        let r = runtimes.get(&market_id).expect("market_id came from this map's own keys");
        (r.config.market_acc.clone(), r.config.tick_step, r.config.base_size)
    };
    let rounding = match side { Side::Long => Rounding::Floor, Side::Short => Rounding::Ceil };
    let tick = rate_to_tick(rate, tick_step, rounding)?;

    let outcome = execution.place_order(market_acc, market_id, side, FixedX18::from_f64(base_size), Some(tick as i32), None, TimeInForce::Alo).await?;

    let runtime = runtimes.get_mut(&market_id).expect("market_id came from this map's own keys");
    match outcome.order_id {
        Some(order_id) => {
            runtime.resting_legs.push(RestingLeg { order_id, side, rate });
            Ok(true)
        }
        None => {
            tracing::warn!(market_id, ?side, "leg placement returned no order_id, ALO likely rejected as crossing");
            Ok(false)
        }
    }
}

/// Calendar spread scan: fit the zone's curve, find butterfly deviations,
/// and (cooldown + pre-trade permitting) enter all three legs. All-or-
/// nothing on the pre-trade check (checked sequentially before placing
/// anything), but NOT atomic on placement itself, if leg 2's `place_order`
/// call fails after leg 1 already went through, this leaves a real,
/// partial, unbalanced position sitting on-chain. Logged loudly when it
/// happens, not silently absorbed, but not automatically unwound either,
/// that's real scope this pass doesn't cover.
#[allow(clippy::too_many_arguments)]
pub async fn run_calendar_scan(
    runtimes: &mut HashMap<u32, MarketRuntime>,
    zone_name: &str,
    min_abs_deviation: f64,
    cooldowns: &mut SignalCooldowns,
    signal_cooldown: Duration,
    token_id: u32,
    account: &AccountState,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    recent_order_count: u32,
    execution: &mut ExecutionClient,
    kill_switch_tripped: bool,
) {
    if kill_switch_tripped {
        return; // no new entries; existing legs are left resting, not auto-unwound, see module doc
    }

    let zone = build_zone(zone_name, runtimes);
    let Ok(curve) = Curve::fit(&zone) else { return };
    let signals = curve.detect_butterflies(min_abs_deviation);

    let configs: HashMap<MarketId, margin_sim::MarginConfig> = runtimes.iter().map(|(&id, r)| (MarketId(id), r.margin_config)).collect();
    let margin_engine = MarginEngine::new(configs, market_states.clone());

    for signal in signals {
        let key = (signal.left_maturity_secs, signal.mid_maturity_secs, signal.right_maturity_secs);
        if !cooldowns.calendar_ready(key, signal_cooldown) {
            continue;
        }

        let (Some(left_id), Some(mid_id), Some(right_id)) = (
            find_market_for_maturity(runtimes, signal.left_maturity_secs),
            find_market_for_maturity(runtimes, signal.mid_maturity_secs),
            find_market_for_maturity(runtimes, signal.right_maturity_secs),
        ) else {
            tracing::warn!(?key, "butterfly signal maturities don't map back to a configured market, skipping");
            continue;
        };

        let trade = to_calendar_spread_trade(signal);
        let (mid_rate, left_rate, right_rate) = {
            let r = |id: u32| runtimes.get(&id).expect("looked up above").market_state.mark_rate;
            (r(mid_id), r(left_id), r(right_id))
        };
        let base_size = runtimes.get(&mid_id).expect("looked up above").config.base_size;

        let margin_account = build_margin_account(account, token_id, runtimes);
        let mid_ok = check_leg(mid_id, trade.mid_side, base_size, mid_rate, &margin_engine, &margin_account, market_states, limits, recent_order_count);
        let left_ok = mid_ok && check_leg(left_id, trade.wing_side, base_size, left_rate, &margin_engine, &margin_account, market_states, limits, recent_order_count);
        let right_ok = left_ok && check_leg(right_id, trade.wing_side, base_size, right_rate, &margin_engine, &margin_account, market_states, limits, recent_order_count);

        if !(mid_ok && left_ok && right_ok) {
            tracing::info!(?key, deviation = trade.signal.deviation, "calendar spread signal detected but blocked by pre-trade checks, skipping");
            continue;
        }

        tracing::info!(?key, deviation = trade.signal.deviation, mid_id, left_id, right_id, "calendar spread signal, entering all three legs");
        cooldowns.mark_calendar(key);

        let results = [
            place_leg(mid_id, runtimes, execution, trade.mid_side, mid_rate).await,
            place_leg(left_id, runtimes, execution, trade.wing_side, left_rate).await,
            place_leg(right_id, runtimes, execution, trade.wing_side, right_rate).await,
        ];
        for (leg_name, result) in [("mid", &results[0]), ("left", &results[1]), ("right", &results[2])] {
            if let Err(e) = result {
                tracing::error!(?key, leg_name, "leg placement failed, position may now be partial/unbalanced: {e}");
            }
        }
    }
}

/// Cross-venue scan: for each market with a configured `CexReference`,
/// compare Boros's mark rate against the latest funding rate observed
/// from that venue/symbol (`funding_rates`, populated by `main.rs` from
/// `feed-ingest`'s funding channel). Only places the Boros-side leg, the
/// CEX hedge is not this crate's job (see
/// `arb_engine::CrossVenueSignal`'s doc comment).
#[allow(clippy::too_many_arguments)]
pub async fn run_cross_venue_scan(
    runtimes: &mut HashMap<u32, MarketRuntime>,
    funding_rates: &HashMap<(feed_ingest::Venue, String), f64>,
    cooldowns: &mut SignalCooldowns,
    signal_cooldown: Duration,
    token_id: u32,
    account: &AccountState,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    recent_order_count: u32,
    execution: &mut ExecutionClient,
    kill_switch_tripped: bool,
) {
    if kill_switch_tripped {
        return;
    }

    let configs: HashMap<MarketId, margin_sim::MarginConfig> = runtimes.iter().map(|(&id, r)| (MarketId(id), r.margin_config)).collect();
    let margin_engine = MarginEngine::new(configs, market_states.clone());

    let market_ids: Vec<u32> = runtimes.keys().copied().collect();
    for market_id in market_ids {
        let (cex_ref, mark_rate, base_size) = {
            let r = runtimes.get(&market_id).expect("market_id came from this map's own keys");
            let Some(cex_ref) = r.config.cex_reference.clone() else { continue };
            (cex_ref, r.market_state.mark_rate, r.config.base_size)
        };

        let Some(&cex_apr) = funding_rates.get(&(cex_ref.venue, cex_ref.symbol.clone())) else {
            continue; // no funding data observed yet for this venue/symbol
        };

        if !cooldowns.cross_venue_ready(market_id, signal_cooldown) {
            continue;
        }

        let obs = CrossVenueObservation { boros_market_id: market_id, boros_implied_apr: mark_rate.to_f64(), cex_expected_funding_apr: cex_apr };
        let Some(signal) = detect_cross_venue_signal(&obs, cex_ref.min_abs_basis) else { continue };

        let margin_account = build_margin_account(account, token_id, runtimes);
        if !check_leg(market_id, signal.boros_side, base_size, mark_rate, &margin_engine, &margin_account, market_states, limits, recent_order_count) {
            tracing::info!(market_id, basis = signal.basis, "cross-venue signal detected but blocked by pre-trade check, skipping");
            continue;
        }

        tracing::info!(market_id, basis = signal.basis, ?signal.boros_side, "cross-venue signal, entering Boros leg (CEX hedge is NOT placed by this bot)");
        cooldowns.mark_cross_venue(market_id);

        if let Err(e) = place_leg(market_id, runtimes, execution, signal.boros_side, mark_rate).await {
            tracing::error!(market_id, "cross-venue leg placement failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_sim::MarginConfig;
    use oms_core::OrderId;

    fn runtime_fixture(market_id: u32, ttm_secs: u32, mark_rate: f64) -> MarketRuntime {
        MarketRuntime::new(
            crate::config::MarketConfig {
                market_id,
                market_acc: format!("0xacc{market_id}"),
                feed_tick_size: 0.01,
                tick_step: 1,
                base_size: 100.0,
                cex_reference: None,
            },
            MarginConfig {
                k_im: FixedX18::from_f64(0.1),
                k_mm: FixedX18::from_f64(0.05),
                k_i_thresh: FixedX18::from_f64(0.001),
                t_thresh: 86_400,
                token_id: TokenId(0),
            },
            MarketState { mark_rate: FixedX18::from_f64(mark_rate), time_to_maturity_secs: ttm_secs },
        )
    }

    #[test]
    fn margin_side_maps_long_and_short() {
        assert_eq!(margin_side(Side::Long), MarginSide::Long);
        assert_eq!(margin_side(Side::Short), MarginSide::Short);
    }

    #[test]
    fn build_zone_collects_one_point_per_runtime() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 30 * 86_400, 0.05));
        runtimes.insert(2, runtime_fixture(2, 60 * 86_400, 0.06));

        let zone = build_zone("btc-perp", &runtimes);
        assert_eq!(zone.name, "btc-perp");
        assert_eq!(zone.points.len(), 2);
        let market_ids: std::collections::HashSet<u32> = zone.points.iter().map(|p| p.market_id).collect();
        assert_eq!(market_ids, [1, 2].into_iter().collect());
    }

    #[test]
    fn build_zone_empty_when_no_runtimes() {
        let runtimes = HashMap::new();
        let zone = build_zone("empty-zone", &runtimes);
        assert!(zone.points.is_empty());
    }

    #[test]
    fn find_market_for_maturity_matches_exact_ttm() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 1000, 0.05));
        runtimes.insert(2, runtime_fixture(2, 2000, 0.06));

        assert_eq!(find_market_for_maturity(&runtimes, 2000), Some(2));
    }

    #[test]
    fn find_market_for_maturity_none_when_no_match() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 1000, 0.05));

        assert_eq!(find_market_for_maturity(&runtimes, 9999), None);
    }

    #[test]
    fn build_margin_account_only_includes_nonzero_positions() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 1000, 0.05));
        runtimes.insert(2, runtime_fixture(2, 2000, 0.06));

        let mut account = AccountState::default();
        account.positions.insert(1, FixedX18::from_f64(10.0));
        // market 2 left unset -> zero -> must be filtered out

        let margin_account = build_margin_account(&account, 7, &runtimes);
        assert_eq!(margin_account.positions.len(), 1);
        assert_eq!(margin_account.positions[0].market_id, MarketId(1));
        assert_eq!(margin_account.token_id, TokenId(7));
        assert!(margin_account.is_cross());
    }

    #[test]
    fn build_margin_account_carries_one_open_order_per_resting_leg() {
        let mut runtimes = HashMap::new();
        let mut runtime = runtime_fixture(1, 1000, 0.05);
        runtime.resting_legs.push(RestingLeg { order_id: OrderId::from_parts(Side::Long, 100, 0).unwrap(), side: Side::Long, rate: FixedX18::from_f64(0.04) });
        runtimes.insert(1, runtime);

        let account = AccountState::default();
        let margin_account = build_margin_account(&account, 0, &runtimes);
        assert_eq!(margin_account.open_orders.len(), 1);
        assert_eq!(margin_account.open_orders[0].side, MarginSide::Long);
        assert_eq!(margin_account.open_orders[0].market_id, MarketId(1));
    }
}
