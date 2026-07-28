use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq)]
pub enum MathError {
    #[error("division by zero")]
    DivisionByZero,
    #[error("overflow")]
    Overflow,
    #[error("invalid tick step: {0}")]
    InvalidTickStep(u8),
    #[error("invalid tick: {0}")]
    InvalidTick(i16),
    #[error("rate not representable as tick: {0}")]
    RateNotRepresentable(f64),
}
