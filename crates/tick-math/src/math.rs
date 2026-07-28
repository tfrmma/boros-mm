use crate::error::MathError;

/// u128 × u128 → (hi, lo). Classic 4-limb schoolbook. Verified for a=b=u128::MAX.
///
/// This is here for when we finally need bit-exact FixedX18 mul. Right now only
/// used for overflow detection in widening_inner.
#[inline]
pub(crate) fn u128_wide_mul(a: u128, b: u128) -> (u128, u128) {
    // schoolbook with u64 limbs, O(4 muls), no branching
    let lo = a.wrapping_mul(b); // low 128 bits; wrapping is intentional and correct

    let a0 = a as u64 as u128;
    let a1 = (a >> 64) as u64 as u128;
    let b0 = b as u64 as u128;
    let b1 = (b >> 64) as u64 as u128;

    // partial products at bit-offsets [64..191]
    let (p1, c1) = (a0 * b1).overflowing_add(a1 * b0);
    // p1 sits at [64..191]; add the carry from low product
    let (p1, c2) = p1.overflowing_add(a0 * b0 >> 64);
    // carry bookkeeping, each overflow contributes 2^128 to the high word
    let carry = (c1 as u128) + (c2 as u128);

    let hi = a1 * b1 + (p1 >> 64) + (carry << 64);

    (hi, lo)
}

/// Approximate (a * b) / d via f64.
///
/// ~15.9 significant decimal digits. Good enough for shadow margin sim;
/// the on-chain contracts are the source of truth anyway.
///
/// TODO: replace with exact 256/128 integer division when we have the
///       correct Knuth D implementation. See mul_div_exact below (stub).
#[inline]
pub(crate) fn mul_div_approx(a: i128, b: i128, d: i128) -> Result<i128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }

    let result = (a as f64 * b as f64) / d as f64;

    if !result.is_finite() {
        return Err(MathError::Overflow);
    }
    // don't be clever with LLONG_MIN/MAX tricks here, just check
    const MAX_F: f64 = i128::MAX as f64;
    const MIN_F: f64 = i128::MIN as f64;
    if result > MAX_F || result < MIN_F {
        return Err(MathError::Overflow);
    }

    Ok(result as i128)
}

/// Divide the 256-bit unsigned magnitude `(hi, lo)` by `d`, returning
/// `(quotient, remainder)`.
///
/// This is a bit-serial binary long division (256 iterations of shift +
/// compare + conditional subtract), not the classic multi-limb Knuth
/// Algorithm D (which works in a higher radix, e.g. base 2^32, for speed).
/// It is exact, mathematically equivalent in correctness, just not the same
/// algorithm the original TODO named. The tradeoff is deliberate: this runs
/// at most once per settlement/margin event, nowhere near a hot path, so a
/// simpler algorithm that's easy to fully verify beats a faster one that's
/// harder to audit. If a genuinely hot-path caller shows up later (e.g.
/// per-tick quoting math), revisit with a real Knuth D or a vetted
/// external bignum crate instead of hand-rolling one under time pressure.
fn div_u256_by_u128(hi: u128, lo: u128, d: u128) -> Result<(u128, u128), MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }

    let mut remainder: u128 = 0;
    let mut quotient_hi: u128 = 0;
    let mut quotient_lo: u128 = 0;

    for i in (0..256).rev() {
        let bit = if i >= 128 { (hi >> (i - 128)) & 1 } else { (lo >> i) & 1 };
        remainder = (remainder << 1) | bit;

        if remainder >= d {
            remainder -= d;
            if i >= 128 {
                quotient_hi |= 1u128 << (i - 128);
            } else {
                quotient_lo |= 1u128 << i;
            }
        }
    }

    if quotient_hi != 0 {
        return Err(MathError::Overflow);
    }
    Ok((quotient_lo, remainder))
}

/// `floor(a*b/d)`, exact, for signed `i128` operands. `d` must be positive
/// (every real call site, `IONE`, `IONE_YEAR`, `IONE_MUL_YEAR`, is a
/// positive constant; this is not a general signed-divisor division routine).
///
/// Mirrors `PMath.mulFloor(int256,int256) = rawDivFloor(x*y, IONE)`
/// generalized to an arbitrary positive `d`, source-verified against
/// `pendle-finance/boros-core-public/contracts/lib/math/PMath.sol`:
/// `rawDivFloor` truncates toward zero (`sdiv`), then subtracts 1 iff the
/// product's sign is negative (`x` and `d` differ in sign) and there's a
/// nonzero remainder, i.e. true floor, not truncation.
pub fn mul_div_floor(a: i128, b: i128, d: i128) -> Result<i128, MathError> {
    if d <= 0 {
        return Err(MathError::DivisionByZero);
    }
    if a == 0 || b == 0 {
        return Ok(0);
    }

    let neg = (a < 0) ^ (b < 0);
    let (hi, lo) = u128_wide_mul(a.unsigned_abs(), b.unsigned_abs());
    let (q, r) = div_u256_by_u128(hi, lo, d.unsigned_abs())?;
    let q = i128::try_from(q).map_err(|_| MathError::Overflow)?;

    if !neg {
        Ok(q)
    } else if r == 0 {
        q.checked_neg().ok_or(MathError::Overflow)
    } else {
        q.checked_neg().and_then(|v| v.checked_sub(1)).ok_or(MathError::Overflow)
    }
}

/// `ceil(a*b/d)`, exact, for signed `i128` operands, `d > 0`.
///
/// Mirrors `PMath.mulCeil(int256,int256) = rawDivCeil(x*y, IONE)`: truncate
/// toward zero, then add 1 iff the product's sign is non-negative and
/// there's a nonzero remainder.
pub fn mul_div_ceil(a: i128, b: i128, d: i128) -> Result<i128, MathError> {
    if d <= 0 {
        return Err(MathError::DivisionByZero);
    }
    if a == 0 || b == 0 {
        return Ok(0);
    }

    let neg = (a < 0) ^ (b < 0);
    let (hi, lo) = u128_wide_mul(a.unsigned_abs(), b.unsigned_abs());
    let (q, r) = div_u256_by_u128(hi, lo, d.unsigned_abs())?;
    let q = i128::try_from(q).map_err(|_| MathError::Overflow)?;

    if neg {
        q.checked_neg().ok_or(MathError::Overflow)
    } else if r == 0 {
        Ok(q)
    } else {
        q.checked_add(1).ok_or(MathError::Overflow)
    }
}

/// `trunc(a*b/d)` (round toward zero), exact, for signed `i128` operands,
/// `d > 0`.
///
/// Mirrors `PMath.mulDown(int256,int256) = sdiv(x*y, IONE)`, source-verified
/// against `pendle-finance/boros-core-public/contracts/lib/math/PMath.sol`.
/// `sdiv` is the EVM's native signed-division opcode, which truncates
/// toward zero, this is genuinely different from `mul_div_floor` for a
/// negative result with a nonzero remainder (floor rounds further away
/// from zero, toward -infinity; truncation stops at zero), they're not
/// interchangeable despite both being "round down" in the unsigned case.
pub fn mul_div_trunc(a: i128, b: i128, d: i128) -> Result<i128, MathError> {
    if d <= 0 {
        return Err(MathError::DivisionByZero);
    }
    if a == 0 || b == 0 {
        return Ok(0);
    }

    let neg = (a < 0) ^ (b < 0);
    let (hi, lo) = u128_wide_mul(a.unsigned_abs(), b.unsigned_abs());
    let (q, _remainder) = div_u256_by_u128(hi, lo, d.unsigned_abs())?;
    let q = i128::try_from(q).map_err(|_| MathError::Overflow)?;

    if neg { q.checked_neg().ok_or(MathError::Overflow) } else { Ok(q) }
}

/// `ceil(x*y/d)`, exact, for unsigned `u128` magnitudes, `d > 0`.
///
/// Mirrors `PMath.mulUp(uint256,uint256)`. Takes `u128`, not
/// `FixedX18`, the real contract enforces "this operand is a magnitude,
/// never negative" at the type level (`uint256` vs `int256`), and
/// `FixedX18` wraps a signed `i128` with no such guarantee. Forcing callers
/// to hand over a `u128` (e.g. via `.unsigned_abs()` on a value they've
/// already reasoned is non-negative, such as `|signedSize|` or a fee-index
/// delta) preserves that same invariant here instead of silently calling
/// `.abs()` internally and hiding the assumption.
pub fn mul_div_up(x: u128, y: u128, d: u128) -> Result<u128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }
    if x == 0 || y == 0 {
        return Ok(0);
    }
    let (hi, lo) = u128_wide_mul(x, y);
    let (q, r) = div_u256_by_u128(hi, lo, d)?;
    if r == 0 { Ok(q) } else { q.checked_add(1).ok_or(MathError::Overflow) }
}

/// `floor(x*y/d)` = plain truncation since both operands are unsigned,
/// exact, `d > 0`. Mirrors `PMath.mulDown(uint256,uint256)`. Same
/// unsigned-domain reasoning as `mul_div_up`.
pub fn mul_div_down(x: u128, y: u128, d: u128) -> Result<u128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }
    if x == 0 || y == 0 {
        return Ok(0);
    }
    let (hi, lo) = u128_wide_mul(x, y);
    let (q, _r) = div_u256_by_u128(hi, lo, d)?;
    Ok(q)
}

/// `floor(a*b*c/d)`, exact, where `c` fits in `u32`, matching the real
/// contract's `PaymentLib.calcPositionValue(int256 signedSize, int256
/// markRate, uint32 timeToMat)`, source-verified:
/// `(signedSize * markRate * int256(uint256(timeToMat))).rawDivFloor(IONE_MUL_YEAR)`.
///
/// This exists because `a*b` alone can already need the full 256-bit
/// widening (`u128_wide_mul`), and multiplying that by a third factor can
/// need up to 288 bits, three `u128` limbs, not two. The bound on `c` is
/// not an arbitrary simplification: the contract itself types `timeToMat`
/// as `uint32`, so a 3-limb (384-bit-capacity) intermediate is exactly
/// sufficient, not a guess at "small enough in practice."
///
/// Critically, this computes the triple product first and divides **once**
///, calling `mul_div_floor` twice in sequence (`floor(floor(a*b/d)*c/d)`-style)
/// is a different, less accurate computation due to intermediate rounding.
/// That double-rounding gap is exactly what this function exists to close
/// (see `margin-sim::Position::value`'s documented TODO).
pub fn mul3_div_floor_u32(a: i128, b: i128, c: u32, d: i128) -> Result<i128, MathError> {
    if d <= 0 {
        return Err(MathError::DivisionByZero);
    }
    if a == 0 || b == 0 || c == 0 {
        return Ok(0);
    }

    let neg = (a < 0) ^ (b < 0); // c (u32) and d (>0 checked above) never flip the sign
    let (hi1, lo1) = u128_wide_mul(a.unsigned_abs(), b.unsigned_abs()); // |a*b|, up to 256 bits
    let (limb2, limb1, limb0) = wide_mul_u256_by_u128(hi1, lo1, c as u128); // |a*b*c|, up to ~288 bits, 3 limbs

    let (q_hi, q_lo, r) = div_u3limb_by_u128(limb2, limb1, limb0, d.unsigned_abs())?;
    if q_hi != 0 {
        return Err(MathError::Overflow);
    }
    let q = i128::try_from(q_lo).map_err(|_| MathError::Overflow)?;

    if !neg {
        Ok(q)
    } else if r == 0 {
        q.checked_neg().ok_or(MathError::Overflow)
    } else {
        q.checked_neg().and_then(|v| v.checked_sub(1)).ok_or(MathError::Overflow)
    }
}

/// Multiply a 256-bit unsigned magnitude `(hi, lo)` by a `u128` scalar,
/// returning the result as 3 limbs `(limb2, limb1, limb0)` (up to ~384 bits
/// of capacity, comfortably more than the ~288 bits actually reachable
/// when the scalar is bounded to `u32`, per `mul3_div_floor_u32`'s use).
/// Schoolbook: multiply each input limb by the scalar, then propagate carries.
fn wide_mul_u256_by_u128(hi: u128, lo: u128, scalar: u128) -> (u128, u128, u128) {
    let (lo_hi, lo_lo) = u128_wide_mul(lo, scalar);
    let (hi_hi, hi_lo) = u128_wide_mul(hi, scalar);

    let (limb1, carry) = hi_lo.overflowing_add(lo_hi);
    let limb2 = hi_hi + (carry as u128);

    (limb2, limb1, lo_lo)
}

/// Divide a 3-limb unsigned magnitude `(limb2, limb1, limb0)` by `d`,
/// returning the quotient as `(q_hi, q_lo)` (256-bit capacity, the
/// realistic quotient for our use case fits in `q_lo` alone with `q_hi`
/// reserved as an overflow signal) and the remainder. Same bit-serial
/// restoring-division technique as `div_u256_by_u128`, extended to a wider
/// dividend, not a different algorithm, just more bits to walk.
fn div_u3limb_by_u128(limb2: u128, limb1: u128, limb0: u128, d: u128) -> Result<(u128, u128, u128), MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }

    let mut remainder: u128 = 0;
    let mut q_hi: u128 = 0;
    let mut q_lo: u128 = 0;

    for i in (0..384).rev() {
        let bit = if i >= 256 {
            (limb2 >> (i - 256)) & 1
        } else if i >= 128 {
            (limb1 >> (i - 128)) & 1
        } else {
            (limb0 >> i) & 1
        };
        remainder = (remainder << 1) | bit;

        if remainder >= d {
            remainder -= d;
            if i >= 128 {
                q_hi |= 1u128 << (i - 128);
            } else {
                q_lo |= 1u128 << i;
            }
        }
    }

    Ok((q_hi, q_lo, remainder))
}

/// `ceil(a*b*c/d)`, exact, unsigned magnitudes, `c` bounded to `u32`. Mirrors
/// `MarginViewUtils._calcMM`/`_calcIM`'s real rounding, source-verified,
/// both use `rawDivUp`, not floor: `(PM * kMM * timeToMat.max(tThresh)).rawDivUp(ONE_MUL_YEAR)`.
/// Same 3-limb machinery as `mul3_div_floor_u32`, unsigned and ceiling
/// instead of signed and floor.
pub fn mul3_div_up_u32(a: u128, b: u128, c: u32, d: u128) -> Result<u128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }
    if a == 0 || b == 0 || c == 0 {
        return Ok(0);
    }

    let (hi1, lo1) = u128_wide_mul(a, b);
    let (limb2, limb1, limb0) = wide_mul_u256_by_u128(hi1, lo1, c as u128);
    let (q_hi, q_lo, r) = div_u3limb_by_u128(limb2, limb1, limb0, d)?;
    if q_hi != 0 {
        return Err(MathError::Overflow);
    }
    if r == 0 { Ok(q_lo) } else { q_lo.checked_add(1).ok_or(MathError::Overflow) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul3_div_up_exact() {
        assert_eq!(mul3_div_up_u32(2_000_000_000_000_000_000, 3_000_000_000_000_000_000, 4, 4_000_000_000_000_000_000).unwrap(), 6_000_000_000_000_000_000);
    }

    #[test]
    fn mul3_div_up_rounds_up_on_remainder() {
        assert_eq!(mul3_div_up_u32(1, 1, 1, 1_000_000_000_000_000_000).unwrap(), 1);
    }

    #[test]
    fn mul3_div_up_zero_seconds_is_zero() {
        assert_eq!(mul3_div_up_u32(1_000_000_000_000_000_000, 1_000_000_000_000_000_000, 0, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul3_div_floor_matches_two_step_when_exact() {
        // 2.0 * 3.0 * 4 seconds / (1.0 * 4 seconds) = 6.0 -- simple sanity case
        let a = 2_000_000_000_000_000_000i128;
        let b = 3_000_000_000_000_000_000i128;
        let d = 4_000_000_000_000_000_000i128;
        assert_eq!(mul3_div_floor_u32(a, b, 4, d).unwrap(), 6_000_000_000_000_000_000);
    }

    #[test]
    fn mul3_div_floor_zero_seconds_is_zero() {
        assert_eq!(mul3_div_floor_u32(1_000_000_000_000_000_000, 1_000_000_000_000_000_000, 0, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul3_div_floor_single_division_differs_from_double_rounding() {
        // construct a case where floor(floor(a*b/d)*c/d) != floor(a*b*c/d) --
        // this is the exact double-rounding bug (Hallazgo 2) this function
        // exists to avoid. Pick values with a deliberate remainder at the
        // first division so the two paths diverge.
        let a = 1_000_000_000_000_000_003i128; // slightly more than 1.0
        let b = 1_000_000_000_000_000_003i128;
        let d = 1_000_000_000_000_000_000i128;
        let c: u32 = 3;

        let single = mul3_div_floor_u32(a, b, c, d).unwrap();

        // the double-rounding path: floor(a*b/d) first, then floor(that*c/d)
        let step1 = mul_div_floor(a, b, d).unwrap();
        let double = mul_div_floor(step1, c as i128, d).unwrap();

        // they need not always differ, but for THESE inputs they must, to
        // prove this isn't accidentally equivalent to the buggy path
        assert_ne!(single, double, "single-division and double-rounding paths must diverge for this input, or the test doesn't prove anything");
    }

    #[test]
    fn mul3_div_floor_exercises_third_limb() {
        // force a*b to be large enough that multiplying by c needs the third
        // limb: use near-i128::MAX magnitudes for a and b
        let huge = i128::MAX / 2; // still leaves room for the sign-free unsigned_abs multiply
        let d = 1_000_000_000_000_000_000i128;
        // just confirm it computes without overflowing incorrectly and
        // without panicking - either a valid result or a clean Overflow err
        let result = mul3_div_floor_u32(huge, huge, u32::MAX, d);
        match result {
            Ok(_) => {} // fits, fine
            Err(MathError::Overflow) => {} // correctly detected as too large for i128, fine
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn mul3_div_floor_negative_rounds_toward_negative_infinity() {
        let a = -1i128;
        let b = 1i128;
        let d = 1_000_000_000_000_000_000i128;
        // -1 * 1 * 1 / 1e18 is a tiny negative fraction -> floor must be -1, not 0
        assert_eq!(mul3_div_floor_u32(a, b, 1, d).unwrap(), -1);
    }

    #[test]
    fn mul3_div_floor_rejects_nonpositive_divisor() {
        assert_eq!(mul3_div_floor_u32(1, 1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul3_div_floor_u32(1, 1, 1, -1), Err(MathError::DivisionByZero));
    }

    #[test]
    fn wide_mul_identity() {
        let (hi, lo) = u128_wide_mul(1, 1);
        assert_eq!((hi, lo), (0, 1));
    }

    #[test]
    fn wide_mul_max() {
        // (2^128-1)^2 = 2^256 - 2^129 + 1  →  hi = 2^128-2, lo = 1
        let (hi, lo) = u128_wide_mul(u128::MAX, u128::MAX);
        assert_eq!(hi, u128::MAX - 1, "hi mismatch");
        assert_eq!(lo, 1, "lo mismatch");
    }

    #[test]
    fn wide_mul_round_numbers() {
        // 2^64 * 2^64 = 2^128  → hi=1, lo=0
        let base = 1u128 << 64;
        let (hi, lo) = u128_wide_mul(base, base);
        assert_eq!(hi, 1);
        assert_eq!(lo, 0);
    }

    #[test]
    fn mul_div_approx_basic() {
        // (3 * 4) / 6 = 2
        let r = mul_div_approx(3, 4, 6).unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn mul_div_approx_negative() {
        let r = mul_div_approx(-3, 4, 6).unwrap();
        assert_eq!(r, -2);
    }

    #[test]
    fn mul_div_approx_div_zero() {
        assert_eq!(mul_div_approx(1, 1, 0), Err(MathError::DivisionByZero));
    }

    // ── mul_div_floor / mul_div_ceil (signed) ──────────────────────────────

    #[test]
    fn mul_div_floor_exact_positive() {
        // 3.0 * 2.0 / 1.0 = 6.0, evenly divides
        assert_eq!(mul_div_floor(3_000_000_000_000_000_000, 2_000_000_000_000_000_000, 1_000_000_000_000_000_000).unwrap(), 6_000_000_000_000_000_000);
    }

    #[test]
    fn mul_div_floor_negative_exact_no_remainder() {
        // -1.5 * 0.1 = -0.15 exactly, no remainder to round
        let x = -1_500_000_000_000_000_000i128;
        let y = 100_000_000_000_000_000i128;
        assert_eq!(mul_div_floor(x, y, 1_000_000_000_000_000_000).unwrap(), -150_000_000_000_000_000);
    }

    #[test]
    fn mul_div_floor_rounds_toward_negative_infinity() {
        // raw product is a nonzero negative fraction of 1 raw unit, floor
        // must give -1, never 0 (0 would be truncation-toward-zero, not floor)
        assert_eq!(mul_div_floor(-1, 1, 1_000_000_000_000_000_000).unwrap(), -1);
        assert_eq!(mul_div_floor(1, -1, 1_000_000_000_000_000_000).unwrap(), -1);
    }

    #[test]
    fn mul_div_floor_positive_truncates_down() {
        // smallest positive nonzero product before scaling: floor gives 0,
        // same as truncation for positive numbers
        assert_eq!(mul_div_floor(1, 1, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul_div_ceil_rounds_toward_positive_infinity() {
        // mirror of the floor test: positive product with remainder rounds UP
        assert_eq!(mul_div_ceil(1, 1, 1_000_000_000_000_000_000).unwrap(), 1);
    }

    #[test]
    fn mul_div_ceil_negative_truncates_toward_zero() {
        // negative product: ceil == truncation-toward-zero already (sdiv),
        // no adjustment needed, must NOT become -1
        assert_eq!(mul_div_ceil(-1, 1, 1_000_000_000_000_000_000).unwrap(), 0);
        assert_eq!(mul_div_ceil(1, -1, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul_div_floor_ceil_agree_on_exact_division() {
        let a = 7_000_000_000_000_000_000i128;
        let b = 1_000_000_000_000_000_000i128;
        let d = 1_000_000_000_000_000_000i128;
        assert_eq!(mul_div_floor(a, b, d).unwrap(), mul_div_ceil(a, b, d).unwrap());
    }

    #[test]
    fn mul_div_floor_rejects_nonpositive_divisor() {
        assert_eq!(mul_div_floor(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_div_floor(1, 1, -1), Err(MathError::DivisionByZero));
    }

    #[test]
    fn mul_div_floor_zero_operand_is_zero() {
        assert_eq!(mul_div_floor(0, 12345, 1_000_000_000_000_000_000).unwrap(), 0);
        assert_eq!(mul_div_ceil(0, 12345, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    // ── mul_div_trunc (signed, round toward zero) ──────────────────────────

    #[test]
    fn mul_div_trunc_negative_truncates_toward_zero() {
        // this is the whole point of the function: floor gives -1 here
        // (see mul_div_floor_rounds_toward_negative_infinity above), trunc
        // must give 0
        assert_eq!(mul_div_trunc(-1, 1, 1_000_000_000_000_000_000).unwrap(), 0);
        assert_eq!(mul_div_trunc(1, -1, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul_div_trunc_positive_matches_floor() {
        // for a positive result, truncation and floor are the same thing,
        // no adjustment needed either way
        assert_eq!(mul_div_trunc(1, 1, 1_000_000_000_000_000_000).unwrap(), mul_div_floor(1, 1, 1_000_000_000_000_000_000).unwrap());
    }

    #[test]
    fn mul_div_trunc_matches_ceil_for_negative_results() {
        // ceil already truncates toward zero for negative products (see
        // mul_div_ceil_negative_truncates_toward_zero), so trunc should
        // land on exactly the same value as ceil here, not floor
        let (a, b, d) = (-1i128, 1i128, 1_000_000_000_000_000_000i128);
        assert_eq!(mul_div_trunc(a, b, d).unwrap(), mul_div_ceil(a, b, d).unwrap());
    }

    #[test]
    fn mul_div_trunc_differs_from_floor_by_one_with_negative_remainder() {
        // -3 / 2 = -1.5, has a remainder, negative sign
        let (a, b, d) = (-3i128, 1i128, 2i128);
        let floor = mul_div_floor(a, b, d).unwrap();
        let trunc = mul_div_trunc(a, b, d).unwrap();
        assert_eq!(floor, -2, "floor(-1.5) should round further from zero");
        assert_eq!(trunc, -1, "trunc(-1.5) should round toward zero");
        assert_eq!(trunc - floor, 1, "trunc must be exactly one unit closer to zero than floor");
    }

    #[test]
    fn mul_div_trunc_exact_division_agrees_with_floor_and_ceil() {
        let a = 7_000_000_000_000_000_000i128;
        let b = 1_000_000_000_000_000_000i128;
        let d = 1_000_000_000_000_000_000i128;
        let result = mul_div_trunc(a, b, d).unwrap();
        assert_eq!(result, mul_div_floor(a, b, d).unwrap());
        assert_eq!(result, mul_div_ceil(a, b, d).unwrap());
    }

    #[test]
    fn mul_div_trunc_rejects_nonpositive_divisor() {
        assert_eq!(mul_div_trunc(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_div_trunc(1, 1, -1), Err(MathError::DivisionByZero));
    }

    #[test]
    fn mul_div_trunc_zero_operand_is_zero() {
        assert_eq!(mul_div_trunc(0, 12345, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    // ── mul_div_up / mul_div_down (unsigned) ───────────────────────────────

    #[test]
    fn mul_div_up_exact() {
        assert_eq!(mul_div_up(3_000_000_000_000_000_000, 2_000_000_000_000_000_000, 1_000_000_000_000_000_000).unwrap(), 6_000_000_000_000_000_000);
    }

    #[test]
    fn mul_div_up_rounds_up_on_remainder() {
        assert_eq!(mul_div_up(1, 1, 1_000_000_000_000_000_000).unwrap(), 1);
    }

    #[test]
    fn mul_div_down_truncates() {
        assert_eq!(mul_div_down(1, 1, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul_div_up_down_agree_on_exact_division() {
        let x = 7_000_000_000_000_000_000u128;
        let y = 1_000_000_000_000_000_000u128;
        let d = 1_000_000_000_000_000_000u128;
        assert_eq!(mul_div_up(x, y, d).unwrap(), mul_div_down(x, y, d).unwrap());
    }

    #[test]
    fn mul_div_up_zero_operand_is_zero() {
        assert_eq!(mul_div_up(0, 12345, 1_000_000_000_000_000_000).unwrap(), 0);
        assert_eq!(mul_div_down(0, 12345, 1_000_000_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn mul_div_up_div_zero() {
        assert_eq!(mul_div_up(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_div_down(1, 1, 0), Err(MathError::DivisionByZero));
    }

    #[test]
    fn mul_div_detects_quotient_overflow() {
        // huge product, tiny divisor -> quotient doesn't fit in u128/i128
        let huge = i128::MAX;
        assert_eq!(mul_div_floor(huge, huge, 1), Err(MathError::Overflow));
        assert_eq!(mul_div_up(u128::MAX, u128::MAX, 1), Err(MathError::Overflow));
    }

    // ── cross-check: exact vs f64-approx agree within tolerance on the same inputs ──

    #[test]
    fn exact_and_approx_agree_within_f64_tolerance() {
        // sanity check that the new exact path and the old approx path aren't
        // wildly divergent on ordinary magnitudes, approx has documented
        // ~2-3 ULP error, so this just guards against a sign/scale bug, not
        // bit-exactness (that's the whole point of having both).
        let a = 1_234_567_890_123_456_789i128;
        let b = 987_654_321_098_765_432i128;
        let d = 1_000_000_000_000_000_000i128;

        let exact = mul_div_floor(a, b, d).unwrap();
        let approx = mul_div_approx(a, b, d).unwrap();

        let diff = (exact - approx).abs();
        // approx is f64-based (~15.9 sig digits); at this magnitude (~1e18)
        // that's an absolute error budget in the low hundreds of raw units
        assert!(diff < 10_000, "exact={exact} approx={approx} diff={diff}");
    }
}
