use tick_math::FixedX18;

/// Calibration inputs for the Avellaneda-Stoikov reservation price + spread
/// model. No defaults anywhere in this crate, these are genuine trading
/// parameters a desk calibrates from its own risk appetite and observed
/// market microstructure, not something derivable from the protocol.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct AvellanedaStoikovParams {
    /// Risk aversion (γ). Higher = skews harder away from inventory, quotes
    /// wider. Must be > 0.
    pub gamma: f64,
    /// Rate volatility (σ) over the quoting horizon, same units as the
    /// rate itself (e.g. 0.02 for 2% annualized implied-APR vol). Must be
    /// >= 0.
    pub sigma: f64,
    /// Order arrival intensity decay (κ) from `λ(δ) = A·exp(-κδ)`, how
    /// fast fill probability drops as quotes move away from the
    /// reference rate. Must be > 0. Calibrated from historical fill data,
    /// not guessed.
    pub kappa: f64,
    /// Quoting/requoting horizon in seconds, **not** the instrument's
    /// time-to-maturity. This is how far ahead the MM is optimizing its
    /// inventory risk over (e.g. until the next recalibration), matching
    /// Avellaneda-Stoikov's `T-t`. Must be > 0.
    pub horizon_secs: u32,
    /// Weight on the carry-adjustment term (see `reservation::carry_adjustment`
    /// doc comment), 0.0 disables it entirely (pure classic A-S). Must be
    /// >= 0.
    pub carry_weight: f64,
}

impl AvellanedaStoikovParams {
    pub fn validate(&self) -> Result<(), crate::error::QuoteError> {
        if !(self.gamma > 0.0) {
            return Err(crate::error::QuoteError::InvalidParams("gamma must be > 0".into()));
        }
        if !(self.sigma >= 0.0) {
            return Err(crate::error::QuoteError::InvalidParams("sigma must be >= 0".into()));
        }
        if !(self.kappa > 0.0) {
            return Err(crate::error::QuoteError::InvalidParams("kappa must be > 0".into()));
        }
        if self.horizon_secs == 0 {
            return Err(crate::error::QuoteError::InvalidParams("horizon_secs must be > 0".into()));
        }
        if !(self.carry_weight >= 0.0) {
            return Err(crate::error::QuoteError::InvalidParams("carry_weight must be >= 0".into()));
        }
        Ok(())
    }
}

/// This account's current exposure in one market, in the units the
/// reservation-price model actually needs: DV01, not notional.
/// DV01 = |size| × 0.0001 × ttm_years, since PV = size × rate × ttm means
/// d(PV)/d(rate) = size × ttm.
#[derive(Debug, Clone, Copy, Default)]
pub struct InventoryState {
    /// Signed: positive = net long DV01 (benefits from rates rising),
    /// negative = net short DV01.
    pub net_dv01: f64,
    /// Size-weighted average fixed rate locked in across the account's
    /// current position in this market (the rate paid/received upfront at
    /// entry, see `oms-core::payment::calc_upfront_fixed_cost` and
    /// `PaymentLib.calcUpfrontFixedCost`, which is exactly what "locked in"
    /// refers to here). `None` if flat.
    pub avg_locked_fixed_rate: Option<f64>,
}

/// Maker order rate bounds. Fixed 2026-07-18: this used to be a symmetric
/// two-sided band (`mark * (1 ± const ± slope*ttm)`), which isn't what the
/// contract does at all. Real source: `MarginViewUtils.sol:222-239`
/// (`_calcRateBound`/`__calcRateBoundPositive`, `pendle-finance/boros-core-public`):
///
/// ```solidity
/// function __calcRateBoundPositive(int256 rMark, uint256 k_iThresh, Side side) private view returns (int256) {
///     if (rMark >= int256(k_iThresh)) {
///         int16 slope = side == Side.LONG ? loUpperSlopeBase1e4 : loLowerSlopeBase1e4;
///         return mulBase1e4(rMark, slope);
///     } else {
///         int16 constBase1e4 = side == Side.LONG ? loUpperConstBase1e4 : loLowerConstBase1e4;
///         return addBase1e18And1e4(rMark, constBase1e4);
///     }
/// }
/// ```
///
/// Three things the old version got wrong, not just the numbers: it's one
/// bound per side, not a two-sided band; it's slope OR const depending on
/// whether `|markRate| >= k_iThresh`, never both added together; and there's
/// no time-to-maturity term anywhere in it. `k_iThresh` is the same value
/// as `margin-sim::MarginConfig::k_i_thresh`, same protocol constant, two
/// crates that both need it, fetch it once and hand it to both.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct MakerRateBounds {
    pub lo_upper_slope_base1e4: i16,
    pub lo_upper_const_base1e4: i16,
    pub lo_lower_slope_base1e4: i16,
    pub lo_lower_const_base1e4: i16,
}

impl MakerRateBounds {
    /// The allowed-rate bound for one side. LONG orders must satisfy
    /// `rate <= bound`, SHORT orders `rate >= bound` (checked on-chain by
    /// `checkRateInBound`, not here, this just computes the same number).
    pub fn bound_for(&self, side: oms_core::Side, mark_rate: f64, k_i_thresh: f64) -> f64 {
        if mark_rate >= 0.0 {
            self.bound_positive(side, mark_rate, k_i_thresh)
        } else {
            -self.bound_positive(side.opposite(), -mark_rate, k_i_thresh)
        }
    }

    fn bound_positive(&self, side: oms_core::Side, mark_rate_abs: f64, k_i_thresh: f64) -> f64 {
        use oms_core::Side;
        if mark_rate_abs >= k_i_thresh {
            let slope = match side {
                Side::Long => self.lo_upper_slope_base1e4,
                Side::Short => self.lo_lower_slope_base1e4,
            };
            mark_rate_abs * (slope as f64 / 10_000.0)
        } else {
            let konst = match side {
                Side::Long => self.lo_upper_const_base1e4,
                Side::Short => self.lo_lower_const_base1e4,
            };
            mark_rate_abs + (konst as f64 / 10_000.0)
        }
    }
}

/// A computed bid/ask pair, still in continuous rate space, the caller
/// (execution-adapter/oms-core) converts to ticks via
/// `tick_math::rate_to_tick` with the correct rounding per side (Floor for
/// the bid so it never crosses further than intended, Ceil for the ask,
/// same convention already established in `tick-math::conversion`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub bid_rate: FixedX18,
    pub ask_rate: FixedX18,
    pub reservation_rate: FixedX18,
}
