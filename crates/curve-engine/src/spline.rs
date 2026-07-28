//! Monotone cubic Hermite interpolation (Fritsch-Carlson, 1980).
//!
//! Given data that's monotone between consecutive points, this produces an
//! interpolant that's also monotone there. A naive cubic spline can
//! overshoot and imply a locally *decreasing* rate between two points that
//! are both increasing, a real (if small) correctness bug for a rate
//! curve: overshoot there could suggest a butterfly signal that isn't
//! really in the input data at all.
//!
//! f64 throughout, not `FixedX18`: this is a reference/signal curve for
//! quoting and relative-value detection, not a settlement calculation,
//! same precedent as `tick_to_rate`/`rate_to_tick` in `tick-math`.

use crate::error::CurveError;

#[derive(Debug)]
pub struct MonotoneCubicSpline {
    /// Sorted ascending by `.0` (the x-coordinate), strictly increasing, no
    /// duplicates, enforced at construction.
    points: Vec<(f64, f64)>,
    /// One tangent per point, same length as `points`.
    tangents: Vec<f64>,
}

impl MonotoneCubicSpline {
    /// `points` need not be pre-sorted; duplicates in `x` are rejected
    /// (`CurveError::DuplicateMaturity`, using the raw `u32` seconds value
    /// for the error since that's what the caller will recognize, the
    /// caller passes seconds as `x`, see `Curve::fit`).
    pub fn fit(mut points: Vec<(f64, f64)>) -> Result<Self, CurveError> {
        if points.len() < 2 {
            return Err(CurveError::InsufficientPoints(points.len()));
        }
        for &(x, y) in &points {
            if !x.is_finite() || !y.is_finite() {
                return Err(CurveError::NonFiniteRate(x as u32));
            }
        }
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("checked finite above"));
        for w in points.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(CurveError::DuplicateMaturity(w[0].0 as u32));
            }
        }

        let n = points.len();
        let secants: Vec<f64> = (0..n - 1)
            .map(|k| (points[k + 1].1 - points[k].1) / (points[k + 1].0 - points[k].0))
            .collect();

        let mut tangents = vec![0.0; n];
        tangents[0] = secants[0];
        tangents[n - 1] = secants[n - 2];
        for k in 1..n - 1 {
            tangents[k] = (secants[k - 1] + secants[k]) / 2.0;
        }

        // Fritsch-Carlson monotonicity constraint: for each segment, the
        // pair of tangents (alpha, beta) normalized by the secant must lie
        // within the circle of radius 3, or the interpolant can overshoot.
        for k in 0..n - 1 {
            let d = secants[k];
            if d == 0.0 {
                tangents[k] = 0.0;
                tangents[k + 1] = 0.0;
                continue;
            }
            let mut alpha = tangents[k] / d;
            let mut beta = tangents[k + 1] / d;
            if alpha < 0.0 {
                tangents[k] = 0.0;
                alpha = 0.0;
            }
            if beta < 0.0 {
                tangents[k + 1] = 0.0;
                beta = 0.0;
            }
            let s = alpha * alpha + beta * beta;
            if s > 9.0 {
                let tau = 3.0 / s.sqrt();
                tangents[k] = tau * alpha * d;
                tangents[k + 1] = tau * beta * d;
            }
        }

        Ok(Self { points, tangents })
    }

    pub fn domain(&self) -> (f64, f64) {
        (self.points[0].0, self.points[self.points.len() - 1].0)
    }

    /// Evaluate at `x`. Returns `None` outside the fitted domain, this
    /// spline never extrapolates. A rate curve extrapolated past its
    /// furthest observed maturity is a guess, not a signal, and callers
    /// have to opt into that instead of getting it silently.
    pub fn eval(&self, x: f64) -> Option<f64> {
        let (lo, hi) = self.domain();
        if x < lo || x > hi {
            return None;
        }

        // exact hit at a knot, avoids floating point boundary weirdness
        // when x lands exactly on the last point
        if let Some(&(_, y)) = self.points.iter().find(|&&(px, _)| px == x) {
            return Some(y);
        }

        let seg = self.points.windows(2).position(|w| x >= w[0].0 && x <= w[1].0)?;
        let (x0, y0) = self.points[seg];
        let (x1, y1) = self.points[seg + 1];
        let (m0, m1) = (self.tangents[seg], self.tangents[seg + 1]);

        let h = x1 - x0;
        let t = (x - x0) / h;
        let t2 = t * t;
        let t3 = t2 * t;

        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;

        Some(h00 * y0 + h10 * h * m0 + h01 * y1 + h11 * h * m1)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_exactly_at_knots() {
        let spline = MonotoneCubicSpline::fit(vec![(0.0, 1.0), (1.0, 2.0), (2.0, 1.5), (3.0, 3.0)]).unwrap();
        assert_eq!(spline.eval(0.0), Some(1.0));
        assert_eq!(spline.eval(1.0), Some(2.0));
        assert_eq!(spline.eval(2.0), Some(1.5));
        assert_eq!(spline.eval(3.0), Some(3.0));
    }

    #[test]
    fn returns_none_outside_domain() {
        let spline = MonotoneCubicSpline::fit(vec![(0.0, 1.0), (1.0, 2.0)]).unwrap();
        assert_eq!(spline.eval(-0.1), None);
        assert_eq!(spline.eval(1.1), None);
    }

    #[test]
    fn monotone_input_produces_monotone_output_no_overshoot() {
        // strictly increasing input -- a naive cubic spline can overshoot
        // and dip between points; this must never happen here
        let spline = MonotoneCubicSpline::fit(vec![
            (0.0, 1.0), (1.0, 1.01), (2.0, 5.0), (3.0, 5.01), (4.0, 10.0),
        ]).unwrap();

        let mut prev = spline.eval(0.0).unwrap();
        let mut x = 0.0;
        while x <= 4.0 {
            let y = spline.eval(x).unwrap();
            assert!(y >= prev - 1e-9, "overshoot detected: y({x})={y} < prev={prev}");
            prev = y;
            x += 0.01;
        }
    }

    #[test]
    fn linear_data_interpolates_linearly() {
        // a straight line's monotone cubic interpolant must equal the line
        // itself everywhere (tangents all equal the constant secant)
        let spline = MonotoneCubicSpline::fit(vec![(0.0, 0.0), (1.0, 2.0), (2.0, 4.0), (3.0, 6.0)]).unwrap();
        for i in 0..=30 {
            let x = i as f64 * 0.1;
            let y = spline.eval(x).unwrap();
            assert!((y - 2.0 * x).abs() < 1e-9, "expected {}, got {} at x={}", 2.0 * x, y, x);
        }
    }

    #[test]
    fn insufficient_points_rejected() {
        assert_eq!(MonotoneCubicSpline::fit(vec![(0.0, 1.0)]).unwrap_err(), CurveError::InsufficientPoints(1));
        assert_eq!(MonotoneCubicSpline::fit(vec![]).unwrap_err(), CurveError::InsufficientPoints(0));
    }

    #[test]
    fn duplicate_x_rejected() {
        let err = MonotoneCubicSpline::fit(vec![(1.0, 1.0), (1.0, 2.0)]).unwrap_err();
        assert_eq!(err, CurveError::DuplicateMaturity(1));
    }

    #[test]
    fn unsorted_input_is_sorted_before_fitting() {
        let spline = MonotoneCubicSpline::fit(vec![(2.0, 3.0), (0.0, 1.0), (1.0, 2.0)]).unwrap();
        assert_eq!(spline.eval(0.0), Some(1.0));
        assert_eq!(spline.eval(1.0), Some(2.0));
        assert_eq!(spline.eval(2.0), Some(3.0));
    }

    #[test]
    fn non_finite_rate_rejected() {
        let err = MonotoneCubicSpline::fit(vec![(0.0, f64::NAN), (1.0, 1.0)]).unwrap_err();
        assert!(matches!(err, CurveError::NonFiniteRate(_)));
    }

    #[test]
    fn local_extremum_does_not_overshoot_past_the_peak() {
        // a local max in the middle: naive cubic splines commonly overshoot
        // above the peak value just before/after it
        let spline = MonotoneCubicSpline::fit(vec![(0.0, 0.0), (1.0, 10.0), (2.0, 0.0)]).unwrap();
        let mut x = 0.0;
        while x <= 2.0 {
            let y = spline.eval(x).unwrap();
            assert!(y <= 10.0 + 1e-9, "overshoot past local max: y({x})={y}");
            x += 0.01;
        }
    }
}
