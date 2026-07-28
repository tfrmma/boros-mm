use curve_engine::ButterflySignal;
use oms_core::Side;

use crate::types::CalendarSpreadTrade;

/// Translates a `curve_engine::ButterflySignal` into a trade direction.
///
/// `deviation < 0`: the mid maturity is priced cheap (its fixed rate is
/// low relative to its neighbors), going `Long` there (paying that low
/// fixed rate) is the attractive side, paired with `Short` on both wings
/// (receiving their relatively richer fixed rate).
/// `deviation > 0`: the reverse.
pub fn to_calendar_spread_trade(signal: ButterflySignal) -> CalendarSpreadTrade {
    let (mid_side, wing_side) = if signal.deviation < 0.0 {
        (Side::Long, Side::Short)
    } else {
        (Side::Short, Side::Long)
    };
    CalendarSpreadTrade { signal, mid_side, wing_side }
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
        let trade = to_calendar_spread_trade(signal(-0.02));
        assert_eq!(trade.mid_side, Side::Long);
        assert_eq!(trade.wing_side, Side::Short);
    }

    #[test]
    fn rich_mid_maturity_goes_short_at_mid_long_wings() {
        let trade = to_calendar_spread_trade(signal(0.02));
        assert_eq!(trade.mid_side, Side::Short);
        assert_eq!(trade.wing_side, Side::Long);
    }

    #[test]
    fn mid_and_wing_side_are_always_opposite() {
        for dev in [-0.05, -0.001, 0.001, 0.05] {
            let trade = to_calendar_spread_trade(signal(dev));
            assert_ne!(trade.mid_side, trade.wing_side);
        }
    }
}
