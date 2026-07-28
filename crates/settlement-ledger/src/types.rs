use tick_math::FixedX18;

/// A fill applied to a position, tagged with the market's `FTag` at the
/// moment it was processed by the contract.
///
/// Source: `SweptF.fTag` in `contracts/types/MarketTypes.sol`, every fill
/// swept off a user's order list carries the `FTag` active when it filled.
/// This is NOT a timestamp. `FTag` is a `uint32` sequence counter over
/// market events (odd = an FIndex-oracle update, even = a force-cancel
/// "purge", see `FTagLib.isFIndexUpdate`/`isPurge`), and settlement walks
/// these tags in order, never interpolating between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub f_tag: u32,
    /// Signed. Long fill (paying fixed, receiving floating) is positive,
    /// short fill is negative.
    pub size_delta: FixedX18,
}

/// One published `FIndex`, exactly as the market contract stores it in
/// `fTagToIndex[fTag]` (`MarketInfoAndState.sol:_toFIndex`).
///
/// Source of truth for these values in production: the `FIndexUpdated`
/// event emitted by the market contract (`MarketInfoAndState.sol:109`), or
/// the equivalent field via the Boros REST API. There is no
/// interpolation path in this crate: if a required `f_tag` hasn't been
/// recorded, `settle_to` returns `LedgerError::MissingFIndexRecord`
/// instead of approximating it. The real contract never approximates either: it
/// reads `fTagToIndex[fTag]` directly, an exact mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FIndexRecord {
    pub f_tag: u32,
    /// Unix seconds. Informational only, not used in any settlement math
    /// (the contract's `calcSettlement` never reads `fTime` either).
    pub f_time: u64,
    /// `FIndex.floatingIndex()`, signed, FixedX18 (1e18) scale, packed as
    /// `int112` on-chain (`FIndexLib.floatingIndex`). The floating leg's
    /// cumulative accumulator.
    pub floating_index: FixedX18,
    /// `FIndex.feeIndex()`, unsigned, FixedX18 scale, packed as `uint64`
    /// on-chain (`FIndexLib.feeIndex`). The protocol fee's cumulative
    /// accumulator, distinct from the floating index and always
    /// monotonically non-decreasing (`FIndexOracle._calcNewFIndex` only
    /// ever adds to it via `PaymentLib.calcNewFeeIndex`).
    pub fee_index: FixedX18,
}

/// The two payment legs the real contract computes together in one
/// `calcSettlement` call (`PayFee` in `contracts/types/MarketTypes.sol`).
/// Kept as a pair here for the same reason: they're always produced
/// together and share the same `(last, current)` FIndex pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayFee {
    /// Floating-leg payment. Positive = the account receives cash,
    /// negative = the account owes cash. `mulFloor` rounding (rounds
    /// toward -infinity, i.e. always at least as unfavorable to the
    /// account as the true value, matches the contract exactly).
    pub payment: FixedX18,
    /// Protocol fee owed for this period. Always non-negative.
    /// `mulUp` rounding (always rounds in the protocol's favor).
    pub fee: FixedX18,
}

impl PayFee {
    pub const ZERO: Self = Self { payment: FixedX18::ZERO, fee: FixedX18::ZERO };

    pub fn add(self, other: Self) -> Self {
        Self { payment: self.payment + other.payment, fee: self.fee + other.fee }
    }
}

/// One sub-period of a settlement window: constant position size, bounded
/// by consecutive `FTag`s. Exposed for audit: this is the granularity you
/// diff against real `PaymentFromSettlement` events when reconciling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubPeriod {
    pub start_f_tag: u32,
    pub end_f_tag: u32,
    pub size_held: FixedX18,
    pub result: PayFee,
}

/// Result of a `settle_to` call for one market.
#[derive(Debug, Clone, PartialEq)]
pub struct SettlementResult {
    pub market_id: u32,
    pub start_f_tag: u32,
    pub end_f_tag: u32,
    pub total: PayFee,
    pub sub_periods: Vec<SubPeriod>,
}
