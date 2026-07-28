mod conversion;
mod error;
mod fixed;
mod math;

pub use conversion::{rate_to_tick, rate_to_tick_bracket, tick_to_rate, Rounding, TICK_MAX, TICK_MIN};
pub use error::MathError;
pub use fixed::FixedX18;
pub use math::{mul3_div_floor_u32, mul3_div_up_u32, mul_div_ceil, mul_div_down, mul_div_floor, mul_div_up};

/// Seconds in a year, as the protocol defines it: exactly `365 days`, no
/// leap-year adjustment. Source-verified: `PMath.sol` (`ONE_YEAR`/`IONE_YEAR`
/// in `pendle-finance/boros-core-public`), used to scale `timeToMaturity`
/// (always raw seconds on-chain, never pre-converted to a FixedX18 fraction)
/// in `PaymentLib.calcUpfrontFixedCost` and `calcPositionValue`.
pub const SECONDS_PER_YEAR: u32 = 365 * 24 * 60 * 60;

/// `PMath.ONE_MUL_YEAR`/`IONE_MUL_YEAR` = `1e18 * 365 days`. The divisor
/// `calcPositionValue`/`_calcMM`/`_calcIM` all use after a triple product of
/// two FixedX18-scaled values and one raw-seconds value. See
/// `mul3_div_floor_u32`'s doc comment for why that triple product needs a
/// single fused division instead of two chained ones.
pub const ONE_MUL_YEAR: i128 = FixedX18::SCALE * SECONDS_PER_YEAR as i128;
