//! Avellaneda-Stoikov (2008) closed-form reservation price and spread,
//! adapted to DV01 inventory instead of contract count. Uses the
//! practical closed-form approximation from Guéant-Lehalle-Fernandez-Tapia
//! (2012), not the full HJB PDE.
//!
//! ## The carry-adjustment term, and why it's weaker than it might look
//!
//! Le (2026, arXiv:2605.06405) extends A-S with a `-qf` HJB coupling term:
//! holding inventory `q` is costly when the *ongoing* funding rate `f` is
//! adverse, because perpetual funding is a continuously-accruing cash flow
//! paid while the position is held.
//!
//! Boros doesn't have that structure. The fixed leg is paid **upfront**
//! (`PaymentLib.calcUpfrontFixedCost`), by the time a position exists,
//! that cost is sunk and already reflected in mark-to-market PV
//! (`Position::value` in `margin-sim`). There's no ongoing "funding you
//! keep paying while holding" the way a perpetual has. So the paper's
//! coupling term doesn't transfer cleanly; `carry_adjustment` below is a
//! much weaker, honestly-labeled heuristic: it nudges the skew based on
//! whether the account's existing locked-in rate looks favorable versus
//! the current curve reference, on the theory that a favorably-locked
//! position is less urgent to unwind, not a rigorously derived
//! optimal-control term like the rest of this module. `carry_weight = 0.0`
//! disables it entirely; that's a legitimate, arguably more defensible
//! default than any nonzero value this crate could pick for you.

use crate::types::{AvellanedaStoikovParams, InventoryState};

fn horizon_years(params: &AvellanedaStoikovParams) -> f64 {
    params.horizon_secs as f64 / tick_math::SECONDS_PER_YEAR as f64
}

/// Classic A-S reservation price: `r = s - q·γ·σ²·(T-t)`, plus the optional
/// carry adjustment (see module docs).
pub fn reservation_rate(reference_rate: f64, inventory: &InventoryState, params: &AvellanedaStoikovParams) -> f64 {
    let skew = inventory.net_dv01 * params.gamma * params.sigma.powi(2) * horizon_years(params);
    reference_rate - skew + carry_adjustment(reference_rate, inventory, params)
}

/// See module docs. `0.0` whenever `carry_weight == 0.0` or the account is
/// flat (`avg_locked_fixed_rate.is_none()`).
fn carry_adjustment(reference_rate: f64, inventory: &InventoryState, params: &AvellanedaStoikovParams) -> f64 {
    let Some(locked) = inventory.avg_locked_fixed_rate else { return 0.0 };
    if inventory.net_dv01 == 0.0 || params.carry_weight == 0.0 {
        return 0.0;
    }
    // advantage > 0 means the account's locked rate is favorable relative
    // to the current reference, given its position direction (see module
    // docs for the sign derivation: long wants locked < reference, short
    // wants locked > reference).
    let advantage = inventory.net_dv01.signum() * (reference_rate - locked);
    params.carry_weight * advantage
}

/// `δ = γ·σ²·(T-t) + (2/γ)·ln(1+γ/κ)`, the full A-S optimal spread, this
/// is `ask - bid`, not the half-spread. Boros charges makers **zero fee on
/// placement** (`Mechanics/Fees.md`: "Maker orders incur no fees when
/// placed"), so unlike a venue with maker fees or rebates, this spread
/// doesn't need a fee-coverage floor added on top, it's purely
/// inventory-risk and adverse-selection compensation.
///
/// **Calibration caveat, found while testing this, not obvious from the
/// formula on its own**: the two terms don't move the same direction in
/// `γ`. The inventory term (`γσ²(T-t)`) increases with `γ`, as expected.
/// The liquidity term (`(2/γ)ln(1+γ/κ)`) *decreases* with `γ` for small
/// `γ` (its first-order Taylor expansion is `2/κ - γ/κ² + O(γ²)`). For
/// short requoting horizons or low-volatility markets, the common case
/// for frequent requoting, not an edge case, the liquidity term
/// dominates, so **increasing `γ` can narrow the total spread**, the
/// opposite of the usual "more risk-averse = wider" intuition. It only
/// widens monotonically with `γ` once `σ²·(T-t)` is large enough for the
/// inventory term to dominate. Check this numerically for your actual
/// calibration inputs instead of assuming the intuitive direction.
pub fn optimal_spread(params: &AvellanedaStoikovParams) -> f64 {
    let t = horizon_years(params);
    let inventory_term = params.gamma * params.sigma.powi(2) * t;
    let liquidity_term = (2.0 / params.gamma) * (1.0 + params.gamma / params.kappa).ln();
    inventory_term + liquidity_term
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> AvellanedaStoikovParams {
        AvellanedaStoikovParams { gamma: 0.1, sigma: 0.02, kappa: 1.5, horizon_secs: 3600, carry_weight: 0.0 }
    }

    #[test]
    fn zero_inventory_reservation_equals_reference() {
        let inv = InventoryState::default();
        let r = reservation_rate(0.05, &inv, &params());
        assert!((r - 0.05).abs() < 1e-12);
    }

    #[test]
    fn long_inventory_skews_reservation_down() {
        let inv = InventoryState { net_dv01: 1000.0, avg_locked_fixed_rate: None };
        let r = reservation_rate(0.05, &inv, &params());
        assert!(r < 0.05, "long inventory should skew reservation below reference, got {r}");
    }

    #[test]
    fn short_inventory_skews_reservation_up() {
        let inv = InventoryState { net_dv01: -1000.0, avg_locked_fixed_rate: None };
        let r = reservation_rate(0.05, &inv, &params());
        assert!(r > 0.05, "short inventory should skew reservation above reference, got {r}");
    }

    #[test]
    fn larger_inventory_skews_more() {
        let small = InventoryState { net_dv01: 100.0, avg_locked_fixed_rate: None };
        let large = InventoryState { net_dv01: 1000.0, avg_locked_fixed_rate: None };
        let r_small = reservation_rate(0.05, &small, &params());
        let r_large = reservation_rate(0.05, &large, &params());
        assert!((0.05 - r_large) > (0.05 - r_small), "larger inventory must skew further");
    }

    #[test]
    fn optimal_spread_is_always_positive() {
        for gamma in [0.01, 0.05, 0.1, 0.5, 1.0, 5.0] {
            let s = optimal_spread(&AvellanedaStoikovParams { gamma, ..params() });
            assert!(s > 0.0, "spread must be positive for gamma={gamma}, got {s}");
        }
    }

    #[test]
    fn spread_widens_with_gamma_when_inventory_term_dominates() {
        // high vol, long horizon -> inventory term (γσ²T) dominates the
        // liquidity term -> total spread increases with γ, matching the
        // "more risk-averse = wider" intuition
        let regime = AvellanedaStoikovParams { gamma: 0.05, sigma: 0.8, kappa: 1.5, horizon_secs: tick_math::SECONDS_PER_YEAR, carry_weight: 0.0 };
        let narrow = optimal_spread(&regime);
        let wide = optimal_spread(&AvellanedaStoikovParams { gamma: 0.5, ..regime });
        assert!(wide > narrow, "in the inventory-dominated regime, higher gamma must widen spread: {narrow} vs {wide}");
    }

    #[test]
    fn spread_can_narrow_with_gamma_when_liquidity_term_dominates() {
        // short horizon, low vol (typical high-frequency requoting) -> the
        // liquidity term (2/γ)ln(1+γ/κ) dominates and DECREASES with γ for
        // small γ -> total spread can narrow as gamma increases. This is a
        // real, documented property of the closed form (see optimal_spread's
        // doc comment), not a bug, this test exists specifically so a
        // future change that "fixes" this into naive monotonicity gets caught.
        let narrow_gamma = optimal_spread(&params()); // gamma=0.1, sigma=0.02, horizon=1h
        let wide_gamma = optimal_spread(&AvellanedaStoikovParams { gamma: 1.0, ..params() });
        assert!(wide_gamma < narrow_gamma, "in the liquidity-dominated regime, higher gamma should narrow spread: {narrow_gamma} vs {wide_gamma}");
    }

    #[test]
    fn optimal_spread_widens_with_longer_horizon() {
        let short_h = optimal_spread(&AvellanedaStoikovParams { horizon_secs: 60, ..params() });
        let long_h = optimal_spread(&AvellanedaStoikovParams { horizon_secs: 3600 * 24, ..params() });
        assert!(long_h > short_h, "longer horizon must widen the inventory-risk component of spread");
    }

    #[test]
    fn carry_adjustment_disabled_when_weight_zero() {
        let inv = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: Some(0.03) };
        let p = params(); // carry_weight: 0.0
        let with_carry = reservation_rate(0.05, &inv, &p);
        let no_carry_inv = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: None };
        let without = reservation_rate(0.05, &no_carry_inv, &p);
        assert!((with_carry - without).abs() < 1e-12);
    }

    #[test]
    fn favorable_carry_reduces_long_skew() {
        // long position, locked rate BELOW current reference -> favorable
        // (pays less than market) -> should pull reservation back UP
        // (less urgent to sell) compared to no carry adjustment at all
        let p = AvellanedaStoikovParams { carry_weight: 1.0, ..params() };
        let inv_favorable = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: Some(0.03) };
        let inv_flat_carry = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: None };

        let r_favorable = reservation_rate(0.05, &inv_favorable, &p);
        let r_no_carry = reservation_rate(0.05, &inv_flat_carry, &p);

        assert!(r_favorable > r_no_carry, "favorable carry should pull reservation up (less selling pressure)");
    }

    #[test]
    fn unfavorable_carry_increases_long_skew() {
        // long position, locked rate ABOVE current reference -> unfavorable
        let p = AvellanedaStoikovParams { carry_weight: 1.0, ..params() };
        let inv_unfavorable = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: Some(0.08) };
        let inv_flat_carry = InventoryState { net_dv01: 500.0, avg_locked_fixed_rate: None };

        let r_unfavorable = reservation_rate(0.05, &inv_unfavorable, &p);
        let r_no_carry = reservation_rate(0.05, &inv_flat_carry, &p);

        assert!(r_unfavorable < r_no_carry, "unfavorable carry should push reservation down further (more selling pressure)");
    }

    #[test]
    fn params_validation_rejects_nonpositive_gamma() {
        let p = AvellanedaStoikovParams { gamma: 0.0, ..params() };
        assert!(p.validate().is_err());
    }

    #[test]
    fn params_validation_accepts_reasonable_values() {
        assert!(params().validate().is_ok());
    }
}
