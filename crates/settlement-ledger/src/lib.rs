//! Off-chain replica of Boros lazy settlement, verified against
//! `pendle-finance/boros-core-public` source (BUSL-1.1), not inferred.
//!
//! Boros settles the floating leg lazily: each market publishes an `FIndex`
//! (`{ FTag, floatingIndex, feeIndex }`) at discrete, `FTag`-indexed events
//! (an oracle update, or a force-cancel "purge", see `FTagLib` in
//! `contracts/types/MarketTypes.sol`). A position stores the last `FIndex`
//! it synced against; settling walks the `FTag`s since then in order,
//! pricing each sub-period at the size held *before* that period's fills,
//! per `contracts/lib/PaymentLib.sol::calcSettlement` and
//! `contracts/core/market/settle/ProcessMergeUtils.sol::__processSweptUntilStop`.
//!
//! Without this, `total_value` in `margin-sim` silently drifts from
//! on-chain state as floating payments accrue unaccounted-for.
//!
//! ## Scope
//! This crate owns the settlement *math* for the floating leg + protocol
//! fee: given a timeline of fills and a timeline of exact `FIndex` records
//! (both keyed by `FTag`, never a timestamp), compute the payment/fee for
//! a window. It does not:
//! - source `FIndex` records (no oracle/event feed exists yet in this
//!   workspace; production sourcing is the market's `FIndexUpdated` event
//!   log or the equivalent Boros REST API field)
//! - know about `feed-ingest::TradeEvent` or any wire format (kept
//!   decoupled so this crate isn't dragged into `feed-ingest`'s toolchain
//!   requirement)
//! - compute the upfront fixed-leg cost (`PaymentLib.calcUpfrontFixedCost`)
//!   paid at fill time from the trade's own cost, a separate payment
//!   stream that belongs with fill/order processing (`oms-core`), not here
//! - decide *when* to call `settle_to` (the caller's job, e.g. mm-bot's
//!   event loop, or a reconciliation job)
//!
//! Never interpolates. If a required `FIndex` record for a specific `FTag`
//! isn't known, `settle_to` returns `LedgerError::MissingFIndexRecord`
//! instead of approximating one. The real contract never approximates
//! either, it reads an exact mapping (`fTagToIndex[fTag]`,
//! `contracts/core/market/core/MarketInfoAndState.sol::_toFIndex`).

mod error;
mod ledger;
mod types;

pub use error::LedgerError;
pub use ledger::SettlementLedger;
pub use types::{Fill, FIndexRecord, PayFee, SettlementResult, SubPeriod};
