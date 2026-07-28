//! Upfront fixed-leg cost, paid at fill time, entirely separate from the
//! periodic floating-leg settlement in `settlement-ledger`. Source:
//! `contracts/lib/PaymentLib.sol:24-30` (`pendle-finance/boros-core-public`):
//! ```solidity
//! function calcUpfrontFixedCost(int256 cost, uint32 timeToMat) internal pure returns (int256) {
//!     return (cost * int256(uint256(timeToMat))).rawDivCeil(PMath.IONE_YEAR);
//! }
//! function toUpfrontFixedCost(Trade trade, uint32 timeToMat) internal pure returns (int256) {
//!     return calcUpfrontFixedCost(trade.signedCost(), timeToMat);
//! }
//! ```
//! `timeToMat` is raw **seconds** on-chain (`uint32`), never a pre-scaled
//! FixedX18 fraction, same as `margin-sim::Position::value()`. This
//! function takes seconds directly for the same reason: converting to a
//! FixedX18 fraction first and then multiplying is not the same
//! computation as one fused integer division, even with exact arithmetic
//! on both sides.

use tick_math::{mul_div_ceil, FixedX18, MathError, SECONDS_PER_YEAR};

/// `rawDivCeil(cost * timeToMat, IONE_YEAR)`, exact, matches the
/// contract's `mulCeil`-family rounding (protocol-favoring: the cost is
/// never rounded in the trader's favor).
pub fn calc_upfront_fixed_cost(cost: FixedX18, time_to_maturity_secs: u32) -> Result<FixedX18, MathError> {
    let divisor = SECONDS_PER_YEAR as i128; // IONE_YEAR is unscaled seconds, not 1e18-scaled, see module doc
    mul_div_ceil(cost.inner(), time_to_maturity_secs as i128, divisor).map(FixedX18::raw)
}

/// Mirrors `PaymentLib.toUpfrontFixedCost`: the upfront cost owed for one
/// `Trade`, using its own `signed_cost`.
pub fn trade_upfront_fixed_cost(trade: &crate::types::Trade, time_to_maturity_secs: u32) -> Result<FixedX18, MathError> {
    calc_upfront_fixed_cost(trade.signed_cost, time_to_maturity_secs)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_full_year_returns_cost_unchanged() {
        let cost = FixedX18::from_f64(100.0);
        let got = calc_upfront_fixed_cost(cost, SECONDS_PER_YEAR).unwrap();
        assert_eq!(got, cost);
    }

    #[test]
    fn half_year_halves_the_cost() {
        let cost = FixedX18::from_f64(100.0);
        let got = calc_upfront_fixed_cost(cost, SECONDS_PER_YEAR / 2).unwrap();
        let diff = (got.to_f64() - 50.0).abs();
        assert!(diff < 1e-6, "got {}", got.to_f64());
    }

    #[test]
    fn zero_time_to_maturity_is_zero_cost() {
        let cost = FixedX18::from_f64(100.0);
        assert_eq!(calc_upfront_fixed_cost(cost, 0).unwrap(), FixedX18::ZERO);
    }

    #[test]
    fn rounds_up_matching_ceil_semantics() {
        // raw(1) * 1 second, divided by a full year: rounds UP to 1 raw unit,
        // never truncates to 0, protocol-favoring, matches mulCeil-family
        let got = calc_upfront_fixed_cost(FixedX18::raw(1), 1).unwrap();
        assert_eq!(got, FixedX18::raw(1));
    }

    #[test]
    fn negative_cost_rounds_toward_zero_not_more_negative() {
        // ceiling on a negative value moves it TOWARD zero (less negative),
        // the opposite direction from floor, this is the case that would
        // silently break if floor were substituted for ceil here
        let cost = FixedX18::raw(-1);
        let got = calc_upfront_fixed_cost(cost, 1).unwrap();
        assert_eq!(got, FixedX18::ZERO, "ceil(-1 * 1 / YEAR) must round toward zero, got {got:?}");
    }
}
