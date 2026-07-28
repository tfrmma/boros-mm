use std::collections::HashMap;

use tick_math::{FixedX18, MathError, ONE_MUL_YEAR};
use tracing::warn;

use crate::{
    error::MarginError,
    types::{
        AccountMarginState, MarginAccount, MarginConfig, MarketId, MarketState,
        OpenOrder, OrderSide, LIQUIDATION_HEALTH_RATIO,
    },
};

// ── engine ────────────────────────────────────────────────────────────────────

/// Shadow replica of `MarginViewUtils`/`LiquidationViewUtils`
/// (`pendle-finance/boros-core-public`), source-verified formula by formula,
/// not approximated. Not the source of truth, the chain is. Use this for
/// pre-trade checks and monitoring.
pub struct MarginEngine {
    configs: HashMap<MarketId, MarginConfig>,
    market_states: HashMap<MarketId, MarketState>,
}

impl MarginEngine {
    pub fn new(configs: HashMap<MarketId, MarginConfig>, market_states: HashMap<MarketId, MarketState>) -> Self {
        Self { configs, market_states }
    }

    pub fn insert_config(&mut self, market_id: MarketId, config: MarginConfig) {
        self.configs.insert(market_id, config);
    }

    pub fn update_market_state(&mut self, market_id: MarketId, state: MarketState) {
        self.market_states.insert(market_id, state);
    }

    fn config(&self, market_id: MarketId) -> Result<&MarginConfig, MarginError> {
        self.configs.get(&market_id).ok_or(MarginError::UnknownMarket(market_id.0))
    }

    fn market(&self, market_id: MarketId) -> Result<&MarketState, MarginError> {
        self.market_states.get(&market_id).ok_or(MarginError::UnknownMarket(market_id.0))
    }

    // ── TTM utility ───────────────────────────────────────────────────────────

    /// Seconds remaining until maturity, saturating at zero for expired
    /// markets, matches the contract's raw `uint32 timeToMat` directly, no
    /// FixedX18 conversion (see `MarketState`'s doc comment for why an
    /// earlier version's pre-scaled year-fraction was wrong).
    pub fn seconds_to_maturity(maturity_ts: u64, now_ts: u64) -> u32 {
        maturity_ts.saturating_sub(now_ts).min(u32::MAX as u64) as u32
    }

    // ── main entry point ──────────────────────────────────────────────────────

    /// Compute full margin state for an account.
    ///
    /// Caller is responsible for:
    ///   - passing fresh mark rates / time-to-maturity via `update_market_state`
    ///   - calling settleAllAndGet before this if accrued payments matter
    pub fn compute_account_state(&self, account: &MarginAccount) -> Result<AccountMarginState, MarginError> {
        if account.is_cross() {
            self.check_cross_token_consistency(account)?;
        }

        let markets = self.markets_touched(account);

        let mut total_pv = FixedX18::ZERO;
        let mut total_im = FixedX18::ZERO;
        let mut total_mm = FixedX18::ZERO;

        for market_id in markets {
            let cfg = self.config(market_id)?;
            let market = self.market(market_id)?;

            let signed_size = account.positions.iter()
                .find(|p| p.market_id == market_id)
                .map(|p| p.size)
                .unwrap_or(FixedX18::ZERO);

            if let Some(pos) = account.positions.iter().find(|p| p.market_id == market_id) {
                total_pv = total_pv.checked_add(pos.value(market).map_err(MarginError::Math)?)
                    .ok_or(MarginError::Math(MathError::Overflow))?;
            }

            let (long_orders, short_orders): (Vec<_>, Vec<_>) = account.open_orders.iter()
                .filter(|o| o.market_id == market_id)
                .partition(|o| o.side == OrderSide::Long);

            let pm = calc_pm(signed_size, market.mark_rate, cfg.k_i_thresh, &long_orders, &short_orders)?;
            let im = calc_im_from_pm(pm, cfg, market)?;
            let mm = calc_mm(signed_size, market, cfg)?;

            total_im = total_im.checked_add(im).ok_or(MarginError::Math(MathError::Overflow))?;
            total_mm = total_mm.checked_add(mm).ok_or(MarginError::Math(MathError::Overflow))?;
        }

        let total_value = account.cash.checked_add(total_pv).ok_or(MarginError::Math(MathError::Overflow))?;
        let health_ratio = health_ratio_f64(total_value, total_mm);

        if health_ratio < LIQUIDATION_HEALTH_RATIO * 1.1 {
            // no protocol-defined "risky" constant exists (see types.rs),
            // this 1.1x-of-liquidation margin is purely a local monitoring
            // heuristic for this log line, not a margin/trading decision
            warn!(health_ratio, total_value = total_value.to_f64(), total_mm = total_mm.to_f64(), "account approaching liquidation health ratio");
        }

        Ok(AccountMarginState {
            total_value,
            total_im,
            total_mm,
            health_ratio,
            is_liquidatable: health_ratio < LIQUIDATION_HEALTH_RATIO,
        })
    }

    /// Margin headroom if a new order were added. Negative = don't place it.
    pub fn margin_headroom_for_order(&self, account: &MarginAccount, order: &OpenOrder) -> Result<FixedX18, MarginError> {
        let mut with_order = account.clone();
        with_order.open_orders.push(*order);

        let state = self.compute_account_state(&with_order)?;
        state.total_value.checked_sub(state.total_im).ok_or(MarginError::Math(MathError::Overflow))
    }

    fn markets_touched(&self, account: &MarginAccount) -> Vec<MarketId> {
        let mut ids: Vec<MarketId> = account.positions.iter().map(|p| p.market_id)
            .chain(account.open_orders.iter().map(|o| o.market_id))
            .collect();
        ids.sort_by_key(|m| m.0);
        ids.dedup_by_key(|m| m.0);
        ids
    }

    fn check_cross_token_consistency(&self, account: &MarginAccount) -> Result<(), MarginError> {
        for market_id in self.markets_touched(account) {
            let cfg = self.config(market_id)?;
            if cfg.token_id != account.token_id {
                return Err(MarginError::TokenMismatch {
                    market_id,
                    market_token: cfg.token_id,
                    account_token: account.token_id,
                });
            }
        }
        Ok(())
    }
}

// ── free functions: exact ports of MarginViewUtils.sol ────────────────────────

/// `__calcPMFromRate`: `absSize.mulDown(absRate.max(k_iThresh))`.
fn calc_pm_from_rate(abs_size: FixedX18, rate: FixedX18, k_i_thresh: FixedX18) -> Result<FixedX18, MarginError> {
    let effective_rate = rate.abs().max(k_i_thresh);
    let raw = tick_math::mul_div_down(abs_size.inner().unsigned_abs(), effective_rate.inner().unsigned_abs(), FixedX18::SCALE as u128)
        .map_err(MarginError::Math)?;
    Ok(FixedX18::raw(i128::try_from(raw).map_err(|_| MarginError::Math(MathError::Overflow))?))
}

fn sum_orders_pm(orders: &[&OpenOrder], k_i_thresh: FixedX18) -> Result<(FixedX18, FixedX18), MarginError> {
    let mut sum_size = FixedX18::ZERO;
    let mut sum_pm = FixedX18::ZERO;
    for o in orders {
        sum_size += o.size;
        sum_pm += calc_pm_from_rate(o.size, o.rate, k_i_thresh)?;
    }
    Ok((sum_size, sum_pm))
}

/// `_calcPM`: worst-case position margin, netting the position against
/// resting orders on each side. Source: `MarginViewUtils.sol:64-87`.
fn calc_pm(
    signed_size: FixedX18,
    mark_rate: FixedX18,
    k_i_thresh: FixedX18,
    long_orders: &[&OpenOrder],
    short_orders: &[&OpenOrder],
) -> Result<FixedX18, MarginError> {
    let pm_alone = calc_pm_from_rate(signed_size.abs(), mark_rate, k_i_thresh)?;

    let (b_long, pm_long) = sum_orders_pm(long_orders, k_i_thresh)?;
    let pm_total_long = if signed_size.is_negative() && b_long <= signed_size.abs() {
        FixedX18::ZERO
    } else if signed_size.is_positive() {
        pm_long + pm_alone
    } else if pm_long > pm_alone {
        pm_long - pm_alone
    } else {
        FixedX18::ZERO
    };

    let (b_short, pm_short) = sum_orders_pm(short_orders, k_i_thresh)?;
    let pm_total_short = if signed_size.is_positive() && b_short <= signed_size.abs() {
        FixedX18::ZERO
    } else if signed_size.is_negative() {
        pm_short + pm_alone
    } else if pm_short > pm_alone {
        pm_short - pm_alone
    } else {
        FixedX18::ZERO
    };

    Ok(pm_total_long.max(pm_total_short))
}

/// `_calcIM` (given a precomputed PM): `(PM * kIM * max(timeToMat,tThresh)).rawDivUp(ONE_MUL_YEAR)`.
/// Source: `MarginViewUtils.sol:36-39`.
fn calc_im_from_pm(pm: FixedX18, cfg: &MarginConfig, market: &MarketState) -> Result<FixedX18, MarginError> {
    let t = market.time_to_maturity_secs.max(cfg.t_thresh);
    let raw = tick_math::mul3_div_up_u32(pm.inner().unsigned_abs(), cfg.k_im.inner().unsigned_abs(), t, ONE_MUL_YEAR as u128)
        .map_err(MarginError::Math)?;
    Ok(FixedX18::raw(i128::try_from(raw).map_err(|_| MarginError::Math(MathError::Overflow))?))
}

/// `_calcMM`. Source: `MarginViewUtils.sol:42-58`.
fn calc_mm(signed_size: FixedX18, market: &MarketState, cfg: &MarginConfig) -> Result<FixedX18, MarginError> {
    let abs_size = signed_size.abs();
    let abs_mark_rate = market.mark_rate.abs();
    let t = market.time_to_maturity_secs;

    if abs_mark_rate > cfg.k_i_thresh {
        let scaled_t_thresh_raw = tick_math::mul_div_up(cfg.t_thresh as u128, cfg.k_mm.inner().unsigned_abs(), FixedX18::SCALE as u128)
            .map_err(MarginError::Math)?;
        let scaled_t_thresh = u32::try_from(scaled_t_thresh_raw).unwrap_or(u32::MAX);

        let same_sign = !signed_size.is_zero() && !market.mark_rate.is_zero()
            && (signed_size.is_positive() == market.mark_rate.is_positive());

        if t < scaled_t_thresh && same_sign {
            // absSize * (absMarkRate*t + k_iThresh*(scaledTThresh - t)), rawDivUp(ONE_MUL_YEAR)
            let term1 = abs_mark_rate.inner().checked_mul(t as i128).ok_or(MarginError::Math(MathError::Overflow))?;
            let remaining = (scaled_t_thresh - t) as i128;
            let term2 = cfg.k_i_thresh.inner().checked_mul(remaining).ok_or(MarginError::Math(MathError::Overflow))?;
            let sum = term1.checked_add(term2).ok_or(MarginError::Math(MathError::Overflow))?;

            let raw = tick_math::mul_div_up(abs_size.inner().unsigned_abs(), sum.unsigned_abs(), ONE_MUL_YEAR as u128)
                .map_err(MarginError::Math)?;
            return Ok(FixedX18::raw(i128::try_from(raw).map_err(|_| MarginError::Math(MathError::Overflow))?));
        }
        // else fall through to the shared calculation below
    }

    let pm = calc_pm_from_rate(abs_size, market.mark_rate, cfg.k_i_thresh)?;
    let t_eff = t.max(cfg.t_thresh);
    let raw = tick_math::mul3_div_up_u32(pm.inner().unsigned_abs(), cfg.k_mm.inner().unsigned_abs(), t_eff, ONE_MUL_YEAR as u128)
        .map_err(MarginError::Math)?;
    Ok(FixedX18::raw(i128::try_from(raw).map_err(|_| MarginError::Math(MathError::Overflow))?))
}

fn health_ratio_f64(total_value: FixedX18, total_mm: FixedX18) -> f64 {
    if total_mm.is_zero() {
        return f64::INFINITY; // no positions, no requirement
    }
    total_value.to_f64() / total_mm.to_f64()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MarginMode, Position, SubaccountId, TokenId};

    fn test_config() -> MarginConfig {
        MarginConfig {
            k_im: FixedX18::from_f64(0.10),
            k_mm: FixedX18::from_f64(0.05),
            k_i_thresh: FixedX18::from_f64(0.001), // 10 bps floor
            t_thresh: 24 * 3600,                    // 1-day floor
            token_id: TokenId(0),
        }
    }

    fn test_market(mark_rate: f64, ttm_secs: u32) -> MarketState {
        MarketState { mark_rate: FixedX18::from_f64(mark_rate), time_to_maturity_secs: ttm_secs }
    }

    fn test_engine(mark_rate: f64, ttm_secs: u32) -> MarginEngine {
        let mut configs = HashMap::new();
        configs.insert(MarketId(1), test_config());
        let mut markets = HashMap::new();
        markets.insert(MarketId(1), test_market(mark_rate, ttm_secs));
        MarginEngine::new(configs, markets)
    }

    fn empty_account() -> MarginAccount {
        MarginAccount {
            subaccount_id: SubaccountId::DEFAULT,
            token_id: TokenId(0),
            margin_mode: MarginMode::Cross,
            cash: FixedX18::from_f64(10_000.0),
            positions: vec![],
            open_orders: vec![],
            last_settled_at: 0,
        }
    }

    const HALF_YEAR_SECS: u32 = tick_math::SECONDS_PER_YEAR / 2;

    #[test]
    fn no_positions_is_infinite_health() {
        let engine = test_engine(0.08, HALF_YEAR_SECS);
        let account = empty_account();
        let state = engine.compute_account_state(&account).unwrap();
        assert_eq!(state.health_ratio, f64::INFINITY);
        assert!(!state.is_liquidatable);
    }

    #[test]
    fn long_position_positive_pv() {
        let engine = test_engine(0.08, HALF_YEAR_SECS);
        let mut account = empty_account();
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(1000.0) });

        let state = engine.compute_account_state(&account).unwrap();

        // PV = 1000 * 0.08 * 0.5 = 40 units
        let pv_got = (state.total_value - account.cash).to_f64();
        assert!((pv_got - 40.0).abs() < 0.001, "PV expected 40.0, got {pv_got:.4}");

        // MM (shared branch, since |0.08| is not > k_i_thresh=0.001... wait it IS >
        // so check the time-gated branch condition: t=0.5y >> scaledTThresh (~1 day * 0.05 ~ tiny),
        // so falls to the shared branch: PM=1000*max(0.08,0.001)=80, MM=ceil(80*0.05*0.5)=2.0
        let mm_got = state.total_mm.to_f64();
        assert!((mm_got - 2.0).abs() < 0.001, "MM expected ~2.0, got {mm_got:.4}");

        assert!(state.health_ratio > 100.0);
    }

    #[test]
    fn short_position_negative_pv_same_mm_as_long() {
        let engine = test_engine(0.08, HALF_YEAR_SECS);

        let mut short_account = empty_account();
        short_account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(-1000.0) });
        let short_state = engine.compute_account_state(&short_account).unwrap();

        let pv = (short_state.total_value - short_account.cash).to_f64();
        assert!(pv < 0.0, "short PV should be negative");

        let mut long_account = empty_account();
        long_account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(1000.0) });
        let long_state = engine.compute_account_state(&long_account).unwrap();

        assert!((short_state.total_mm.to_f64() - long_state.total_mm.to_f64()).abs() < 1e-9);
    }

    #[test]
    fn rate_floor_binds_for_low_rate() {
        let engine = test_engine(0.0005, HALF_YEAR_SECS); // below the 10bps floor
        let mut account = empty_account();
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(1000.0) });

        let state = engine.compute_account_state(&account).unwrap();

        // mark_rate (0.0005) is NOT > k_i_thresh (0.001), so the time-gated
        // branch's outer condition is false -> shared branch directly.
        // PM = 1000 * max(0.0005, 0.001) = 1.0 ; MM = ceil(1.0*0.05*0.5) = 0.025
        let expected_mm = 1000.0 * 0.001 * 0.05 * 0.5;
        assert!((state.total_mm.to_f64() - expected_mm).abs() < 1e-6);
    }

    #[test]
    fn open_order_increases_im_and_headroom_is_positive() {
        let engine = test_engine(0.08, HALF_YEAR_SECS);
        let account = empty_account();
        let state_no_order = engine.compute_account_state(&account).unwrap();

        let order = OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(500.0), rate: FixedX18::from_f64(0.08) };

        let headroom = engine.margin_headroom_for_order(&account, &order).unwrap();
        assert!(headroom.is_positive(), "should have headroom: {}", headroom.to_f64());

        let mut with_order = account.clone();
        with_order.open_orders.push(order);
        let state_with_order = engine.compute_account_state(&with_order).unwrap();

        assert!(state_with_order.total_im > state_no_order.total_im);
    }

    #[test]
    fn closing_orders_net_against_position_instead_of_adding_im() {
        // short 1000, with a LONG order that only partially closes it (500),
        // per _calcPM, long orders when S<0 subtract from pmAlone instead of
        // adding on top, since they're closing exposure not adding it
        let engine = test_engine(0.08, HALF_YEAR_SECS);

        let mut short_only = empty_account();
        short_only.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(-1000.0) });
        let im_short_only = engine.compute_account_state(&short_only).unwrap().total_im;

        let mut short_with_closing_order = short_only.clone();
        short_with_closing_order.open_orders.push(OpenOrder {
            market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(500.0), rate: FixedX18::from_f64(0.08),
        });
        let im_with_closing_order = engine.compute_account_state(&short_with_closing_order).unwrap().total_im;

        // a naively-additive (un-netted) implementation would show IM going UP
        // when adding a closing order. The real formula must not increase it
        // beyond what full closure would imply, assert it does NOT simply add
        // the order's own stand-alone IM on top (the old, wrong behavior).
        let naive_additive_im = im_short_only + {
            let mut solo = empty_account();
            solo.open_orders.push(OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(500.0), rate: FixedX18::from_f64(0.08) });
            engine.compute_account_state(&solo).unwrap().total_im
        };
        assert!(im_with_closing_order < naive_additive_im, "closing order must net, not add IM on top: got {} vs naive {}", im_with_closing_order.to_f64(), naive_additive_im.to_f64());
    }

    #[test]
    fn fully_closing_orders_add_zero_extra_pm() {
        // short 1000, LONG orders totaling exactly 1000 (fully closing) or more
        // must not add any margin risk on the long side at all (pmTotalLong=0
        // path: S<0 && B_Long<=|S|)
        let engine = test_engine(0.08, HALF_YEAR_SECS);
        let mut account = empty_account();
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(-1000.0) });
        account.open_orders.push(OpenOrder { market_id: MarketId(1), side: OrderSide::Long, size: FixedX18::from_f64(1000.0), rate: FixedX18::from_f64(0.08) });

        let state = engine.compute_account_state(&account).unwrap();
        let short_only_im = {
            let mut a = empty_account();
            a.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(-1000.0) });
            engine.compute_account_state(&a).unwrap().total_im
        };

        assert_eq!(state.total_im, short_only_im, "fully-closing order must not change IM at all");
    }

    #[test]
    fn liquidation_threshold_is_exactly_one() {
        assert_eq!(LIQUIDATION_HEALTH_RATIO, 1.0);
    }

    #[test]
    fn cross_account_touching_a_different_token_market_is_rejected() {
        // two markets, two different collateral tokens, account is cross
        // margined in token 0 but has a position in the token-1 market
        let mut configs = HashMap::new();
        configs.insert(MarketId(1), test_config());
        configs.insert(MarketId(2), MarginConfig { token_id: TokenId(1), ..test_config() });
        let mut markets = HashMap::new();
        markets.insert(MarketId(1), test_market(0.08, HALF_YEAR_SECS));
        markets.insert(MarketId(2), test_market(0.08, HALF_YEAR_SECS));
        let engine = MarginEngine::new(configs, markets);

        let mut account = empty_account(); // token_id: TokenId(0)
        account.positions.push(Position { market_id: MarketId(2), size: FixedX18::from_f64(100.0) });

        let err = engine.compute_account_state(&account).unwrap_err();
        assert!(matches!(err, MarginError::TokenMismatch { market_id: MarketId(2), .. }), "expected TokenMismatch for market 2, got {err:?}");
    }

    #[test]
    fn cross_account_across_matching_token_markets_is_fine() {
        let mut configs = HashMap::new();
        configs.insert(MarketId(1), test_config());
        configs.insert(MarketId(2), test_config()); // same TokenId(0) as the account
        let mut markets = HashMap::new();
        markets.insert(MarketId(1), test_market(0.08, HALF_YEAR_SECS));
        markets.insert(MarketId(2), test_market(0.08, HALF_YEAR_SECS));
        let engine = MarginEngine::new(configs, markets);

        let mut account = empty_account();
        account.positions.push(Position { market_id: MarketId(1), size: FixedX18::from_f64(100.0) });
        account.positions.push(Position { market_id: MarketId(2), size: FixedX18::from_f64(-50.0) });

        assert!(engine.compute_account_state(&account).is_ok());
    }

    #[test]
    fn isolated_account_skips_the_token_check_entirely() {
        // isolated mode never nets across markets, so a token mismatch here
        // isn't even a meaningful question, compute_account_state only calls
        // check_cross_token_consistency when account.is_cross()
        let mut configs = HashMap::new();
        configs.insert(MarketId(2), MarginConfig { token_id: TokenId(1), ..test_config() });
        let mut markets = HashMap::new();
        markets.insert(MarketId(2), test_market(0.08, HALF_YEAR_SECS));
        let engine = MarginEngine::new(configs, markets);

        let mut account = empty_account(); // token_id: TokenId(0), but isolated
        account.margin_mode = MarginMode::Isolated { market_id: MarketId(2) };
        account.positions.push(Position { market_id: MarketId(2), size: FixedX18::from_f64(100.0) });

        assert!(engine.compute_account_state(&account).is_ok());
    }

    #[test]
    fn seconds_to_maturity_basic() {
        let now = 1_700_000_000u64;
        let maturity = now + 365 * 24 * 3600;
        assert_eq!(MarginEngine::seconds_to_maturity(maturity, now), 365 * 24 * 3600);
    }

    #[test]
    fn seconds_to_maturity_saturates_at_zero() {
        let now = 1_700_000_000u64;
        let maturity = now - 3600; // already expired
        assert_eq!(MarginEngine::seconds_to_maturity(maturity, now), 0);
    }
}
