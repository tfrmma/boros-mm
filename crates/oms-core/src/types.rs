//! Types mirroring `types/MarketTypes.sol`, `types/Order.sol`, and
//! `types/Trade.sol` in `pendle-finance/boros-core-public`, verified
//! against source. These are semantic (Rust-native field types), not
//! bit-exact on-chain encodings, encoding calldata is the official SDK's
//! job (`execution-adapter`/`rust-bridge`), not this crate's.

use tick_math::FixedX18;

use crate::order_id::{OrderId, Side};

// ── order lifecycle ──────────────────────────────────────────────────────────

/// Mirrors `enum TimeInForce` in `Order.sol` exactly (`GTC, IOC, FOK, ALO,
/// SOFT_ALO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    Alo,
    SoftAlo,
}

impl TimeInForce {
    pub fn is_alo(self) -> bool {
        matches!(self, TimeInForce::Alo | TimeInForce::SoftAlo)
    }

    /// Mirrors `TimeInForceLib.shouldSkipMatchableOrders`: `SOFT_ALO` orders
    /// that would cross the book are dropped entirely instead of matched
    /// or resting at a worse price.
    pub fn should_skip_matchable_orders(self) -> bool {
        matches!(self, TimeInForce::SoftAlo)
    }
}

/// On-chain order status. Exactly 4 states, **not** the richer
/// `Pending/Resting/PartiallyFilled/Filled/Cancelled/Purged` naming a
/// naive read might suggest. Verified against `Order.sol:15-20` +
/// `MarketOffView.sol`. There is no on-chain "Cancelled" status
/// distinguishable from "never existed" once an order is removed:
/// `PURGED` is specifically the force-cancel ("purge") outcome; a
/// user-initiated cancel just removes the order from storage. See
/// `LocalOrderStatus` for the richer, off-chain-tracked state this crate
/// maintains instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    NotExist,
    Open,
    PendingSettle,
    Purged,
}

/// This crate's own, richer order lifecycle, reconstructed from the 5 real
/// lifecycle events (`LimitOrderPlaced/Filled/PartiallyFilled/Cancelled/
/// ForcedCancelled` in `IMarket.sol:224-232`) instead of mirroring
/// `OrderStatus` 1:1, the whole point of tracking this locally is to keep
/// the distinction the contract itself doesn't retain post-hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOrderStatus {
    /// Resting, no fills yet.
    Open,
    /// Resting, partially filled, `remaining_size() > 0`.
    PartiallyFilled,
    /// Fully filled. Terminal.
    Filled,
    /// Cancelled by the user (or removed via `CancelData`). Terminal.
    Cancelled,
    /// Force-cancelled by the protocol's out-of-band purge bot
    /// (`_bookPurgeOob`). Terminal.
    ForcedCancelled,
}

// ── trade / fill ─────────────────────────────────────────────────────────────

/// Mirrors `Trade`/`Fill` from `types/Trade.sol`, on-chain, both are the
/// same packed `(int128 signedSize, int128 signedCost)` layout (`Fill` is a
/// `Trade` for a single tick; a `Trade` can be the sum of several `Fill`s).
/// Represented here as a plain struct since this crate doesn't encode
/// calldata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    pub signed_size: FixedX18,
    pub signed_cost: FixedX18,
}

impl Trade {
    pub const ZERO: Self = Self { signed_size: FixedX18::ZERO, signed_cost: FixedX18::ZERO };

    pub fn side(&self) -> Side {
        if self.signed_size.is_positive() { Side::Long } else { Side::Short }
    }

    pub fn abs_size(&self) -> FixedX18 {
        self.signed_size.abs()
    }

    pub fn abs_cost(&self) -> FixedX18 {
        self.signed_cost.abs()
    }

    pub fn add(self, other: Self) -> Self {
        Self { signed_size: self.signed_size + other.signed_size, signed_cost: self.signed_cost + other.signed_cost }
    }

    pub fn opposite(self) -> Self {
        Self { signed_size: -self.signed_size, signed_cost: -self.signed_cost }
    }

    pub fn is_zero(&self) -> bool {
        self.signed_size.is_zero() && self.signed_cost.is_zero()
    }

    /// `TradeLib.fromSizeAndRate`: `signedCost = signedSize.mulDown(rate)`.
    /// Needed `mul_down` (the signed, truncate-toward-zero `PMath.mulDown`
    /// overload) added to `tick-math` first, distinct from `mul_floor`/
    /// `mul_ceil` and from the unsigned `mul_div_up`/`mul_div_down`, see
    /// `tick_math::mul_div_trunc`'s doc comment for why.
    pub fn from_size_and_rate(signed_size: FixedX18, rate: FixedX18) -> Result<Self, crate::error::OmsError> {
        let signed_cost = signed_size.mul_down(rate)?;
        Ok(Self { signed_size, signed_cost })
    }

    /// `TradeLib.from3`: same as `from_size_and_rate` but takes an unsigned
    /// magnitude and an explicit side instead of a pre-signed size.
    pub fn from3(side: Side, size: FixedX18, rate: FixedX18) -> Result<Self, crate::error::OmsError> {
        let signed_size = match side {
            Side::Long => size,
            Side::Short => -size,
        };
        Self::from_size_and_rate(signed_size, rate)
    }
}

// ── account ──────────────────────────────────────────────────────────────────

/// Mirrors `MarketAcc` (`types/Account.sol`): `address(160) | accountId(8) |
/// tokenId(16) | marketId(24)`, 208 bits total. Kept as separate named
/// fields instead of one packed integer, this crate never encodes
/// calldata, so there's nothing to gain from replicating the packing, only
/// bugs to risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarketAcc {
    pub root: [u8; 20],
    pub account_id: u8,
    /// On-chain `uint16`. `margin-sim::TokenId` currently wraps a wider
    /// `u32`, pre-existing, not reconciled here (out of this crate's scope).
    pub token_id: u16,
    /// On-chain `uint24`, only the low 24 bits are ever meaningful.
    pub market_id: u32,
}

// ── orderAndOtc request/response shapes ─────────────────────────────────────

/// Mirrors `struct LongShort` (`MarketTypes.sol:31-36`): a batch of orders
/// on ONE side, submitted together in one `orderAndOtc` call.
#[derive(Debug, Clone)]
pub struct LongShort {
    pub tif: TimeInForce,
    pub side: Side,
    pub sizes: Vec<FixedX18>,
    pub limit_ticks: Vec<i16>,
}

/// Mirrors `struct CancelData` (`MarketTypes.sol:38-42`).
#[derive(Debug, Clone, Default)]
pub struct CancelData {
    pub ids: Vec<OrderId>,
    pub is_all: bool,
    pub is_strict: bool,
}

/// Mirrors `struct OTCTrade` (`MarketTypes.sol:44-48`): a bilateral trade
/// against a specific counterparty at an agreed cash amount, alongside the
/// order-book flow in the same `orderAndOtc` call.
#[derive(Debug, Clone, Copy)]
pub struct OtcTrade {
    pub counter: MarketAcc,
    pub trade: Trade,
    pub cash_to_counter: FixedX18,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_size_and_rate_matches_manual_mul_down() {
        let size = FixedX18::from_f64(1000.0);
        let rate = FixedX18::from_f64(0.08);
        let trade = Trade::from_size_and_rate(size, rate).unwrap();
        assert_eq!(trade.signed_size, size);
        assert_eq!(trade.signed_cost, size.mul_down(rate).unwrap());
    }

    #[test]
    fn from_size_and_rate_truncates_toward_zero_not_floor() {
        // size=-1.5, rate=1 raw unit (1e-18): raw product / SCALE = -1.5
        // exactly, not an integer, needs rounding. trunc gives -1 (closer
        // to zero), floor would give -2, that's the whole distinction this
        // function exists for.
        let size = FixedX18::from_f64(-1.5);
        let rate = FixedX18::raw(1);
        let trade = Trade::from_size_and_rate(size, rate).unwrap();
        assert_eq!(trade.signed_cost, FixedX18::raw(-1));
        assert_eq!(size.mul_floor(rate).unwrap(), FixedX18::raw(-2));
    }

    #[test]
    fn from3_long_gives_positive_size() {
        let trade = Trade::from3(Side::Long, FixedX18::from_f64(500.0), FixedX18::from_f64(0.05)).unwrap();
        assert_eq!(trade.side(), Side::Long);
        assert!(trade.signed_size.is_positive());
        assert!(trade.signed_cost.is_positive());
    }

    #[test]
    fn from3_short_negates_both_size_and_cost() {
        let long = Trade::from3(Side::Long, FixedX18::from_f64(500.0), FixedX18::from_f64(0.05)).unwrap();
        let short = Trade::from3(Side::Short, FixedX18::from_f64(500.0), FixedX18::from_f64(0.05)).unwrap();
        assert_eq!(short.signed_size, -long.signed_size);
        assert_eq!(short.signed_cost, -long.signed_cost);
    }

    #[test]
    fn from3_matches_from_size_and_rate_for_long() {
        let size = FixedX18::from_f64(200.0);
        let rate = FixedX18::from_f64(0.03);
        assert_eq!(
            Trade::from3(Side::Long, size, rate).unwrap(),
            Trade::from_size_and_rate(size, rate).unwrap(),
        );
    }
}
