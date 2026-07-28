use oms_core::Side;

use crate::types::MakerRateBounds;

/// This crate's convention: bid = LONG, ask = SHORT. Matches the resting
/// order priority tests over in oms-core (`long_priority_favors_higher_tick`
/// calls LONG orders "bids" in its own comment, `short_priority_favors_lower_tick`
/// calls SHORT "asks").
///
/// A bid only ever gets an upper cap and an ask only ever gets a lower
/// cap, because that's literally all `checkRateInBound` checks on-chain:
/// LONG needs `rate <= bound`, SHORT needs `rate >= bound`. There's no
/// matching floor on the bid or ceiling on the ask from this particular
/// check; if you want one of those for other reasons (sanity, inventory)
/// that's a separate concern from this function.
pub fn clamp_bid(raw_bid: f64, bounds: &MakerRateBounds, mark_rate: f64, k_i_thresh: f64) -> f64 {
    raw_bid.min(bounds.bound_for(Side::Long, mark_rate, k_i_thresh))
}

pub fn clamp_ask(raw_ask: f64, bounds: &MakerRateBounds, mark_rate: f64, k_i_thresh: f64) -> f64 {
    raw_ask.max(bounds.bound_for(Side::Short, mark_rate, k_i_thresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> MakerRateBounds {
        MakerRateBounds {
            lo_upper_slope_base1e4: 15_000, // 1.5x when |mark| >= threshold
            lo_upper_const_base1e4: 100,    // +1% when |mark| < threshold
            lo_lower_slope_base1e4: 5_000,  // 0.5x when |mark| >= threshold
            lo_lower_const_base1e4: -100,   // -1% when |mark| < threshold
        }
    }

    #[test]
    fn bid_within_bound_unchanged() {
        // mark=0.08 >= threshold=0.01 -> long bound = 0.08 * 1.5 = 0.12
        let clamped = clamp_bid(0.10, &bounds(), 0.08, 0.01);
        assert!((clamped - 0.10).abs() < 1e-12);
    }

    #[test]
    fn bid_above_bound_gets_capped() {
        let clamped = clamp_bid(0.50, &bounds(), 0.08, 0.01);
        assert!((clamped - 0.12).abs() < 1e-9, "expected cap at 0.12, got {clamped}");
    }

    #[test]
    fn bid_has_no_floor_from_this_check() {
        // nothing in _calcRateBound stops a long from bidding arbitrarily low
        let clamped = clamp_bid(-10.0, &bounds(), 0.08, 0.01);
        assert!((clamped - (-10.0)).abs() < 1e-12);
    }

    #[test]
    fn ask_within_bound_unchanged() {
        // mark=0.08 >= threshold=0.01 -> short bound = 0.08 * 0.5 = 0.04
        let clamped = clamp_ask(0.06, &bounds(), 0.08, 0.01);
        assert!((clamped - 0.06).abs() < 1e-12);
    }

    #[test]
    fn ask_below_bound_gets_floored() {
        let clamped = clamp_ask(0.01, &bounds(), 0.08, 0.01);
        assert!((clamped - 0.04).abs() < 1e-9, "expected floor at 0.04, got {clamped}");
    }

    #[test]
    fn near_zero_mark_rate_uses_const_branch_not_slope() {
        // mark=0.005 < threshold=0.01 -> long bound = 0.005 + 0.01 = 0.015,
        // not 0.005 * 1.5 = 0.0075. If a future edit collapses the two
        // branches back into one, this is the test that catches it.
        let b = bounds();
        let long_bound = b.bound_for(Side::Long, 0.005, 0.01);
        assert!((long_bound - 0.015).abs() < 1e-9, "expected const branch, got {long_bound}");
    }

    #[test]
    fn negative_mark_rate_flips_sign_and_side() {
        // rMark < 0 -> -__calcRateBoundPositive(-rMark, k_iThresh, side.opposite())
        let b = bounds();
        let long_bound_pos = b.bound_for(Side::Long, 0.08, 0.01);
        let long_bound_neg = b.bound_for(Side::Long, -0.08, 0.01);
        // for negative mark, LONG's bound mirrors what SHORT would get at +0.08, negated
        let short_bound_pos = b.bound_for(Side::Short, 0.08, 0.01);
        assert!((long_bound_neg - (-short_bound_pos)).abs() < 1e-9);
        assert_ne!(long_bound_pos, -long_bound_neg, "slopes differ by side, so this isn't just a sign flip");
    }
}
