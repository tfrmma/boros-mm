use tick_math::FixedX18;

/// One observed point feeding the curve: a market's current implied APR at
/// its time-to-maturity. Typically the mid or mark rate of one Boros
/// market, this crate doesn't care which, that's the caller's choice
/// (mark rate is the protocol's own reference; a book mid might be
/// noisier but more current).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurvePoint {
    pub market_id: u32,
    pub time_to_maturity_secs: u32,
    pub rate: FixedX18,
}

/// A group of markets sharing the same underlying asset but different
/// maturities, matches Boros's own "zone" grouping used for cross-margin
/// (see `whitepapers/Boros.pdf`). A curve is fit per zone: rates across
/// different underlyings aren't comparable points on the same curve.
#[derive(Debug, Clone)]
pub struct Zone {
    pub name: String,
    pub points: Vec<CurvePoint>,
}

/// A detected deviation of the middle of three adjacent maturities from
/// the straight line connecting its neighbors.
///
/// Called a "signal", not "arbitrage": on a real bond curve, a negative
/// butterfly is a riskless, replicable arbitrage (you can construct the
/// offsetting position from the same underlying discount curve). Boros
/// markets at different maturities are **not fungible on-chain**, there's
/// no mechanism to convert exposure at one maturity into another directly.
/// This is a relative-value / mean-reversion trading signal (a candidate
/// calendar spread), not a riskless arbitrage. Don't let anything
/// downstream treat it as guaranteed profit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButterflySignal {
    pub left_maturity_secs: u32,
    pub mid_maturity_secs: u32,
    pub right_maturity_secs: u32,
    /// `mid_rate - linear_interpolation(left_rate, right_rate at mid_maturity)`.
    /// Negative: the middle maturity is priced "cheap" relative to its
    /// neighbors (candidate: pay fixed there, receive fixed on the wings).
    /// Positive: priced "rich" relative to its neighbors (the reverse).
    pub deviation: f64,
}
