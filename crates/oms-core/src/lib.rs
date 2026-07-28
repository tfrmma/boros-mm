//! Order management core, verified against `pendle-finance/boros-core-public`
//! source (BUSL-1.1): `types/Order.sol`, `types/Trade.sol`,
//! `types/MarketTypes.sol`, and
//! `core/market/{MarketOrderAndOtc,orderbook/OrderBookUtils}.sol`.
//!
//! ## Scope
//! - `order_id`: exact `OrderId` bit-packing/unpacking (side, tick, order
//!   index, no `marketId`, no `expiry`, no `nonce`).
//! - `types`: `TimeInForce`, on-chain `OrderStatus` (4 states) vs. this
//!   crate's own richer `LocalOrderStatus`, `Trade`/`Fill`, `MarketAcc`,
//!   and the `orderAndOtc` request/response shapes (`LongShort`,
//!   `CancelData`, `OtcTrade`).
//! - `tracker`: `OrderTracker`, local order state driven by the 5 real
//!   lifecycle events. Needed because the contract itself doesn't retain a
//!   "cancelled" vs. "never existed" distinction once an order is removed.
//! - `payment`: the upfront fixed-leg cost (`PaymentLib.calcUpfrontFixedCost`),
//!   out of `settlement-ledger`'s scope: paid at fill time from the
//!   trade's own cost, not part of the periodic floating settlement.
//!
//! Not in scope: submitting transactions, encoding calldata, or talking to
//! a WS/API transport. Those belong to `execution-adapter`, which goes
//! through the official SDK instead of hand-rolled contract calls.

pub mod error;
pub mod order_id;
pub mod payment;
pub mod tracker;
pub mod types;

pub use error::OmsError;
pub use order_id::{OrderId, Side};
pub use payment::{calc_upfront_fixed_cost, trade_upfront_fixed_cost};
pub use tracker::{LocalOrder, OrderTracker};
pub use types::{CancelData, LocalOrderStatus, LongShort, MarketAcc, OrderStatus, OtcTrade, TimeInForce, Trade};
