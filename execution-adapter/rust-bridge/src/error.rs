use thiserror::Error;

use crate::error_class::ErrorClass;

#[derive(Debug, Error)]
pub enum BridgeError {
    /// The request was rejected by the Boros backend/contract. Retrying
    /// the identical request is either pointless (`Fatal`) or of unknown
    /// safety (`Unknown`). See `error_class` module docs.
    #[error("execution rejected [{code}] ({class:?}): {message}")]
    Rejected { code: String, message: String, class: ErrorClass },

    /// Retried up to the configured limit, all attempts failed.
    #[error("exhausted {attempts} attempts, last error: {last}")]
    RetriesExhausted { attempts: u32, last: String },

    /// gRPC transport-level failure (connection, TLS, etc.), distinct from
    /// an application-level rejection.
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// sidecar-ts returned something this client couldn't parse into a
    /// valid domain value (bad FixedX18/OrderId string, unspecified enum).
    #[error("invalid response from sidecar: {0}")]
    InvalidResponse(String),
}
