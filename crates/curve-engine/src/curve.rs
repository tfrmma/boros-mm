use tick_math::FixedX18;

use crate::{
    error::CurveError,
    spline::MonotoneCubicSpline,
    types::{ButterflySignal, CurvePoint, Zone},
};

/// A fitted implied-APR curve across the maturities of one zone.
#[derive(Debug)]
pub struct Curve {
    zone_name: String,
    spline: MonotoneCubicSpline,
    /// Kept alongside the spline (not just inside it) because butterfly
    /// detection needs the *original* rates at *exactly* the observed
    /// maturities, not spline-evaluated values. Evaluating the spline at
    /// its own knots returns the same numbers today, but keeping the
    /// source data explicit avoids that becoming a silent assumption.
    sorted_points: Vec<CurvePoint>,
}

impl Curve {
    /// Fits a curve from a zone's points. Rates are converted to `f64` for
    /// the fit (see `spline` module docs for why). The FixedX18 raw values
    /// are still available via `sorted_points` for anything that needs
    /// exact rates at the observed maturities specifically.
    pub fn fit(zone: &Zone) -> Result<Self, CurveError> {
        let mut sorted_points = zone.points.clone();
        sorted_points.sort_by_key(|p| p.time_to_maturity_secs);

        let xy: Vec<(f64, f64)> = sorted_points.iter()
            .map(|p| (p.time_to_maturity_secs as f64, p.rate.to_f64()))
            .collect();

        let spline = MonotoneCubicSpline::fit(xy)?;

        Ok(Self { zone_name: zone.name.clone(), spline, sorted_points })
    }

    pub fn zone_name(&self) -> &str {
        &self.zone_name
    }

    /// Reference implied APR at an arbitrary time-to-maturity, interpolated
    /// from the zone's observed markets. `None` outside the observed
    /// maturity range, see `MonotoneCubicSpline::eval`'s doc comment on
    /// why this never extrapolates.
    pub fn rate_at(&self, time_to_maturity_secs: u32) -> Option<FixedX18> {
        self.spline.eval(time_to_maturity_secs as f64).map(FixedX18::from_f64)
    }

    pub fn shortest_maturity_secs(&self) -> u32 {
        self.sorted_points[0].time_to_maturity_secs
    }

    pub fn longest_maturity_secs(&self) -> u32 {
        self.sorted_points[self.sorted_points.len() - 1].time_to_maturity_secs
    }

    /// Scans every consecutive triple of observed maturities for a
    /// deviation from the straight line between its neighbors. See
    /// `ButterflySignal`'s doc comment: this is a relative-value signal,
    /// not a riskless arbitrage.
    ///
    /// `min_abs_deviation` filters out noise: pass `0.0` to get every
    /// triple regardless of size, or a real threshold (e.g. `0.001` for 10
    /// bps) to only see signals worth acting on. Not defaulted, the right
    /// threshold depends on fees, typical bid/ask spread, and how much
    /// capital the caller is willing to commit to a calendar spread, none
    /// of which this crate knows.
    pub fn detect_butterflies(&self, min_abs_deviation: f64) -> Vec<ButterflySignal> {
        let mut signals = Vec::new();

        for w in self.sorted_points.windows(3) {
            let (left, mid, right) = (w[0], w[1], w[2]);

            let left_t = left.time_to_maturity_secs as f64;
            let mid_t = mid.time_to_maturity_secs as f64;
            let right_t = right.time_to_maturity_secs as f64;

            let left_r = left.rate.to_f64();
            let mid_r = mid.rate.to_f64();
            let right_r = right.rate.to_f64();

            let weight = (mid_t - left_t) / (right_t - left_t);
            let linear_interp = left_r + weight * (right_r - left_r);
            let deviation = mid_r - linear_interp;

            if deviation.abs() >= min_abs_deviation {
                signals.push(ButterflySignal {
                    left_maturity_secs: left.time_to_maturity_secs,
                    mid_maturity_secs: mid.time_to_maturity_secs,
                    right_maturity_secs: right.time_to_maturity_secs,
                    deviation,
                });
            }
        }

        signals
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(market_id: u32, ttm_days: u32, rate: f64) -> CurvePoint {
        CurvePoint { market_id, time_to_maturity_secs: ttm_days * 86_400, rate: FixedX18::from_f64(rate) }
    }

    fn zone(points: Vec<CurvePoint>) -> Zone {
        Zone { name: "ETH".to_string(), points }
    }

    #[test]
    fn fits_and_interpolates_between_markets() {
        let z = zone(vec![pt(1, 30, 0.05), pt(2, 90, 0.06), pt(3, 180, 0.07)]);
        let curve = Curve::fit(&z).unwrap();

        let rate_60d = curve.rate_at(60 * 86_400).unwrap().to_f64();
        assert!(rate_60d > 0.05 && rate_60d < 0.06, "expected between 5% and 6%, got {rate_60d}");
    }

    #[test]
    fn none_outside_observed_range() {
        let z = zone(vec![pt(1, 30, 0.05), pt(2, 90, 0.06)]);
        let curve = Curve::fit(&z).unwrap();
        assert!(curve.rate_at(10 * 86_400).is_none());
        assert!(curve.rate_at(200 * 86_400).is_none());
    }

    #[test]
    fn no_butterfly_signal_on_a_straight_line() {
        let z = zone(vec![pt(1, 30, 0.05), pt(2, 90, 0.06), pt(3, 150, 0.07)]); // evenly spaced, linear rate
        let curve = Curve::fit(&z).unwrap();
        let signals = curve.detect_butterflies(1e-9);
        assert!(signals.is_empty(), "straight-line rates must not trigger a butterfly signal: {signals:?}");
    }

    #[test]
    fn detects_cheap_middle_maturity() {
        // middle maturity priced noticeably below its neighbors' straight line
        let z = zone(vec![pt(1, 30, 0.08), pt(2, 90, 0.03), pt(3, 150, 0.08)]);
        let curve = Curve::fit(&z).unwrap();
        let signals = curve.detect_butterflies(0.01);

        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].mid_maturity_secs, 90 * 86_400);
        assert!(signals[0].deviation < 0.0, "middle maturity is cheap -> negative deviation, got {}", signals[0].deviation);
    }

    #[test]
    fn detects_rich_middle_maturity() {
        let z = zone(vec![pt(1, 30, 0.03), pt(2, 90, 0.08), pt(3, 150, 0.03)]);
        let curve = Curve::fit(&z).unwrap();
        let signals = curve.detect_butterflies(0.01);

        assert_eq!(signals.len(), 1);
        assert!(signals[0].deviation > 0.0, "middle maturity is rich -> positive deviation, got {}", signals[0].deviation);
    }

    #[test]
    fn threshold_filters_small_deviations() {
        let z = zone(vec![pt(1, 30, 0.0500), pt(2, 90, 0.0505), pt(3, 150, 0.0500)]); // tiny 5bps wobble
        let curve = Curve::fit(&z).unwrap();
        assert!(curve.detect_butterflies(0.01).is_empty()); // filtered out at 100bps threshold
        assert!(!curve.detect_butterflies(0.0001).is_empty()); // visible at 1bp threshold
    }

    #[test]
    fn needs_at_least_two_markets() {
        let z = zone(vec![pt(1, 30, 0.05)]);
        assert_eq!(Curve::fit(&z).unwrap_err(), CurveError::InsufficientPoints(1));
    }

    #[test]
    fn shortest_and_longest_maturity() {
        let z = zone(vec![pt(2, 90, 0.06), pt(1, 30, 0.05), pt(3, 180, 0.07)]);
        let curve = Curve::fit(&z).unwrap();
        assert_eq!(curve.shortest_maturity_secs(), 30 * 86_400);
        assert_eq!(curve.longest_maturity_secs(), 180 * 86_400);
    }
}
