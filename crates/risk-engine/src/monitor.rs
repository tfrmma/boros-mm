use crate::types::{DivergenceConfig, RiskAlert};

/// Compares this workspace's shadow-computed health ratio against the real
/// value (fetched from the API/chain). `real` should come from
/// `settleAllAndGet`/the account query endpoint, not derived locally,
/// the whole point is catching cases where the shadow calc has drifted.
pub fn check_health_ratio_divergence(shadow: f64, real: f64, cfg: &DivergenceConfig) -> Option<RiskAlert> {
    if shadow == real {
        return None; // covers both-infinite (flat account, no MM) exactly
    }
    if !real.is_finite() || !shadow.is_finite() {
        // one finite, one not -> definitely a divergence, can't compute a
        // meaningful relative ratio against a non-finite denominator
        return Some(RiskAlert::HealthRatioDivergence { shadow, real, relative_diff: f64::INFINITY });
    }
    let relative_diff = if real.abs() > f64::EPSILON {
        ((shadow - real) / real).abs()
    } else {
        (shadow - real).abs()
    };
    if relative_diff > cfg.max_health_ratio_relative_diff {
        Some(RiskAlert::HealthRatioDivergence { shadow, real, relative_diff })
    } else {
        None
    }
}

/// Compares this workspace's tracked mark rate (from `feed-ingest`, once
/// its wire format exists) against the real on-chain/API value.
pub fn check_mark_rate_divergence(shadow: f64, real: f64, cfg: &DivergenceConfig) -> Option<RiskAlert> {
    let abs_diff = (shadow - real).abs();
    if abs_diff > cfg.max_mark_rate_abs_diff {
        Some(RiskAlert::MarkRateDivergence { shadow, real, abs_diff })
    } else {
        None
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DivergenceConfig {
        DivergenceConfig { max_health_ratio_relative_diff: 0.05, max_mark_rate_abs_diff: 0.001 }
    }

    #[test]
    fn no_alert_when_within_tolerance() {
        assert_eq!(check_health_ratio_divergence(1.50, 1.52, &cfg()), None);
        assert_eq!(check_mark_rate_divergence(0.0800, 0.0805, &cfg()), None);
    }

    #[test]
    fn alert_on_health_ratio_divergence() {
        let alert = check_health_ratio_divergence(2.0, 1.0, &cfg());
        assert!(matches!(alert, Some(RiskAlert::HealthRatioDivergence { .. })));
    }

    #[test]
    fn alert_on_mark_rate_divergence() {
        let alert = check_mark_rate_divergence(0.08, 0.10, &cfg());
        assert!(matches!(alert, Some(RiskAlert::MarkRateDivergence { .. })));
    }

    #[test]
    fn both_infinite_and_equal_is_not_a_divergence() {
        // flat account on both sides: health_ratio = infinity by convention
        assert_eq!(check_health_ratio_divergence(f64::INFINITY, f64::INFINITY, &cfg()), None);
    }

    #[test]
    fn one_infinite_one_finite_is_a_divergence() {
        let alert = check_health_ratio_divergence(f64::INFINITY, 1.2, &cfg());
        assert!(alert.is_some());
    }
}
