//! Two relative-value strategies, neither a riskless arbitrage despite the
//! crate name. See each signal type's doc comment for exactly what risk
//! remains:
//!
//! - `cross_venue`: Boros implied APR vs. a comparable CEX perp's expected
//!   funding, cash-and-carry-style relative value between two different
//!   funding-rate-realization mechanisms.
//! - `calendar_spread`: wraps `curve_engine::ButterflySignal` into a
//!   directional, DV01-neutral-sized trade (which side at the mid
//!   maturity, opposite at the wings, each wing sized to offset half the
//!   mid leg's DV01). The caller still picks the mid leg's own notional.

mod calendar_spread;
mod cross_venue;
mod types;

pub use calendar_spread::to_calendar_spread_trade;
pub use cross_venue::detect_cross_venue_signal;
pub use types::{CalendarSpreadTrade, CrossVenueObservation, CrossVenueSignal};
