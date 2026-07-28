use tick_math::FixedX18;

use crate::{
    bounds::{clamp_ask, clamp_bid},
    error::QuoteError,
    reservation::{optimal_spread, reservation_rate},
    types::{AvellanedaStoikovParams, InventoryState, MakerRateBounds, Quote},
};

/// Ties together a reference rate (typically from `curve_engine::Curve`,
/// though any source works, this crate doesn't depend on `curve-engine`
/// for correctness, only for a sensible default reference source),
/// inventory state, A-S calibration, and the real on-chain maker bounds
/// into a placeable bid/ask pair.
pub struct QuotingEngine {
    params: AvellanedaStoikovParams,
}

impl QuotingEngine {
    pub fn new(params: AvellanedaStoikovParams) -> Result<Self, QuoteError> {
        params.validate()?;
        Ok(Self { params })
    }

    /// Compute a bid/ask quote.
    ///
    /// `reference_rate`: the "fair" rate to skew around, pass
    /// `curve_engine::Curve::rate_at(ttm)` for the smoothed cross-maturity
    /// view, or the market's own mark rate directly if you'd rather quote
    /// off the single market with no cross-maturity smoothing.
    ///
    /// `mark_rate` and `k_i_thresh` must be this specific market's own
    /// values (the maker bound formula is defined per-market, not off the
    /// smoothed curve). `k_i_thresh` is the same threshold
    /// `margin-sim::MarginConfig` uses, fetch it once per market and hand
    /// it to both.
    ///
    /// No `time_to_maturity` parameter here anymore, it used to feed the
    /// old (wrong) bounds formula. The real one doesn't use it at all.
    pub fn quote(
        &self,
        reference_rate: FixedX18,
        mark_rate: FixedX18,
        k_i_thresh: FixedX18,
        inventory: &InventoryState,
        bounds: &MakerRateBounds,
    ) -> Result<Quote, QuoteError> {
        let reference_f64 = reference_rate.to_f64();
        let mark_f64 = mark_rate.to_f64();
        let k_i_thresh_f64 = k_i_thresh.to_f64();

        let reservation = reservation_rate(reference_f64, inventory, &self.params);
        let half_spread = optimal_spread(&self.params) / 2.0;

        let raw_bid = reservation - half_spread;
        let raw_ask = reservation + half_spread;

        let bid = clamp_bid(raw_bid, bounds, mark_f64, k_i_thresh_f64);
        let ask = clamp_ask(raw_ask, bounds, mark_f64, k_i_thresh_f64);

        if bid >= ask {
            return Err(QuoteError::CrossedAfterClamp { bid, ask });
        }

        Ok(Quote {
            bid_rate: FixedX18::from_f64(bid),
            ask_rate: FixedX18::from_f64(ask),
            reservation_rate: FixedX18::from_f64(reservation),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> AvellanedaStoikovParams {
        AvellanedaStoikovParams { gamma: 0.1, sigma: 0.02, kappa: 1.5, horizon_secs: 3600, carry_weight: 0.0 }
    }

    fn wide_bounds() -> MakerRateBounds {
        MakerRateBounds {
            lo_upper_slope_base1e4: 30_000,
            lo_upper_const_base1e4: 5_000,
            lo_lower_slope_base1e4: 30_000,
            lo_lower_const_base1e4: 5_000,
        }
    }

    #[test]
    fn flat_inventory_quote_straddles_reference() {
        let engine = QuotingEngine::new(params()).unwrap();
        let inv = InventoryState::default();
        let quote = engine.quote(
            FixedX18::from_f64(0.05), FixedX18::from_f64(0.05), FixedX18::from_f64(0.001), &inv, &wide_bounds(),
        ).unwrap();

        assert!(quote.bid_rate < quote.reservation_rate);
        assert!(quote.ask_rate > quote.reservation_rate);
        assert!(quote.bid_rate < quote.ask_rate);
    }

    #[test]
    fn invalid_params_rejected_at_construction() {
        let bad = AvellanedaStoikovParams { gamma: -1.0, ..params() };
        assert!(QuotingEngine::new(bad).is_err());
    }

    #[test]
    fn quote_respects_maker_bounds_even_with_huge_inventory() {
        let engine = QuotingEngine::new(params()).unwrap();
        // huge inventory to try to push the quote way outside bounds
        let inv = InventoryState { net_dv01: 1_000_000_000.0, avg_locked_fixed_rate: None };
        let narrow_bounds = MakerRateBounds {
            lo_upper_slope_base1e4: 10_100,
            lo_upper_const_base1e4: 100,
            lo_lower_slope_base1e4: 9_900,
            lo_lower_const_base1e4: -100,
        };

        let quote = engine.quote(
            FixedX18::from_f64(0.05), FixedX18::from_f64(0.05), FixedX18::from_f64(0.001), &inv, &narrow_bounds,
        );

        // either it clamps successfully within bounds, or it correctly
        // reports a crossed quote, either way it must never silently
        // return something past its own side's bound
        if let Ok(q) = quote {
            let long_bound = narrow_bounds.bound_for(oms_core::Side::Long, 0.05, 0.001);
            let short_bound = narrow_bounds.bound_for(oms_core::Side::Short, 0.05, 0.001);
            assert!(q.bid_rate.to_f64() <= long_bound + 1e-9);
            assert!(q.ask_rate.to_f64() >= short_bound - 1e-9);
        }
    }

    #[test]
    fn one_sided_clamps_cannot_cross_even_with_absurdly_narrow_bounds() {
        // this is the flip side of the fix: with the old symmetric clamp,
        // a tiny [lower, upper] window could push both bid and ask to the
        // same edge and cross them. clamp_bid only ever moves bid DOWN
        // (min) and clamp_ask only ever moves ask UP (max), so as long as
        // raw_bid < raw_ask going in (guaranteed by optimal_spread being
        // strictly positive for valid params), they can't cross coming
        // out, no matter how tight the bounds are. CrossedAfterClamp is
        // effectively unreachable now under normal params, this test
        // exists so a future change that reintroduces a two-sided clamp
        // gets caught here instead of in production.
        let engine = QuotingEngine::new(params()).unwrap();
        let inv = InventoryState::default();
        let brutal_bounds = MakerRateBounds {
            lo_upper_slope_base1e4: 1,
            lo_upper_const_base1e4: -10_000,
            lo_lower_slope_base1e4: 1,
            lo_lower_const_base1e4: 10_000,
        };

        let quote = engine.quote(
            FixedX18::from_f64(0.05), FixedX18::from_f64(0.05), FixedX18::from_f64(0.001), &inv, &brutal_bounds,
        ).expect("one-sided clamps must never cross a positive spread");
        assert!(quote.bid_rate < quote.ask_rate);
    }
}
