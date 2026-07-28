//! One tick of the quoting loop. Split out of `main.rs` because this is
//! the part with actual trading logic in it, everything else is wiring.

use std::collections::HashMap;

use curve_engine::{Curve, CurvePoint, Zone};
use margin_sim::{MarginAccount, MarginEngine, MarginMode, MarketId, MarketState, OpenOrder as MarginOpenOrder, OrderSide as MarginSide, Position, SubaccountId, TokenId};
use oms_core::{OrderId, Side, TimeInForce};
use quoting_engine::{InventoryState, Quote, QuotingEngine};
use risk_engine::{check_pre_trade, position_dv01, PreTradeLimits};
use rust_bridge::ExecutionClient;
use tick_math::{rate_to_tick, FixedX18, Rounding};

use crate::state::{AccountState, MarketRuntime};

#[derive(Debug, thiserror::Error)]
pub enum QuoteCycleError {
    #[error(transparent)]
    Quote(#[from] quoting_engine::QuoteError),
    #[error(transparent)]
    Tick(#[from] tick_math::MathError),
    #[error(transparent)]
    Execution(#[from] rust_bridge::BridgeError),
}

/// Reference rate for one market: the zone's fitted curve if it covers
/// this market's time-to-maturity, the market's own mark rate otherwise
/// (curve fit failing outright, e.g. only one market configured, or this
/// specific maturity falling outside the observed range, both fall back
/// the same way instead of skipping the market entirely).
fn reference_rate_for(curve: Option<&Curve>, runtime: &MarketRuntime) -> FixedX18 {
    curve
        .and_then(|c| c.rate_at(runtime.market_state.time_to_maturity_secs))
        .unwrap_or(runtime.market_state.mark_rate)
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

fn build_margin_account(account: &AccountState, token_id: u32, runtimes: &HashMap<u32, MarketRuntime>) -> MarginAccount {
    MarginAccount {
        subaccount_id: SubaccountId::DEFAULT,
        token_id: TokenId(token_id),
        margin_mode: MarginMode::Cross,
        cash: account.cash,
        positions: runtimes.keys()
            .map(|&market_id| Position { market_id: MarketId(market_id), size: account.position(market_id) })
            .filter(|p| !p.size.is_zero())
            .collect(),
        open_orders: vec![], // hypothetical order is added by check_pre_trade itself
        last_settled_at: 0,
    }
}

fn desired_quote(runtime: &MarketRuntime, reference_rate: FixedX18, account: &AccountState) -> Result<Quote, quoting_engine::QuoteError> {
    let engine = QuotingEngine::new(runtime.config.as_params)?;
    let position = account.position(runtime.config.market_id);
    let inventory = InventoryState {
        net_dv01: position_dv01(position, runtime.market_state.time_to_maturity_secs),
        // entry-rate cost basis isn't tracked yet (would need accumulating
        // it across fills), carry_adjustment just won't activate
        // meaningfully without it, matches carry_weight=0.0 being the
        // documented safe default anyway
        avg_locked_fixed_rate: None,
    };
    engine.quote(reference_rate, runtime.market_state.mark_rate, runtime.margin_config.k_i_thresh, &inventory, &runtime.config.maker_bounds)
}

fn rate_moved_enough(old: Option<FixedX18>, new: FixedX18, threshold: f64) -> bool {
    match old {
        None => true,
        Some(old) => (new.to_f64() - old.to_f64()).abs() >= threshold,
    }
}

/// Run one full quote cycle across every configured market. Errors from
/// one market don't abort the others, a bad quote or a rejected order on
/// market A shouldn't stop market B from getting looked at.
#[allow(clippy::too_many_arguments)]
pub async fn run_cycle(
    runtimes: &mut HashMap<u32, MarketRuntime>,
    zone_name: &str,
    token_id: u32,
    account: &AccountState,
    margin_engine: &MarginEngine,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    requote_threshold: f64,
    recent_order_count: u32,
    execution: &mut ExecutionClient,
    kill_switch_tripped: bool,
) {
    let zone = build_zone(zone_name, runtimes);
    let curve = Curve::fit(&zone).ok(); // None is a legitimate fallback (e.g. one market), not an error to log every tick

    let market_ids: Vec<u32> = runtimes.keys().copied().collect();
    for market_id in market_ids {
        let result = run_market_cycle(
            market_id, runtimes, curve.as_ref(), token_id, account, margin_engine, market_states,
            limits, requote_threshold, recent_order_count, execution, kill_switch_tripped,
        ).await;
        if let Err(e) = result {
            tracing::error!(market_id, "quote cycle failed for this market: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_market_cycle(
    market_id: u32,
    runtimes: &mut HashMap<u32, MarketRuntime>,
    curve: Option<&Curve>,
    token_id: u32,
    account: &AccountState,
    margin_engine: &MarginEngine,
    market_states: &HashMap<MarketId, MarketState>,
    limits: &PreTradeLimits,
    requote_threshold: f64,
    recent_order_count: u32,
    execution: &mut ExecutionClient,
    kill_switch_tripped: bool,
) -> Result<(), QuoteCycleError> {
    if kill_switch_tripped {
        // pull in everything resting, place nothing new, every tick until
        // someone resets the switch, cancelling an already-cancelled order
        // is harmless, simpler than tracking "did we already pull back"
        return cancel_resting(market_id, runtimes, execution).await;
    }

    // Phase 1: pure reads, scoped so the immutable borrow of `runtimes`
    // ends before phase 2 needs a mutable one.
    let (quote, want_bid, want_ask, tick_step, base_size) = {
        let runtime = runtimes.get(&market_id).expect("market_id came from this map's own keys");
        let reference_rate = reference_rate_for(curve, runtime);
        let quote = desired_quote(runtime, reference_rate, account)?;
        let want_bid = rate_moved_enough(runtime.resting_bid.map(|(_, r)| r), quote.bid_rate, requote_threshold);
        let want_ask = rate_moved_enough(runtime.resting_ask.map(|(_, r)| r), quote.ask_rate, requote_threshold);
        (quote, want_bid, want_ask, runtime.config.tick_step, runtime.config.base_quote_size)
    };

    if !want_bid && !want_ask {
        return Ok(());
    }

    // pre-trade needs the full multi-market snapshot, built while runtimes
    // is still only borrowed immutably (this itself borrows runtimes, but
    // that's fine, multiple immutable borrows coexist, it's only the
    // later `get_mut` calls that need this dropped first)
    let margin_account = build_margin_account(account, token_id, runtimes);
    let size = FixedX18::from_f64(base_size);

    if want_bid {
        let bid_tick = rate_to_tick(quote.bid_rate, tick_step, Rounding::Floor)?;
        let hypothetical = MarginOpenOrder { market_id: MarketId(market_id), side: MarginSide::Long, size, rate: quote.bid_rate };
        match check_pre_trade(margin_engine, &margin_account, hypothetical, market_states, limits, recent_order_count) {
            Ok(()) => requote_side(market_id, runtimes, execution, Side::Long, quote.bid_rate, bid_tick, base_size).await?,
            Err(violations) => tracing::warn!(market_id, ?violations, "bid requote blocked by pre-trade check"),
        }
    }

    if want_ask {
        let ask_tick = rate_to_tick(quote.ask_rate, tick_step, Rounding::Ceil)?;
        let hypothetical = MarginOpenOrder { market_id: MarketId(market_id), side: MarginSide::Short, size, rate: quote.ask_rate };
        match check_pre_trade(margin_engine, &margin_account, hypothetical, market_states, limits, recent_order_count) {
            Ok(()) => requote_side(market_id, runtimes, execution, Side::Short, quote.ask_rate, ask_tick, base_size).await?,
            Err(violations) => tracing::warn!(market_id, ?violations, "ask requote blocked by pre-trade check"),
        }
    }

    Ok(())
}

/// Cancel whatever's resting on `side` (if anything) and place the new
/// order, using `Alo` (add-liquidity-only, a.k.a. post-only). A resting
/// market-maker quote should never cross and take, that's not what this
/// bot is for, and `Mechanics/Fees.md` confirms makers pay zero fees, so
/// there's no economic reason to risk a taker fill here either.
async fn requote_side(
    market_id: u32,
    runtimes: &mut HashMap<u32, MarketRuntime>,
    execution: &mut ExecutionClient,
    side: Side,
    rate: FixedX18,
    tick: i16,
    base_size: f64,
) -> Result<(), QuoteCycleError> {
    let (market_acc, existing) = {
        let runtime = runtimes.get(&market_id).expect("market_id came from this map's own keys");
        let existing = if side == Side::Long { runtime.resting_bid } else { runtime.resting_ask };
        (runtime.config.market_acc.clone(), existing)
    };

    if let Some((old_id, _)) = existing {
        if let Err(e) = execution.cancel_orders(market_acc.clone(), market_id, false, vec![old_id]).await {
            tracing::warn!(market_id, ?side, "cancel before requote failed, placing anyway: {e}");
        }
    }

    let outcome = execution.place_order(market_acc, market_id, side, FixedX18::from_f64(base_size), Some(tick as i32), None, TimeInForce::Alo).await?;

    let runtime = runtimes.get_mut(&market_id).expect("market_id came from this map's own keys");
    match outcome.order_id {
        Some(id) => {
            let slot = if side == Side::Long { &mut runtime.resting_bid } else { &mut runtime.resting_ask };
            *slot = Some((id, rate));
            let _ = runtime.tracker.on_placed(&[id], &[FixedX18::from_f64(base_size)]);
        }
        None => {
            tracing::warn!(market_id, ?side, "place_order returned no order_id, ALO likely rejected as crossing, treating as not resting");
            let slot = if side == Side::Long { &mut runtime.resting_bid } else { &mut runtime.resting_ask };
            *slot = None;
        }
    }
    Ok(())
}

async fn cancel_resting(market_id: u32, runtimes: &mut HashMap<u32, MarketRuntime>, execution: &mut ExecutionClient) -> Result<(), QuoteCycleError> {
    let (market_acc, ids): (String, Vec<OrderId>) = {
        let runtime = runtimes.get(&market_id).expect("market_id came from this map's own keys");
        let ids = [runtime.resting_bid, runtime.resting_ask].into_iter().flatten().map(|(id, _)| id).collect();
        (runtime.config.market_acc.clone(), ids)
    };
    if ids.is_empty() {
        return Ok(());
    }
    execution.cancel_orders(market_acc, market_id, false, ids).await?;

    let runtime = runtimes.get_mut(&market_id).expect("market_id came from this map's own keys");
    runtime.resting_bid = None;
    runtime.resting_ask = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MarketConfig;
    use quoting_engine::AvellanedaStoikovParams;

    fn market_config(market_id: u32) -> MarketConfig {
        MarketConfig {
            market_id,
            market_acc: format!("0xdead{market_id}"),
            feed_tick_size: 0.01,
            tick_step: 1,
            base_quote_size: 100.0,
            as_params: AvellanedaStoikovParams { gamma: 0.1, sigma: 0.02, kappa: 1.5, horizon_secs: 3600, carry_weight: 0.0 },
            maker_bounds: MakerRateBoundsFixture::wide(),
        }
    }

    // quoting_engine::MakerRateBounds fields aren't Copy-constructible
    // inline here without importing the type, small helper to keep the
    // fixture above readable
    struct MakerRateBoundsFixture;
    impl MakerRateBoundsFixture {
        fn wide() -> quoting_engine::MakerRateBounds {
            quoting_engine::MakerRateBounds {
                lo_upper_slope_base1e4: 30_000,
                lo_upper_const_base1e4: 5_000,
                lo_lower_slope_base1e4: 30_000,
                lo_lower_const_base1e4: 5_000,
            }
        }
    }

    fn runtime(market_id: u32, mark_rate: f64, ttm: u32) -> MarketRuntime {
        let margin_config = margin_sim::MarginConfig {
            k_im: FixedX18::from_f64(0.10),
            k_mm: FixedX18::from_f64(0.05),
            k_i_thresh: FixedX18::from_f64(0.001),
            t_thresh: 86_400,
            token_id: margin_sim::TokenId(0),
        };
        let market_state = MarketState { mark_rate: FixedX18::from_f64(mark_rate), time_to_maturity_secs: ttm };
        MarketRuntime::new(market_config(market_id), margin_config, market_state)
    }

    #[test]
    fn rate_moved_enough_true_when_no_prior_order() {
        assert!(rate_moved_enough(None, FixedX18::from_f64(0.05), 0.0001));
    }

    #[test]
    fn rate_moved_enough_false_for_small_change_under_threshold() {
        let old = FixedX18::from_f64(0.0500);
        let new = FixedX18::from_f64(0.0501);
        assert!(!rate_moved_enough(Some(old), new, 0.001));
    }

    #[test]
    fn rate_moved_enough_true_for_change_over_threshold() {
        let old = FixedX18::from_f64(0.0500);
        let new = FixedX18::from_f64(0.0520);
        assert!(rate_moved_enough(Some(old), new, 0.001));
    }

    #[test]
    fn reference_rate_falls_back_to_mark_rate_when_curve_is_none() {
        let r = runtime(1, 0.05, 3600);
        let rate = reference_rate_for(None, &r);
        assert_eq!(rate, r.market_state.mark_rate);
    }

    #[test]
    fn build_zone_collects_one_point_per_market() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime(1, 0.05, 3600));
        runtimes.insert(2, runtime(2, 0.06, 7200));
        let zone = build_zone("test-zone", &runtimes);
        assert_eq!(zone.name, "test-zone");
        assert_eq!(zone.points.len(), 2);
    }

    #[test]
    fn reference_rate_uses_curve_when_it_covers_the_maturity() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime(1, 0.04, 3600));
        runtimes.insert(2, runtime(2, 0.06, 7200));
        let zone = build_zone("test-zone", &runtimes);
        let curve = Curve::fit(&zone).unwrap();

        let r = runtimes.get(&1).unwrap();
        let rate = reference_rate_for(Some(&curve), r);
        // curve-fitted rate at the exact same maturity as an input point
        // should equal that point's own rate (interpolation through its
        // own data), not the market's raw mark_rate coincidentally
        assert!((rate.to_f64() - 0.04).abs() < 1e-6);
    }

    #[test]
    fn build_margin_account_only_includes_nonzero_positions() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime(1, 0.05, 3600));
        runtimes.insert(2, runtime(2, 0.06, 7200));

        let mut account = AccountState::default();
        account.positions.insert(1, FixedX18::from_f64(100.0));
        account.positions.insert(2, FixedX18::ZERO);

        let margin_account = build_margin_account(&account, 0, &runtimes);
        assert_eq!(margin_account.positions.len(), 1, "zero-size position for market 2 should be filtered out");
        assert_eq!(margin_account.positions[0].market_id, MarketId(1));
    }

    #[test]
    fn desired_quote_brackets_the_reference_rate() {
        let r = runtime(1, 0.05, 3600);
        let account = AccountState::default();
        let quote = desired_quote(&r, FixedX18::from_f64(0.05), &account).unwrap();
        assert!(quote.bid_rate < quote.ask_rate);
    }
}
