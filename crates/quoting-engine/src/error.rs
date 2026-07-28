use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum QuoteError {
    #[error("Avellaneda-Stoikov parameters invalid: {0}")]
    InvalidParams(String),

    #[error("no reference rate available for time_to_maturity_secs={0}, curve doesn't cover this maturity")]
    NoReferenceRate(u32),

    /// Defensive check, not expected to trigger in normal operation since
    /// the one-sided clamps in `bounds.rs` can't cross a positive spread
    /// (see the engine.rs test that documents this). Kept in case
    /// `optimal_spread` ever returns 0 or something upstream changes.
    #[error("computed bid/ask crossed after bounds clamping: bid={bid:?} ask={ask:?}, widen params or check bounds config")]
    CrossedAfterClamp { bid: f64, ask: f64 },

    #[error("tick conversion failed: {0}")]
    TickMath(#[from] tick_math::MathError),
}
