use oms_core::Side;

use crate::types::{CrossVenueObservation, CrossVenueSignal};

/// Detects a cross-venue basis worth trading. `min_abs_basis` should
/// account for round-trip cost on both legs (Boros has zero maker fee per
/// `Mechanics/Fees.md`, but the CEX leg and any bridging/settlement cost
/// are not zero). Not defaulted: that cost structure is the caller's to
/// know.
pub fn detect_cross_venue_signal(obs: &CrossVenueObservation, min_abs_basis: f64) -> Option<CrossVenueSignal> {
    let basis = obs.boros_implied_apr - obs.cex_expected_funding_apr;
    if basis == 0.0 || basis.abs() < min_abs_basis {
        return None;
    }
    // basis > 0: Boros fixed rate is rich relative to CEX funding ->
    // receive that richness by going SHORT Boros (short = "receives fixed,
    // pays floating" per Order.sol's Side semantics, already established
    // in oms-core).
    let boros_side = if basis > 0.0 { Side::Short } else { Side::Long };
    Some(CrossVenueSignal { boros_market_id: obs.boros_market_id, basis, boros_side })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(boros: f64, cex: f64) -> CrossVenueObservation {
        CrossVenueObservation { boros_market_id: 1, boros_implied_apr: boros, cex_expected_funding_apr: cex }
    }

    #[test]
    fn rich_boros_fixed_rate_signals_short() {
        let signal = detect_cross_venue_signal(&obs(0.10, 0.05), 0.01).unwrap();
        assert!(signal.basis > 0.0);
        assert_eq!(signal.boros_side, Side::Short);
    }

    #[test]
    fn cheap_boros_fixed_rate_signals_long() {
        let signal = detect_cross_venue_signal(&obs(0.02, 0.05), 0.01).unwrap();
        assert!(signal.basis < 0.0);
        assert_eq!(signal.boros_side, Side::Long);
    }

    #[test]
    fn small_basis_filtered_by_threshold() {
        assert!(detect_cross_venue_signal(&obs(0.051, 0.05), 0.01).is_none());
        assert!(detect_cross_venue_signal(&obs(0.07, 0.05), 0.01).is_some());
    }

    #[test]
    fn zero_basis_no_signal() {
        assert!(detect_cross_venue_signal(&obs(0.05, 0.05), 0.0).is_none());
    }
}
