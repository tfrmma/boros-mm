use curve_engine::ButterflySignal;
use oms_core::Side;

use crate::types::CalendarSpreadTrade;

/// Translates a `curve_engine::ButterflySignal` into a trade direction and
/// DV01-neutral sizing, `mid_size` is the caller's chosen notional for the
/// mid leg (risk budget, account size, whatever policy picks it), the two
/// wing sizes are derived from it.
///
/// `deviation < 0`: the mid maturity is priced cheap (its fixed rate is
/// low relative to its neighbors), going `Long` there (paying that low
/// fixed rate) is the attractive side, paired with `Short` on both wings
/// (receiving their relatively richer fixed rate).
/// `deviation > 0`: the reverse.
pub fn to_calendar_spread_trade(signal: ButterflySignal, mid_size: f64) -> CalendarSpreadTrade {
    let (mid_side, wing_side) = if signal.deviation < 0.0 {
        (Side::Long, Side::Short)
    } else {
        (Side::Short, Side::Long)
    };
    let left_size = dv01_neutral_wing_size(mid_size, signal.mid_maturity_secs, signal.left_maturity_secs);
    let right_size = dv01_neutral_wing_size(mid_size, signal.mid_maturity_secs, signal.right_maturity_secs);
    CalendarSpreadTrade { signal, mid_side, wing_side, mid_size, left_size, right_size }
}

/// The size at `wing_maturity_secs` whose DV01 equals half the mid leg's
/// DV01. Since `DV01 = |size| * ttm_years * 0.0001`, and `ttm_years` is
/// just `ttm_secs / SECONDS_PER_YEAR`, that shared factor cancels out of
/// the ratio: `wing_size / wing_ttm = mid_size / (2 * mid_ttm)`, solved
/// for `wing_size` below. Working in raw seconds instead of years avoids
/// needing `SECONDS_PER_YEAR` here at all.
fn dv01_neutral_wing_size(mid_size: f64, mid_maturity_secs: u32, wing_maturity_secs: u32) -> f64 {
    mid_size * mid_maturity_secs as f64 / (2.0 * wing_maturity_secs as f64)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(deviation: f64) -> ButterflySignal {
        ButterflySignal { left_maturity_secs: 30 * 86_400, mid_maturity_secs: 90 * 86_400, right_maturity_secs: 150 * 86_400, deviation }
    }

    #[test]
    fn cheap_mid_maturity_goes_long_at_mid_short_wings() {
        let trade = to_calendar_spread_trade(signal(-0.02), 1000.0);
        assert_eq!(trade.mid_side, Side::Long);
        assert_eq!(trade.wing_side, Side::Short);
    }

    #[test]
    fn rich_mid_maturity_goes_short_at_mid_long_wings() {
        let trade = to_calendar_spread_trade(signal(0.02), 1000.0);
        assert_eq!(trade.mid_side, Side::Short);
        assert_eq!(trade.wing_side, Side::Long);
    }

    #[test]
    fn mid_and_wing_side_are_always_opposite() {
        for dev in [-0.05, -0.001, 0.001, 0.05] {
            let trade = to_calendar_spread_trade(signal(dev), 1000.0);
            assert_ne!(trade.mid_side, trade.wing_side);
        }
    }

    #[test]
    fn each_wing_gets_the_size_that_matches_half_the_mid_dv01() {
        // left=30d, mid=90d, right=150d: unequal notional sizes, since DV01
        // (not notional) is what's split evenly, see the next test
        let trade = to_calendar_spread_trade(signal(-0.02), 1000.0);
        assert_eq!(trade.mid_size, 1000.0);
        assert!((trade.left_size - trade.mid_size * 90.0 / (2.0 * 30.0)).abs() < 1e-9);
        assert!((trade.right_size - trade.mid_size * 90.0 / (2.0 * 150.0)).abs() < 1e-9);
    }

    #[test]
    fn combined_wing_dv01_equals_the_mid_dv01() {
        // DV01 proxy = size * ttm_secs (the ttm_years*0.0001 factor cancels
        // in a ratio, see dv01_neutral_wing_size's doc comment)
        let trade = to_calendar_spread_trade(signal(-0.02), 1000.0);
        let mid_dv01 = trade.mid_size * 90.0 * 86_400.0;
        let left_dv01 = trade.left_size * 30.0 * 86_400.0;
        let right_dv01 = trade.right_size * 150.0 * 86_400.0;
        assert!((left_dv01 + right_dv01 - mid_dv01).abs() / mid_dv01 < 1e-9);
    }

    #[test]
    fn a_closer_wing_gets_less_size_than_a_farther_wing_for_the_same_offsetting_dv01() {
        // shorter time-to-maturity needs more notional to carry the same
        // DV01, so the closer wing (30d) should be sized larger than the
        // farther wing (150d) even though both offset the same DV01 share
        let trade = to_calendar_spread_trade(signal(-0.02), 1000.0);
        assert!(trade.left_size > trade.right_size, "left (30d) should need more notional than right (150d)");
    }
}
