//! DV01 (dollar value of 1bp): `d(PV)/d(rate) × 0.0001`. Since Boros
//! position value is `PV = size × rate × ttm` (`PaymentLib.calcPositionValue`,
//! ported exactly in `margin-sim::Position::value`),
//! `d(PV)/d(rate) = size × ttm`, so `DV01 = |size| × ttm_years × 0.0001`.
//! This is the inventory unit `quoting-engine`'s `InventoryState::net_dv01`
//! expects, and the unit this crate's pre-trade limits are expressed in:
//! inventory risk measured in rate-sensitivity terms, not raw notional.

use tick_math::FixedX18;

const BASIS_POINT: f64 = 0.0001;

/// Signed DV01 for one position: positive for a long (positive `size`),
/// negative for a short.
pub fn position_dv01(size: FixedX18, time_to_maturity_secs: u32) -> f64 {
    let ttm_years = time_to_maturity_secs as f64 / tick_math::SECONDS_PER_YEAR as f64;
    size.to_f64() * ttm_years * BASIS_POINT
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_position_positive_dv01() {
        let dv01 = position_dv01(FixedX18::from_f64(1000.0), tick_math::SECONDS_PER_YEAR);
        assert!(dv01 > 0.0);
        assert!((dv01 - 0.1).abs() < 1e-9, "1000 * 1yr * 0.0001 = 0.1, got {dv01}");
    }

    #[test]
    fn short_position_negative_dv01() {
        let dv01 = position_dv01(FixedX18::from_f64(-1000.0), tick_math::SECONDS_PER_YEAR);
        assert!(dv01 < 0.0);
        assert!((dv01 + 0.1).abs() < 1e-9);
    }

    #[test]
    fn zero_ttm_zero_dv01() {
        let dv01 = position_dv01(FixedX18::from_f64(1000.0), 0);
        assert_eq!(dv01, 0.0);
    }

    #[test]
    fn scales_linearly_with_ttm() {
        let dv01_1y = position_dv01(FixedX18::from_f64(1000.0), tick_math::SECONDS_PER_YEAR);
        let dv01_half_y = position_dv01(FixedX18::from_f64(1000.0), tick_math::SECONDS_PER_YEAR / 2);
        assert!((dv01_1y - 2.0 * dv01_half_y).abs() < 1e-9);
    }
}
