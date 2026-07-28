use serde::{Deserialize, Serialize};
use tick_math::FixedX18;

// ── identifiers ───────────────────────────────────────────────────────────────

/// On-chain subaccount index. 0 is the default; up to 255 per user address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubaccountId(pub u8);

impl SubaccountId {
    pub const DEFAULT: Self = Self(0);
}

/// Market identifier. On-chain `uint24`; stored here as `u32` (a
/// pre-existing width mismatch, not reconciled in this pass, see
/// `README.md`'s technical debt table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketId(pub u32);

/// Token identifier for the collateral. On-chain `uint16`; same width note
/// as `MarketId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenId(pub u32);

// ── margin mode ───────────────────────────────────────────────────────────────

/// Cross shares collateral across markets; isolated constrains it to one.
/// On-chain cross-margin accounts have marketId == 2^24 - 1 (CROSS sentinel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarginMode {
    Cross,
    Isolated { market_id: MarketId },
}

// ── market state ─────────────────────────────────────────────────────────────

/// The current mark rate and time-to-maturity for one market, fast-changing
/// data shared by every position and order in that market, not duplicated
/// on each one individually. Source:
/// `interfaces/IMarket.sol::MarketMem { ..., int256 rMark, uint32 timeToMat, ... }`
/// (`pendle-finance/boros-core-public`). Both fields are raw contract types:
/// `rMark` is FixedX18-scaled, `timeToMat` is **raw seconds** (`uint32`),
/// never a pre-converted year-fraction.
///
/// Kept separate from `MarginConfig`: this changes every mark-rate tick,
/// `MarginConfig` changes on the order of governance actions. Bundling them
/// would force refetching slow config alongside fast market data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MarketState {
    pub mark_rate: FixedX18,
    pub time_to_maturity_secs: u32,
}

// ── position ──────────────────────────────────────────────────────────────────

/// A single IRS position in one market.
///
/// Position size is signed: positive = long (paying fixed, receiving float),
/// negative = short (receiving fixed, paying float).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub market_id: MarketId,
    /// Notional size in FixedX18.
    pub size: FixedX18,
}

impl Position {
    /// PV = size × mark_rate × ttm, computed as ONE triple product and ONE
    /// division, matches `PaymentLib.calcPositionValue` exactly:
    /// `rawDivFloor(signedSize * markRate * timeToMat, ONE_MUL_YEAR)`
    /// (`contracts/lib/PaymentLib.sol:32-34`).
    ///
    /// This replaces an earlier version that chained two `mul_fixed` calls
    /// (two independent roundings), provably not the same computation as
    /// one fused division even with exact arithmetic on both sides (the
    /// double-rounding gap this workspace's docs called "Hallazgo 2").
    /// `mul3_div_floor_u32` in `tick-math` exists specifically to close it.
    pub fn value(&self, market: &MarketState) -> Result<FixedX18, tick_math::MathError> {
        tick_math::mul3_div_floor_u32(self.size.inner(), market.mark_rate.inner(), market.time_to_maturity_secs, tick_math::ONE_MUL_YEAR)
            .map(FixedX18::raw)
    }
}

// ── margin config ─────────────────────────────────────────────────────────────

/// Per-account, per-market margin ratios. Source-verified against
/// `MarginViewUtils.sol` (`_calcMM`/`_calcIM`), field names mirror the
/// contract's own (`kMM`, `kIM`, `k_iThresh`, `tThresh`), not renamed to
/// something more "readable" that would drift from the source of truth.
///
/// `k_im`/`k_mm` are personal per-account values returned by
/// `_kIM(addr)`/`_kMM(addr)` on-chain, whitelisted market makers can have
/// a lower personal factor than the global default. Fetch fresh per account,
/// don't assume the global default applies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MarginConfig {
    /// `kIM`. FixedX18-scaled ratio (on-chain `uint64`, still a FixedX18
    /// magnitude, `uint64` is just a narrower container since a margin
    /// ratio never needs the full `int256` range).
    pub k_im: FixedX18,
    /// `kMM`. Always <= `k_im` on a healthy config, but this isn't enforced
    /// here, that's the API/contract's job when it hands you the config.
    pub k_mm: FixedX18,
    /// `I_threshold` (`k_iThresh`): minimum |rate| used throughout margin
    /// calculations to prevent near-zero-rate gaming. FixedX18-scaled rate.
    pub k_i_thresh: FixedX18,
    /// `tThresh`: minimum time-to-maturity used in margin calculations,
    /// preventing near-expiry positions from having near-zero requirements.
    /// **Raw seconds** (`uint32` on-chain), not FixedX18-scaled.
    pub t_thresh: u32,
    /// This market's settlement/collateral token. Added 2026-07-18 for
    /// `check_cross_token_consistency`, source: `MarginManager.sol:26-30`,
    /// which pulls `tokenId` straight off `IMarket(market).getInfo()` and
    /// asserts `user.tokenId() == tokenId` with `Err.MMTokenMismatch()`.
    /// Cross accounts are inherently scoped to one token (confirmed via
    /// `AccountLib.toMainCross`, `Account.sol:79-81`, which packs a single
    /// `TokenId` into the cross `MarketAcc`), so this is what makes "net
    /// everything together" a coherent operation in the first place.
    pub token_id: TokenId,
}

// ── open orders ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Long,
    Short,
}

/// A resting limit order.
///
/// Source-verified: the real margin formula (`MarginViewUtils._calcPM` /
/// `_calcPMFromTick`) does **not** carry a per-order time-to-maturity, an
/// earlier version of this struct had one, which was wrong. Time-to-maturity
/// is a market-level property (`MarketState`); every order and position in
/// the same market shares it. What each order DOES carry independently is
/// its own limit rate, `_calcPMFromTick` prices an order's margin
/// contribution using the rate **at its own tick**, not the current mark
/// rate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct OpenOrder {
    pub market_id: MarketId,
    pub side: OrderSide,
    /// Always positive.
    pub size: FixedX18,
    /// Rate at this order's own limit tick (not the market's current mark
    /// rate).
    pub rate: FixedX18,
}

// ── account ───────────────────────────────────────────────────────────────────

/// Off-chain representation of a MarketAcc.
///
/// Populated by fetching account state from the REST API + settleAllAndGet.
/// The on-chain state is authoritative; this is a shadow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarginAccount {
    pub subaccount_id: SubaccountId,
    pub token_id: TokenId,
    pub margin_mode: MarginMode,
    /// Available cash (collateral deposited minus upfront costs paid).
    pub cash: FixedX18,
    /// Active positions across markets this account has entered.
    pub positions: Vec<Position>,
    /// Resting limit orders (needed for IM worst-case calc).
    pub open_orders: Vec<OpenOrder>,
    /// Unix timestamp of last settlement. If this is stale vs now, the
    /// position values below may not reflect the latest floating payments.
    pub last_settled_at: u64,
}

impl MarginAccount {
    pub fn is_cross(&self) -> bool {
        matches!(self.margin_mode, MarginMode::Cross)
    }
}

// ── health ratio ─────────────────────────────────────────────────────────────

/// The only universal, protocol-invariant health-ratio constant, per
/// source: `LiquidationViewUtils._calcLiqTradeAft` requires
/// `0 <= healthRatio && healthRatio < PMath.IONE`, liquidation eligibility
/// is `healthRatio < 1.0`, full stop, no governance override.
///
/// No `HealthThresholds` struct with `risky`/`deleverage` fields exists
/// here, they aren't protocol constants. `LiquidationViewUtils.sol` shows
/// the liquidation *incentive* is computed dynamically from governance-set
/// `LiqSettings { base, slope, feeRate }` (`k = base + slope*(1-healthRatio)`,
/// capped at `min(k, healthRatio)`); there's no fixed "risky" cutoff in the
/// source, and deleverage is triggered by an admin comparing a winner's
/// and loser's health ratios directly, not a fixed threshold either. Fetch
/// `LiqSettings` fresh from the API/zone config, same as `MarginConfig`.
pub const LIQUIDATION_HEALTH_RATIO: f64 = 1.0;

/// Governance-configurable liquidation incentive settings.
/// Source: `interfaces/IMarket.sol::LiqSettings { uint64 base; uint64 slope; uint64 feeRate; }`.
/// No default, these are per-market/zone governance values, fetch fresh.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LiqSettings {
    pub base: FixedX18,
    pub slope: FixedX18,
    pub fee_rate: FixedX18,
}

/// The computed margin state of an account. This is what we actually care about.
#[derive(Debug, Clone)]
pub struct AccountMarginState {
    pub total_value: FixedX18,
    pub total_im: FixedX18,
    pub total_mm: FixedX18,
    pub health_ratio: f64,
    /// `health_ratio < 1.0`, see `LIQUIDATION_HEALTH_RATIO`.
    pub is_liquidatable: bool,
}
