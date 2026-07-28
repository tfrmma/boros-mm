//! Rust-side gRPC client for `sidecar-ts`, the Node/TS process that wraps
//! the official `@pendle/sdk-boros` SDK. This crate owns retry/backoff and
//! error classification; it never signs anything or encodes calldata
//! itself, same discipline `sidecar-ts` follows on its own side (delegating
//! that to the official SDK instead of hand-rolling it).

pub mod client;
pub mod error;
pub mod error_class;
pub mod types;

pub use client::{ExecutionClient, PlaceOrderOutcome, RetryConfig};
pub use error::BridgeError;
pub use error_class::{classify, ErrorClass};

pub mod proto {
    tonic::include_proto!("boros.execution.v1");
}
