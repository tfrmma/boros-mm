use thiserror::Error;
use tick_math::MathError;

#[derive(Debug, Error, PartialEq)]
pub enum OmsError {
    #[error("order_index {0} exceeds the 40-bit range the contract allows (max {})", (1u64 << 40) - 1)]
    OrderIndexOverflow(u64),

    #[error("LimitOrderPlaced ids/sizes length mismatch: {ids} ids vs {sizes} sizes")]
    MismatchedPlacedLengths { ids: usize, sizes: usize },

    #[error("LimitOrderFilled range spans different side or tick: from={from:?} to={to:?}")]
    InvalidFillRange { from: crate::order_id::OrderId, to: crate::order_id::OrderId },

    #[error("LimitOrderFilled range end (order_index={0}) is before its start (order_index={1})")]
    InvertedFillRange(u64, u64),

    #[error(transparent)]
    Math(#[from] MathError),
}
