//! Avellaneda-Stoikov market making adapted to Boros: DV01 inventory (not
//! contract count), rate space (not price), zero-maker-fee spread economics
//! (`Mechanics/Fees.md`), and hard clamping against the real on-chain
//! maker rate bounds (see `types.rs`, matches `MarginViewUtils.sol::_calcRateBound`).
//!
//! Core reservation-price/spread model from Avellaneda-Stoikov (2008) and
//! the Guéant-Lehalle-Fernandez-Tapia (2012) closed form; see the
//! `reservation` module for the funding-aware coupling term adapted from
//! arXiv:2605.06405.
//!
//! Depends on `curve-engine` only for the `Curve` type as a *suggested*
//! reference-rate source in doc comments, `QuotingEngine::quote` takes a
//! plain `FixedX18` reference rate, so any source works. Depends on
//! `oms-core` for `Side`, used by the rate-bound calc above.

mod bounds;
mod engine;
mod error;
mod reservation;
mod types;

pub use engine::QuotingEngine;
pub use error::QuoteError;
pub use reservation::{optimal_spread, reservation_rate};
pub use types::{AvellanedaStoikovParams, InventoryState, MakerRateBounds, Quote};
