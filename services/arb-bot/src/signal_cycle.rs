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

use crate::state::{AccountState, ActiveCalendarSpread, ActiveCrossVenue, MarketRuntime, OrderRateTracker, RestingLeg, SignalCooldowns};

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
    order_tracker: &mut OrderRateTracker,
) -> bool {
    let hypothetical = MarginOpenOrder { market_id: MarketId(market_id), side: margin_side(side), size: FixedX18::from_f64(size), rate };
    let recent_order_count = order_tracker.count_in_window(std::time::Duration::from_secs(limits.throttle_window_secs as u64));
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
    size: f64,
    rate: FixedX18,
) -> Result<bool, SignalCycleError> {
    let (market_acc, tick_step) = {
        let r = runtimes.get(&market_id).expect("market_id came from this map's own keys");
        (r.config.market_acc.clone(), r.config.tick_step)
    };
    let rounding = match side { Side::Long => Rounding::Floor, Side::Short => Rounding::Ceil };
    let tick = rate_to_tick(rate, tick_step, rounding)?;

    let outcome = execution.place_order(market_acc, market_id, side, FixedX18::from_f64(size), Some(tick as i32), None, TimeInForce::Alo).await?;

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

/// Cancels one already-placed leg's resting order, used to roll back a
/// calendar spread when a later leg fails to place. This only cancels
/// what's still resting on the book, an ALO leg that already filled
/// before the rollback runs is a real position and this does not touch
/// it, cancel_orders has nothing to cancel at that point. Best-effort,
/// not a guarantee: if the cancel itself fails, the leg is logged and
/// left for an operator, same as the rest of this module's failure mode.
async fn cancel_leg(market_id: u32, runtimes: &mut HashMap<u32, MarketRuntime>, execution: &mut ExecutionClient, order_id: oms_core::OrderId) {
    let market_acc = runtimes.get(&market_id).expect("market_id came from this map's own keys").config.market_acc.clone();
    match execution.cancel_orders(market_acc, market_id, false, vec![order_id]).await {
        Ok(_) => {
            if let Some(runtime) = runtimes.get_mut(&market_id) {
                runtime.resting_legs.retain(|leg| leg.order_id != order_id);
            }
        }
        Err(e) => tracing::error!(market_id, ?order_id, "rollback cancel failed, leg may still be resting, needs manual review: {e}"),
    }
}

/// Closes one leg of a reversed signal: opposite side of `entry_side`,
/// IOC (crosses the book now instead of resting), the exact `size` that
/// was entered on that leg, not the market's uniform config default,
/// calendar spread wings enter at their own DV01-neutral size (see
/// `arb_engine::dv01_neutral_wing_size`), closing at a different size
/// would leave a residual instead of flattening the position. This is a
/// best-effort reversal, not a guaranteed close: an IOC still needs
/// matching liquidity on the other side, a thin book can leave part of it
/// unfilled. Doesn't touch `resting_legs` (a filled reversal is a new
/// position, not a cancellation of the old order), the caller is expected
/// to drop the leg from its active-signal tracking regardless of fill
/// outcome, `reconcile.rs` picks up the real resulting position on its
/// own schedule.
async fn unwind_leg(market_id: u32, runtimes: &HashMap<u32, MarketRuntime>, execution: &mut ExecutionClient, entry_side: Side, size: f64, slippage: f64) {
    let market_acc = runtimes.get(&market_id).expect("market_id came from this map's own keys").config.market_acc.clone();
    let closing_side = entry_side.opposite();
    match execution.place_order(market_acc, market_id, closing_side, FixedX18::from_f64(size), None, Some(slippage), TimeInForce::Ioc).await {
        Ok(outcome) => tracing::info!(market_id, ?closing_side, filled = ?outcome.filled_size, "unwind order sent for reversed signal"),
        Err(e) => tracing::error!(market_id, ?closing_side, "unwind order failed, position may still be open, needs manual review: {e}"),
    }
}

/// Calendar spread scan: unwind any active spread whose signal has
/// reverted or reversed, then fit the zone's curve and (cooldown +
/// pre-trade permitting) enter all three legs of any new signal found.
/// All-or-nothing on the pre-trade check (checked sequentially before
/// placing anything). Placement itself still isn't atomic, these are
/// three separate transactions, but a failed leg now triggers a
/// best-effort rollback (`cancel_leg`) of whichever legs did place
/// instead of just logging and leaving them resting. That only covers
/// legs still on the book: an ALO leg that fills before the rollback
/// runs is a real position `cancel_leg` doesn't touch.
#[allow(clippy::too_many_arguments)]
pub async fn run_calendar_scan(
    runtimes: &mut HashMap<u32, MarketRuntime>,
    zone_name: &str,
    min_abs_deviation: f64,
    cooldowns: &mut SignalCooldowns,
    signal_cooldown: Duration,
    active_spreads: &mut HashMap<(u32, u32, u32), ActiveCalendarSpread>,
    unwind_slippage: f64,
    token_id: u32,
    account: &AccountState,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    order_tracker: &mut OrderRateTracker,
    execution: &mut ExecutionClient,
    kill_switch_tripped: bool,
) {
    if kill_switch_tripped {
        return; // no new entries; existing legs are left resting, not auto-unwound, see module doc
    }

    let zone = build_zone(zone_name, runtimes);
    let Ok(curve) = Curve::fit(&zone) else { return };
    let signals = curve.detect_butterflies(min_abs_deviation);

    // unwind pass: an active spread whose triple is no longer in the
    // current signal set (reverted below threshold) or whose sign flipped
    // gets closed before anything new is considered
    let current_signs: HashMap<(u32, u32, u32), bool> = signals.iter()
        .map(|s| ((s.left_maturity_secs, s.mid_maturity_secs, s.right_maturity_secs), s.deviation > 0.0))
        .collect();
    let reversed: Vec<(u32, u32, u32)> = active_spreads.iter()
        .filter(|(key, active)| current_signs.get(key) != Some(&active.entry_deviation_positive))
        .map(|(key, _)| *key)
        .collect();
    for key in reversed {
        let active = active_spreads.remove(&key).expect("key came from this map's own keys");
        tracing::info!(?key, "calendar spread signal reverted or flipped, unwinding all legs");
        for (market_id, side, size) in active.legs {
            unwind_leg(market_id, runtimes, execution, side, size, unwind_slippage).await;
        }
    }

    let configs: HashMap<MarketId, margin_sim::MarginConfig> = runtimes.iter().map(|(&id, r)| (MarketId(id), r.margin_config)).collect();
    let margin_engine = MarginEngine::new(configs, market_states.clone());

    for signal in signals {
        let key = (signal.left_maturity_secs, signal.mid_maturity_secs, signal.right_maturity_secs);
        if active_spreads.contains_key(&key) || !cooldowns.calendar_ready(key, signal_cooldown) {
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

        let entry_deviation_positive = signal.deviation > 0.0;
        let mid_size = runtimes.get(&mid_id).expect("looked up above").config.base_size;
        let trade = to_calendar_spread_trade(signal, mid_size);
        let (mid_rate, left_rate, right_rate) = {
            let r = |id: u32| runtimes.get(&id).expect("looked up above").market_state.mark_rate;
            (r(mid_id), r(left_id), r(right_id))
        };

        let margin_account = build_margin_account(account, token_id, runtimes);
        let mid_ok = check_leg(mid_id, trade.mid_side, trade.mid_size, mid_rate, &margin_engine, &margin_account, market_states, limits, order_tracker);
        let left_ok = mid_ok && check_leg(left_id, trade.wing_side, trade.left_size, left_rate, &margin_engine, &margin_account, market_states, limits, order_tracker);
        let right_ok = left_ok && check_leg(right_id, trade.wing_side, trade.right_size, right_rate, &margin_engine, &margin_account, market_states, limits, order_tracker);

        if !(mid_ok && left_ok && right_ok) {
            tracing::info!(?key, deviation = trade.signal.deviation, "calendar spread signal detected but blocked by pre-trade checks, skipping");
            continue;
        }

        tracing::info!(?key, deviation = trade.signal.deviation, mid_id, left_id, right_id, "calendar spread signal, entering all three legs");
        cooldowns.mark_calendar(key);

        let legs = [(mid_id, "mid", trade.mid_side, trade.mid_size), (left_id, "left", trade.wing_side, trade.left_size), (right_id, "right", trade.wing_side, trade.right_size)];
        let rates = [mid_rate, left_rate, right_rate];

        let mut placed: Vec<(u32, oms_core::OrderId)> = Vec::new();
        let mut entered_legs: Vec<(u32, Side, f64)> = Vec::new();
        let mut any_failed = false;

        for i in 0..3 {
            let (market_id, leg_name, side, size) = legs[i];
            match place_leg(market_id, runtimes, execution, side, size, rates[i]).await {
                Ok(true) => {
                    order_tracker.record_placement();
                    let order_id = runtimes.get(&market_id).expect("just placed above").resting_legs.last().expect("just pushed above").order_id;
                    placed.push((market_id, order_id));
                    entered_legs.push((market_id, side, size));
                }
                Ok(false) => {
                    any_failed = true;
                    tracing::warn!(?key, leg_name, market_id, "leg placement returned no order_id, ALO likely rejected as crossing");
                }
                Err(e) => {
                    any_failed = true;
                    tracing::error!(?key, leg_name, market_id, "leg placement failed: {e}");
                }
            }
        }

        if any_failed && !placed.is_empty() {
            tracing::warn!(?key, legs_to_roll_back = placed.len(), "calendar spread partially entered, rolling back the legs that did place");
            for (market_id, order_id) in placed {
                cancel_leg(market_id, runtimes, execution, order_id).await;
            }
        } else if !any_failed {
            active_spreads.insert(key, ActiveCalendarSpread { legs: entered_legs, entry_deviation_positive });
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
/// Cross-venue scan: unwind any active Boros leg whose basis has reverted
/// or flipped sign, then check each configured market's basis against its
/// `CexReference` and (cooldown + pre-trade permitting) enter the Boros
/// side. The CEX hedge leg is never placed by this bot, see the module
/// doc for why.
#[allow(clippy::too_many_arguments)]
pub async fn run_cross_venue_scan(
    runtimes: &mut HashMap<u32, MarketRuntime>,
    funding_rates: &HashMap<(feed_ingest::Venue, String), f64>,
    cooldowns: &mut SignalCooldowns,
    signal_cooldown: Duration,
    active_positions: &mut HashMap<u32, ActiveCrossVenue>,
    unwind_slippage: f64,
    token_id: u32,
    account: &AccountState,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    order_tracker: &mut OrderRateTracker,
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

        let obs = CrossVenueObservation { boros_market_id: market_id, boros_implied_apr: mark_rate.to_f64(), cex_expected_funding_apr: cex_apr };
        let current_signal = detect_cross_venue_signal(&obs, cex_ref.min_abs_basis);

        // unwind first: reverted below threshold (no signal at all) or
        // flipped sign relative to entry both count as "no longer valid"
        if let Some(active) = active_positions.get(&market_id) {
            let still_valid = current_signal.as_ref().is_some_and(|s| (s.basis > 0.0) == active.entry_basis_positive);
            if !still_valid {
                let active = active_positions.remove(&market_id).expect("checked above");
                tracing::info!(market_id, "cross-venue basis reverted or flipped, unwinding Boros leg");
                unwind_leg(market_id, runtimes, execution, active.side, base_size, unwind_slippage).await;
            }
        }

        let Some(signal) = current_signal else { continue };
        if active_positions.contains_key(&market_id) || !cooldowns.cross_venue_ready(market_id, signal_cooldown) {
            continue;
        }

        let margin_account = build_margin_account(account, token_id, runtimes);
        if !check_leg(market_id, signal.boros_side, base_size, mark_rate, &margin_engine, &margin_account, market_states, limits, order_tracker) {
            tracing::info!(market_id, basis = signal.basis, "cross-venue signal detected but blocked by pre-trade check, skipping");
            continue;
        }

        tracing::info!(market_id, basis = signal.basis, ?signal.boros_side, "cross-venue signal, entering Boros leg (CEX hedge is NOT placed by this bot)");
        cooldowns.mark_cross_venue(market_id);

        match place_leg(market_id, runtimes, execution, signal.boros_side, base_size, mark_rate).await {
            Ok(true) => {
                order_tracker.record_placement();
                active_positions.insert(market_id, ActiveCrossVenue { side: signal.boros_side, entry_basis_positive: signal.basis > 0.0 });
            }
            Ok(false) => tracing::warn!(market_id, "cross-venue leg placement returned no order_id, ALO likely rejected as crossing"),
            Err(e) => tracing::error!(market_id, "cross-venue leg placement failed: {e}"),
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
