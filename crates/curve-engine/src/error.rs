use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum CurveError {
    #[error("need at least 2 points to fit a curve, got {0}")]
    InsufficientPoints(usize),

    #[error("duplicate time_to_maturity_secs={0} in curve points, aggregate before fitting")]
    DuplicateMaturity(u32),

    #[error("non-finite rate at time_to_maturity_secs={0} (NaN or Inf), check upstream data")]
    NonFiniteRate(u32),

    #[error("query time {0}s is outside the curve's domain [{1}, {2}]s, this curve does not extrapolate")]
    OutsideDomain(u32, u32, u32),
}
