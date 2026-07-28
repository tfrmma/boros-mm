//! Multi-maturity implied-APR curve construction and relative-value signal
//! detection, per zone (group of markets sharing an underlying, see
//! `Zone`'s doc comment).
//!
//! **Not required by the protocol.** Boros doesn't publish or require a
//! term structure: each market's implied APR is independently discovered
//! by its own orderbook + AMM (`whitepapers/AMM.pdf`,
//! `(x+a)^t·y=k`). This crate exists purely as a strategy tool: a smoothed
//! reference curve for quoting, and a scanner for maturities that look
//! mispriced relative to their neighbors.
//!
//! Uses Fritsch-Carlson monotone cubic interpolation, adapted to Boros's
//! structure: there's no discount-factor bootstrapping here, because
//! there's nothing to bootstrap from. Every point is directly observed,
//! not implied from a more fundamental instrument.

mod curve;
mod error;
mod spline;
mod types;

pub use curve::Curve;
pub use error::CurveError;
pub use types::{ButterflySignal, CurvePoint, Zone};
