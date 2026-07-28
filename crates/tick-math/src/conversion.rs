use crate::error::MathError;
use crate::fixed::FixedX18;

// protocol constant, DO NOT change this
const TICK_BASE: f64 = 1.00005_f64;
const LN_TICK_BASE: f64 = 4.999_875_006_249_896e-5; // ln(1.00005), pre-computed

/// Tick range the protocol supports. i16 gives [-32768, 32767] which matches
/// the 16-bit tick field in the OrderId encoding.
pub const TICK_MIN: i16 = i16::MIN; // -32768
pub const TICK_MAX: i16 = i16::MAX; //  32767

/// When converting a rate back to a tick we need to pick a direction.
/// Get this wrong and you're posting inside the spread on the wrong side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Round toward better rate for long orders (round down = lower tick = lower rate).
    Floor,
    /// Round toward better rate for short orders (round up = higher tick = higher rate).
    Ceil,
}

/// tick → FixedX18 rate.
///
/// rate(tick) = 1.00005^(|tick| * step) - 1  (with sign from tick)
///
/// Uses f64 internally. For 16-bit ticks this is exact enough, the max
/// representable rate is ~5x, well within f64 precision.
pub fn tick_to_rate(tick: i16, tick_step: u8) -> Result<FixedX18, MathError> {
    if tick_step == 0 {
        return Err(MathError::InvalidTickStep(tick_step));
    }

    if tick == 0 {
        return Ok(FixedX18::ZERO);
    }

    let exp = (tick.unsigned_abs() as f64) * (tick_step as f64);
    let rate_abs = TICK_BASE.powf(exp) - 1.0;

    // should never happen with valid i16 ticks and reasonable step sizes,
    // but be paranoid
    if !rate_abs.is_finite() || rate_abs < 0.0 {
        return Err(MathError::InvalidTick(tick));
    }

    let signed = if tick > 0 { rate_abs } else { -rate_abs };
    Ok(FixedX18::from_f64(signed))
}

/// FixedX18 rate → nearest valid tick.
///
/// Inverse of tick_to_rate. Rounding controls which side of the spread
/// you end up on, this is not a detail you can ignore.
///
/// For long (bid) orders you want Floor (lower tick = lower rate = more conservative).
/// For short (ask) orders you want Ceil (higher tick = higher rate = more conservative).
pub fn rate_to_tick(rate: FixedX18, tick_step: u8, rounding: Rounding) -> Result<i16, MathError> {
    if tick_step == 0 {
        return Err(MathError::InvalidTickStep(tick_step));
    }

    let rate_f64 = rate.to_f64();

    if rate_f64 == 0.0 {
        return Ok(0);
    }

    let rate_abs = rate_f64.abs();

    // inverse: |tick| * step = log(rate_abs + 1) / ln(TICK_BASE)
    let tick_abs_f64 = (rate_abs + 1.0).ln() / LN_TICK_BASE / (tick_step as f64);

    if !tick_abs_f64.is_finite() {
        return Err(MathError::RateNotRepresentable(rate_f64));
    }

    let tick_abs_rounded = match rounding {
        Rounding::Floor => tick_abs_f64.floor() as i64,
        Rounding::Ceil  => tick_abs_f64.ceil()  as i64,
    };

    let tick_signed = if rate_f64 > 0.0 {
        tick_abs_rounded
    } else {
        -tick_abs_rounded
    };

    if tick_signed > TICK_MAX as i64 || tick_signed < TICK_MIN as i64 {
        return Err(MathError::RateNotRepresentable(rate_f64));
    }

    Ok(tick_signed as i16)
}

/// Returns the rate at the best bid and best ask surrounding a target rate.
///
/// Useful for computing the quoting spread at a given position in the book:
/// floor_tick is where you'd post a bid, ceil_tick is where you'd post an ask.
pub fn rate_to_tick_bracket(
    rate: FixedX18,
    tick_step: u8,
) -> Result<(i16, i16), MathError> {
    let bid_tick = rate_to_tick(rate, tick_step, Rounding::Floor)?;
    let ask_tick = rate_to_tick(rate, tick_step, Rounding::Ceil)?;
    Ok((bid_tick, ask_tick))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn zero_tick_is_zero_rate() {
        assert_eq!(tick_to_rate(0, 1).unwrap(), FixedX18::ZERO);
        assert_eq!(tick_to_rate(0, 2).unwrap(), FixedX18::ZERO);
    }

    #[test]
    fn tick_step2_100() {
        // 1.00005^200 - 1 = 0.010049914580...
        let rate = tick_to_rate(100, 2).unwrap().to_f64();
        assert!(approx_eq(rate, 0.010049914580, 1e-8), "got {rate}");
    }

    #[test]
    fn negative_tick_is_negative_rate() {
        let pos = tick_to_rate(100, 2).unwrap().to_f64();
        let neg = tick_to_rate(-100, 2).unwrap().to_f64();
        assert!(approx_eq(pos, -neg, 1e-18), "symmetry broken: {pos} vs {neg}");
    }

    #[test]
    fn roundtrip_positive() {
        let tick_in: i16 = 500;
        let rate = tick_to_rate(tick_in, 2).unwrap();
        let tick_out = rate_to_tick(rate, 2, Rounding::Floor).unwrap();
        // floor can be off by 1 due to floating point, but must not drift more
        assert!((tick_in - tick_out).abs() <= 1, "roundtrip drift: {tick_in} → {tick_out}");
    }

    #[test]
    fn roundtrip_negative() {
        let tick_in: i16 = -500;
        let rate = tick_to_rate(tick_in, 2).unwrap();
        let tick_out = rate_to_tick(rate, 2, Rounding::Ceil).unwrap();
        assert!((tick_in - tick_out).abs() <= 1, "roundtrip drift: {tick_in} → {tick_out}");
    }

    #[test]
    fn bracket_gives_floor_leq_ceil() {
        let rate = FixedX18::from_f64(0.05);
        let (bid, ask) = rate_to_tick_bracket(rate, 2).unwrap();
        assert!(bid <= ask);
    }

    #[test]
    fn invalid_tick_step_zero() {
        assert!(tick_to_rate(1, 0).is_err());
        assert!(rate_to_tick(FixedX18::ONE, 0, Rounding::Floor).is_err());
    }

    #[test]
    fn max_tick_step1() {
        // tick=32767, step=1: 1.00005^32767 - 1 ≈ 4.1465...
        let rate = tick_to_rate(TICK_MAX, 1).unwrap().to_f64();
        assert!(rate > 4.0 && rate < 5.0, "unexpected max rate: {rate}");
    }
}
