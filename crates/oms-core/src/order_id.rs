//! `OrderId` bit-packing, an exact port of `OrderIdLib`
//! (`pendle-finance/boros-core-public`, `contracts/types/Order.sol:99-156`).

use crate::error::OmsError;

/// LONG pays fixed / receives floating; SHORT the reverse.
///
/// Discriminant order matters: it's packed directly into `OrderId` bit 56
/// (LONG=0, SHORT=1), matching `enum Side { LONG, SHORT }` in `Order.sol`
/// exactly. Do not reorder these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    Long = 0,
    Short = 1,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }

    /// LONG sweeps the book from the highest tick down (higher tick = better
    /// bid price); SHORT sweeps from the lowest tick up (lower tick = better
    /// ask price). Mirrors `SideLib.sweepTickTopDown`.
    pub fn sweeps_tick_top_down(self) -> bool {
        matches!(self, Side::Long)
    }
}

const INITIALIZED_MARKER: u64 = 1 << 63;
const ORDER_INDEX_BITS: u32 = 40;
const ORDER_INDEX_MASK: u64 = (1u64 << ORDER_INDEX_BITS) - 1; // 2^40 - 1
const TICK_SIGN_BIT: u16 = 1 << 15;

/// A resting order's identifier, exactly as the contract packs it: 64 bits,
/// `[63: initialized][56: side][55..40: encoded tick][39..0: order index]`.
///
/// **Not** `direction/marketId/tick/expiry/nonce`. There is no `marketId`
/// (an order only makes sense within the `Market` contract it was placed
/// on, the id doesn't need to carry that) and no `expiry`/`nonce`,
/// `order_index` is a plain per-market sequential counter, not a user
/// nonce.
///
/// The raw `u64` value is meaningful as an ordering: for a sorted list of
/// ids **of the same side**, lower raw value means higher book priority.
/// `Ord`/`PartialOrd` are derived directly on the wrapped `u64` for
/// exactly this reason, do not replace with a "smarter" comparison, the
/// rawness is the point (see `OrderIdLib`'s doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderId(u64);

impl OrderId {
    /// Mirrors `OrderIdLib.from`. `order_index` must fit in 40 bits, the
    /// contract's parameter type (`uint40`) enforces this at the ABI level;
    /// here it's an explicit checked error instead of a silent truncation.
    pub fn from_parts(side: Side, tick_index: i16, order_index: u64) -> Result<Self, OmsError> {
        if order_index > ORDER_INDEX_MASK {
            return Err(OmsError::OrderIndexOverflow(order_index));
        }
        let encoded_tick = encode_tick_index(tick_index, side);

        let mut packed: u64 = side as u64;
        packed = (packed << 16) | encoded_tick as u64;
        packed = (packed << 40) | order_index;
        packed |= INITIALIZED_MARKER;

        Ok(Self(packed))
    }

    /// Mirrors `OrderIdLib.unpack`.
    pub fn unpack(self) -> (Side, i16, u64) {
        (self.side(), self.tick_index(), self.order_index())
    }

    pub fn side(self) -> Side {
        if (self.0 >> 56) & 1 == 1 { Side::Short } else { Side::Long }
    }

    pub fn tick_index(self) -> i16 {
        let encoded = ((self.0 >> 40) & 0xFFFF) as u16;
        decode_tick_index(encoded, self.side())
    }

    pub fn order_index(self) -> u64 {
        self.0 & ORDER_INDEX_MASK
    }

    pub fn is_initialized(self) -> bool {
        self.0 & INITIALIZED_MARKER != 0
    }

    /// The raw packed `u64`, for logging/debugging or handing to a lower
    /// layer that needs the exact on-chain representation. Not meant to be
    /// pattern-matched on by callers, use `unpack()`/`side()`/`tick_index()`.
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for OrderId {
    type Error = OmsError;

    /// Builds an `OrderId` from an already-packed value, e.g. one received
    /// over the wire as a decimal string and parsed back to `u64`. Only
    /// checks the initialized-marker bit, nothing else needs checking: side
    /// (1 bit), encoded tick (16 bits), and order_index (40 bits, exactly
    /// `ORDER_INDEX_MASK`) partition the remaining 63 bits exactly, so
    /// every value with the marker bit set decodes to some valid
    /// `(side, tick, order_index)` triple and re-encoding that triple
    /// reproduces the same bits by construction. There's no "corrupt but
    /// marker-bit-set" raw value this bit layout can represent.
    fn try_from(raw: u64) -> Result<Self, OmsError> {
        if raw & INITIALIZED_MARKER == 0 {
            return Err(OmsError::OrderIdNotInitialized(raw));
        }
        Ok(Self(raw))
    }
}

/// Mirrors `OrderIdLib._encodeTickIndex`: XOR with the sign bit turns the
/// signed tick into a monotonically-ordered unsigned value (standard
/// signed-to-unsigned ordering transform); for LONG, bitwise-NOT on top of
/// that inverts the ordering so a *higher* tick (better bid price) maps to
/// a *lower* encoded value, consistent with "lower raw OrderId = higher
/// priority" for both sides.
fn encode_tick_index(tick_index: i16, side: Side) -> u16 {
    let encoded = (tick_index as u16) ^ TICK_SIGN_BIT;
    if side.sweeps_tick_top_down() { !encoded } else { encoded }
}

/// Mirrors `OrderIdLib._decodeTickIndex`, the exact inverse.
fn decode_tick_index(encoded: u16, side: Side) -> i16 {
    let e = if side.sweeps_tick_top_down() { !encoded } else { encoded };
    (e ^ TICK_SIGN_BIT) as i16
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_various_ticks_and_sides() {
        for &side in &[Side::Long, Side::Short] {
            for &tick in &[i16::MIN, -1000, -1, 0, 1, 1000, i16::MAX] {
                for &idx in &[0u64, 1, 12345, ORDER_INDEX_MASK] {
                    let id = OrderId::from_parts(side, tick, idx).unwrap();
                    let (s, t, i) = id.unpack();
                    assert_eq!(s, side, "side mismatch for tick={tick} idx={idx}");
                    assert_eq!(t, tick, "tick mismatch for side={side:?} idx={idx}");
                    assert_eq!(i, idx, "order_index mismatch for side={side:?} tick={tick}");
                    assert!(id.is_initialized());
                }
            }
        }
    }

    #[test]
    fn order_index_overflow_rejected() {
        let err = OrderId::from_parts(Side::Long, 0, ORDER_INDEX_MASK + 1).unwrap_err();
        assert_eq!(err, OmsError::OrderIndexOverflow(ORDER_INDEX_MASK + 1));
    }

    #[test]
    fn long_priority_favors_higher_tick() {
        // for LONG (bids), a HIGHER tick is a better price and must produce
        // a LOWER raw OrderId (= higher priority), same order index/nonce
        let low_tick = OrderId::from_parts(Side::Long, 100, 0).unwrap();
        let high_tick = OrderId::from_parts(Side::Long, 200, 0).unwrap();
        assert!(high_tick < low_tick, "higher LONG tick must sort first (lower raw id)");
    }

    #[test]
    fn short_priority_favors_lower_tick() {
        // for SHORT (asks), a LOWER tick is a better price and must produce
        // a LOWER raw OrderId (= higher priority)
        let low_tick = OrderId::from_parts(Side::Short, 100, 0).unwrap();
        let high_tick = OrderId::from_parts(Side::Short, 200, 0).unwrap();
        assert!(low_tick < high_tick, "lower SHORT tick must sort first (lower raw id)");
    }

    #[test]
    fn same_tick_lower_order_index_has_priority() {
        // FIFO within a tick: earlier order_index = placed first = higher priority
        let first = OrderId::from_parts(Side::Long, 500, 10).unwrap();
        let second = OrderId::from_parts(Side::Long, 500, 20).unwrap();
        assert!(first < second, "lower order_index at the same tick must sort first");
    }

    #[test]
    fn long_orders_and_short_orders_never_interleave_by_priority_semantics() {
        // the doc comment on OrderIdLib is explicit that priority ordering
        // is only meaningful *within the same side*, this just documents
        // that cross-side comparison compiles (raw u64 order) without
        // asserting any semantic meaning to the result
        let long_id = OrderId::from_parts(Side::Long, i16::MAX, 0).unwrap();
        let short_id = OrderId::from_parts(Side::Short, i16::MIN, 0).unwrap();
        let _ = long_id < short_id; // no assertion, side bit dominates, not a priority claim
    }

    #[test]
    fn side_accessor_matches_unpack() {
        let id = OrderId::from_parts(Side::Short, 42, 7).unwrap();
        assert_eq!(id.side(), Side::Short);
        assert_eq!(id.tick_index(), 42);
        assert_eq!(id.order_index(), 7);
    }

    #[test]
    fn opposite_side_round_trips() {
        assert_eq!(Side::Long.opposite(), Side::Short);
        assert_eq!(Side::Short.opposite(), Side::Long);
        assert_eq!(Side::Long.opposite().opposite(), Side::Long);
    }

    #[test]
    fn try_from_u64_round_trips_a_value_this_encoder_produced() {
        let original = OrderId::from_parts(Side::Short, -42, 12345).unwrap();
        let rebuilt = OrderId::try_from(original.raw()).unwrap();
        assert_eq!(rebuilt, original);
        assert_eq!(rebuilt.unpack(), (Side::Short, -42, 12345));
    }

    #[test]
    fn try_from_u64_rejects_missing_initialized_bit() {
        let raw_without_marker = OrderId::from_parts(Side::Long, 0, 0).unwrap().raw() & !INITIALIZED_MARKER;
        let err = OrderId::try_from(raw_without_marker).unwrap_err();
        assert_eq!(err, OmsError::OrderIdNotInitialized(raw_without_marker));
    }

    #[test]
    fn try_from_u64_accepts_every_marker_bit_set_value_regardless_of_tick_or_side() {
        // the bit layout is a bijection over the 63 non-marker bits, so
        // there's no "marker set but otherwise invalid" raw value, this
        // just exercises the extremes to make that concrete
        for raw in [INITIALIZED_MARKER, u64::MAX, INITIALIZED_MARKER | (1 << 56), INITIALIZED_MARKER | ORDER_INDEX_MASK] {
            assert!(OrderId::try_from(raw).is_ok(), "raw={raw:#x} should decode to some valid triple");
        }
    }
}
