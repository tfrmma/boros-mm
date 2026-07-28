use oms_core::Side;

/// One market's Boros implied APR paired with the expected/realized
/// annualized funding rate on a comparable CEX perp for the same
/// underlying. Doesn't depend on `feed-ingest` directly (same as
/// `settlement-ledger`: `feed-ingest` is toolchain-blocked in this
/// workspace). The caller builds this from whatever funding feed it has.
#[derive(Debug, Clone, Copy)]
pub struct CrossVenueObservation {
    pub boros_market_id: u32,
    /// This Boros market's currently available fixed rate (e.g. its mark
    /// rate, or the rate you could actually trade at, the caller decides
    /// which is more appropriate for their execution assumptions).
    pub boros_implied_apr: f64,
    /// Annualized, from whichever CEX venue/symbol the caller is comparing
    /// against.
    pub cex_expected_funding_apr: f64,
}

/// A detected cross-venue basis, with the side of Boros that captures it.
///
/// **Not a riskless arbitrage**, flagged the same way as
/// `curve_engine::ButterflySignal`, for related but distinct reasons:
/// the CEX "expected" funding is exactly that, an expectation, not a
/// locked-in rate the way Boros's fixed leg is; and Boros's floating index
/// and the CEX venue's realized funding are computed by different
/// mechanisms tracking (presumably) the same underlying funding market,
/// not identical by construction. Basis risk is real. This is a candidate
/// trade, not a guaranteed profit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossVenueSignal {
    pub boros_market_id: u32,
    /// `boros_implied_apr - cex_expected_funding_apr`.
    pub basis: f64,
    /// The side of Boros that captures a positive basis: if Boros's fixed
    /// rate is rich relative to CEX funding, going `Short` on Boros
    /// receives that rich fixed rate, paired off-crate with a long
    /// funding-rate exposure on the CEX venue. Sizing and executing that
    /// hedge leg is the caller's job.
    pub boros_side: Side,
}

/// A `curve_engine::ButterflySignal` translated into a directional trade:
/// which side to take at the mid maturity, and the opposite side at both
/// wings.
///
/// Not DV01-sized here: a true risk-minimized butterfly needs the wings
/// weighted by relative DV01 sensitivity so the combined position is close
/// to flat to a parallel curve shift, which needs position-sizing context
/// (account size, risk budget) this crate doesn't have. This gives the
/// *direction*; `risk-engine`/the caller sizes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalendarSpreadTrade {
    pub signal: curve_engine::ButterflySignal,
    pub mid_side: Side,
    pub wing_side: Side,
}
