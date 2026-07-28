use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use crate::error::MathError;
use crate::math::{mul_div_approx, u128_wide_mul};

/// Protocol fixed-point type. 1.0 == 1e18.
/// i128 because rates can go negative (perp funding is a real place).
///
/// Matches @pendle/boros-offchain-math's FixedX18.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FixedX18(pub i128);

impl FixedX18 {
    pub const SCALE: i128 = 1_000_000_000_000_000_000; // 1e18

    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(Self::SCALE);
    pub const NEG_ONE: Self = Self(-Self::SCALE);

    // ── constructors ──────────────────────────────────────────────────────────

    #[inline]
    pub const fn raw(v: i128) -> Self {
        Self(v)
    }

    /// Scale an integer up to FixedX18. Panics on overflow.
    #[inline]
    pub fn from_integer(v: i128) -> Self {
        Self(v.checked_mul(Self::SCALE).expect("FixedX18::from_integer overflow"))
    }

    /// Lossy f64 → FixedX18. For display/debug/config parsing ONLY.
    /// Don't use this anywhere on the hot path.
    #[inline]
    pub fn from_f64(v: f64) -> Self {
        debug_assert!(v.is_finite(), "tried to create FixedX18 from {v}");
        Self((v * Self::SCALE as f64).round() as i128)
    }

    // ── accessors ─────────────────────────────────────────────────────────────

    #[inline]
    pub const fn inner(self) -> i128 {
        self.0
    }

    /// Lossy FixedX18 → f64. OK for logging, shadow risk, not for anything
    /// that needs to match on-chain precision.
    #[inline]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / Self::SCALE as f64
    }

    #[inline]
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    #[inline]
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    #[inline]
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }

    #[inline]
    pub fn signum(self) -> i128 {
        self.0.signum()
    }

    #[inline]
    pub fn max(self, other: Self) -> Self {
        if self.0 >= other.0 { self } else { other }
    }

    #[inline]
    pub fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }

    // ── checked exact arithmetic ───────────────────────────────────────────────

    #[inline]
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Self)
    }

    #[inline]
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.0.checked_sub(rhs.0).map(Self)
    }

    // ── compound arithmetic (mul/div) ─────────────────────────────────────────
    //
    // f64-based for now. ~15 significant digits. For large notionals * high
    // rates we lose ~2-3 ULPs at the 18th decimal place, acceptable for a
    // shadow margin sim where the chain is the source of truth.
    //
    // Exact arithmetic now exists (see `mul_floor`/`mul_ceil` above, and
    // `math::mul_div_up`/`mul_div_down` for the unsigned-magnitude variants
    // used by fee accounting), mirroring PMath.sol's actual surface, which
    // has no single "neutral" rounding mode either. `mul_fixed`/`div_fixed`
    // stay as-is: existing callers (e.g. margin-sim's `Position::value`)
    // use the exact-rounding variants directly where it matters, and
    // migrating remaining callers changes their rounding behavior, not
    // something to do silently.

    /// (self * rhs) / 1e18, the standard FixedX18 multiplication.
    #[inline]
    pub fn mul_fixed(self, rhs: Self) -> Result<Self, MathError> {
        mul_div_approx(self.0, rhs.0, Self::SCALE).map(Self)
    }

    /// (self * rhs) / 1e18, panics on overflow. Use only where you've
    /// already validated the inputs are within a safe range.
    #[inline]
    pub fn mul_fixed_unchecked(self, rhs: Self) -> Self {
        self.mul_fixed(rhs).expect("mul_fixed overflow")
    }

    /// (self * scalar) / 1e18 where scalar is a plain i128 (not scaled).
    /// This is for multiplying a FixedX18 rate by a notional that's already
    /// in FixedX18 scale, same as mul_fixed but named differently to be explicit.
    #[inline]
    pub fn mul_raw(self, raw_rhs: i128) -> Result<Self, MathError> {
        mul_div_approx(self.0, raw_rhs, Self::SCALE).map(Self)
    }

    /// (self * rhs) / 1e18, exact, rounded toward -infinity.
    ///
    /// Mirrors `PMath.mulFloor(int256,int256)`. Use this, not `mul_fixed`
    ///, anywhere the result is real money reconciled against on-chain
    /// state, e.g. the floating-payment leg of an IRS settlement, where the
    /// contract's own rounding direction has to be replicated bit-for-bit,
    /// not merely approximated.
    #[inline]
    pub fn mul_floor(self, rhs: Self) -> Result<Self, MathError> {
        crate::math::mul_div_floor(self.0, rhs.0, Self::SCALE).map(Self)
    }

    /// (self * rhs) / 1e18, exact, rounded toward +infinity.
    ///
    /// Mirrors `PMath.mulCeil(int256,int256)`.
    #[inline]
    pub fn mul_ceil(self, rhs: Self) -> Result<Self, MathError> {
        crate::math::mul_div_ceil(self.0, rhs.0, Self::SCALE).map(Self)
    }

    /// (self * rhs) / 1e18, exact, rounded toward zero (truncating, not
    /// floor, see `mul_div_trunc`'s doc comment for why those differ for
    /// negative results). Mirrors `PMath.mulDown(int256,int256)`.
    #[inline]
    pub fn mul_down(self, rhs: Self) -> Result<Self, MathError> {
        crate::math::mul_div_trunc(self.0, rhs.0, Self::SCALE).map(Self)
    }

    /// Widening-safe inner product: returns the 256-bit magnitude of self.0 * rhs.0.
    /// Useful for overflow detection before dividing.
    #[inline]
    pub fn widening_inner(self, rhs: Self) -> (u128, u128) {
        u128_wide_mul(self.0.unsigned_abs(), rhs.0.unsigned_abs())
    }

    /// Division: (self * 1e18) / rhs.
    #[inline]
    pub fn div_fixed(self, rhs: Self) -> Result<Self, MathError> {
        if rhs.is_zero() {
            return Err(MathError::DivisionByZero);
        }
        mul_div_approx(self.0, Self::SCALE, rhs.0).map(Self)
    }
}

// ── operator impls ─────────────────────────────────────────────────────────────

impl Add for FixedX18 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0.wrapping_add(rhs.0))
    }
}

impl AddAssign for FixedX18 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.wrapping_add(rhs.0);
    }
}

impl Sub for FixedX18 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self(self.0.wrapping_sub(rhs.0))
    }
}

impl SubAssign for FixedX18 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.0 = self.0.wrapping_sub(rhs.0);
    }
}

impl Neg for FixedX18 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl fmt::Debug for FixedX18 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FixedX18({} ≈ {:.8})", self.0, self.to_f64())
    }
}

impl fmt::Display for FixedX18 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.8}", self.to_f64())
    }
}

impl From<i64> for FixedX18 {
    fn from(v: i64) -> Self {
        Self::from_integer(v as i128)
    }
}

impl From<i32> for FixedX18 {
    fn from(v: i32) -> Self {
        Self::from_integer(v as i128)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for FixedX18 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // serialize as string to preserve full i128 precision over JSON
        s.serialize_str(&self.0.to_string())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for FixedX18 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<i128>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_integer_roundtrip() {
        let x = FixedX18::from_integer(5);
        assert_eq!(x.inner(), 5 * FixedX18::SCALE);
    }

    #[test]
    fn add_sub_exact() {
        let a = FixedX18::from_f64(1.5);
        let b = FixedX18::from_f64(0.5);
        assert_eq!((a + b).to_f64(), 2.0);
        assert_eq!((a - b).to_f64(), 1.0);
    }

    #[test]
    fn mul_fixed_basic() {
        // 0.1 * 0.1 = 0.01
        let a = FixedX18::from_f64(0.1);
        let result = a.mul_fixed(a).unwrap();
        let diff = (result.to_f64() - 0.01).abs();
        assert!(diff < 1e-12, "expected ~0.01, got {}", result.to_f64());
    }

    #[test]
    fn neg_rate_arithmetic() {
        let r = FixedX18::from_f64(-0.05);
        assert!(r.is_negative());
        assert_eq!((-r).to_f64(), 0.05);
    }

    #[test]
    fn mul_floor_vs_mul_ceil_differ_on_negative_remainder() {
        // raw(-1) * raw(1): floor must give -1, ceil must give 0, these
        // MUST diverge here, that's the entire point of having both.
        let x = FixedX18::raw(-1);
        let y = FixedX18::raw(1);
        assert_eq!(x.mul_floor(y).unwrap(), FixedX18::raw(-1));
        assert_eq!(x.mul_ceil(y).unwrap(), FixedX18::raw(0));
    }

    #[test]
    fn mul_floor_matches_exact_expectation() {
        // -1.5 * 0.1 = -0.15 exactly (no remainder, floor and ceil agree)
        let x = FixedX18::from_f64(-1.5);
        let y = FixedX18::from_f64(0.1);
        assert_eq!(x.mul_floor(y).unwrap(), FixedX18::raw(-150_000_000_000_000_000));
        assert_eq!(x.mul_ceil(y).unwrap(), FixedX18::raw(-150_000_000_000_000_000));
    }
}
