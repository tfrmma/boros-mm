//! Wire types for the Boros WebSocket feed and for external funding sources.
//!
//! Boros side verified against docs.pendle.finance/boros-dev/Backend/websocket,
//! re-fetched and re-checked field by field, no drift found in any struct
//! below since the previous pass. Where the doc only lists field names in
//! prose (no explicit types table), it's noted, those are typed on best
//! judgement and should still be checked against a real payload capture
//! before anything trades off them.
//!
//! Binance/Bybit/Hyperliquid types are unrelated to Boros and just mirror what
//! funding/binance.rs, funding/bybit.rs, funding/hyperliquid.rs already build.

use serde::Deserialize;

use tick_math::FixedX18;

use crate::error::FeedError;

// ── shared enums (docs.pendle.finance/boros-dev/Backend/glossary + websocket "Order Update" table) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Long = 0,
    Short = 1,
}

impl TryFrom<u8> for Side {
    type Error = FeedError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Side::Long),
            1 => Ok(Side::Short),
            other => Err(FeedError::Protocol(format!("unknown Side value: {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for Side {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u8::deserialize(d)?;
        Side::try_from(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderType {
    Limit = 0,
    Market = 1,
    TakeProfitMarket = 2,
    StopLossMarket = 3,
}

impl TryFrom<u8> for OrderType {
    type Error = FeedError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(OrderType::Limit),
            1 => Ok(OrderType::Market),
            2 => Ok(OrderType::TakeProfitMarket),
            3 => Ok(OrderType::StopLossMarket),
            other => Err(FeedError::Protocol(format!("unknown OrderType value: {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for OrderType {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u8::deserialize(d)?;
        OrderType::try_from(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LimitOrderStatus {
    Filling = 0,
    Cancelled = 1,
    FullyFilled = 2,
    Expired = 3,
    Purged = 4,
}

impl TryFrom<u8> for LimitOrderStatus {
    type Error = FeedError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(LimitOrderStatus::Filling),
            1 => Ok(LimitOrderStatus::Cancelled),
            2 => Ok(LimitOrderStatus::FullyFilled),
            3 => Ok(LimitOrderStatus::Expired),
            4 => Ok(LimitOrderStatus::Purged),
            other => Err(FeedError::Protocol(format!("unknown LimitOrderStatus value: {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for LimitOrderStatus {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u8::deserialize(d)?;
        LimitOrderStatus::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Legacy `account:ACCOUNT:update` change type. Doc only says "type: Position,
/// LimitOrder, or Collateral" in prose, no table, no field name for it beyond
/// implying a `type` key, treat this as low-confidence until a real payload
/// is captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum AccountChangeType {
    Position,
    LimitOrder,
    Collateral,
}

// ── tick size (Orderbook Events table: accepted values 0.1/0.01/0.001/0.0001) ──

#[derive(Debug, Clone, Copy)]
pub struct TickSize(f64);

// f64 doesn't get Eq/Hash for free (NaN), but TickSize::new only ever
// admits one of the 4 exact literals in ALLOWED, so bit-exact comparison
// is genuinely correct equality here, not a hack.
impl PartialEq for TickSize {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for TickSize {}
impl std::hash::Hash for TickSize {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl TickSize {
    pub const ALLOWED: [f64; 4] = [0.1, 0.01, 0.001, 0.0001];

    pub fn new(v: f64) -> Result<Self, FeedError> {
        if Self::ALLOWED.iter().any(|a| (*a - v).abs() < 1e-12) {
            Ok(Self(v))
        } else {
            Err(FeedError::Protocol(format!(
                "tick_size {v} not in accepted set {:?}",
                Self::ALLOWED
            )))
        }
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

// ── orderbook (Orderbook Events table) ──────────────────────────────────────

/// One side of an aggregated orderbook snapshot. `ia` and `sz` pair up by
/// index, level i has implied APR `ia[i]` (bucketed by TICK_SIZE, actual
/// APR = ia * tick_size) and notional size `sz[i]` (FixedX18 raw).
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookSide {
    pub ia: Vec<f64>,
    pub sz: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatus {
    #[serde(rename = "blockNumber")]
    pub block_number: u64,
    pub timestamp: u64,
}

/// Payload of `orderbook:MARKET_ID:TICK_SIZE:update` and
/// `orderbook-include-amm-update`. Always a full snapshot, up to 50 levels
/// per side, no delta and no sequence number, unlike the old book.rs/state.rs
/// design, which still assumes delta+seq and needs rewriting to consume this.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookUpdate {
    pub long: OrderbookSide,
    pub short: OrderbookSide,
    #[serde(rename = "syncStatus")]
    pub sync_status: SyncStatus,
}

/// A single aggregated price level, derived from `OrderbookUpdate` once the
/// caller knows which market/tick_size/side it came from. This is a domain
/// type for consumers (e.g. book.rs after its rewrite), not a wire struct.
#[derive(Debug, Clone, Copy)]
pub struct BookLevel {
    pub apr: f64,
    pub size: FixedX18,
}

/// One full orderbook snapshot for a market, tagged with the channel context
/// (market_id / tick_size / whether AMM liquidity is merged in) that the raw
/// `OrderbookUpdate` payload itself doesn't carry.
#[derive(Debug, Clone)]
pub struct BookEvent {
    pub market_id: u32,
    pub tick_size: TickSize,
    pub include_amm: bool,
    pub long: OrderbookSide,
    pub short: OrderbookSide,
    pub sync_status: SyncStatus,
}

// ── market trade (Market Channels table row: fields listed in prose only) ──

/// Payload of `market-trade:MARKET_ID:update`. The WS doc's own channel
/// description confirms the field set directly now (`rate`, `size`,
/// `blockTimestamp`, `txHash`), not just by analogy with the SDK's REST
/// `MarketTradeResponse` type as before. What's still not nailed down:
/// whether `rate`/`size` are FixedX18-raw strings on this channel
/// specifically, the doc's FixedX18-raw callout lists orderbook/position/
/// settlement fields by name but doesn't name market-trade in that list.
/// `rate`/`size` accept either a JSON string or number on the wire and
/// normalize to a decimal string, instead of betting on one and breaking
/// on the other.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketTradeUpdate {
    #[serde(deserialize_with = "de_numeric_string")]
    pub rate: String,
    #[serde(deserialize_with = "de_numeric_string")]
    pub size: String,
    #[serde(rename = "blockTimestamp")]
    pub block_timestamp: u64,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
}

/// Accepts a JSON string or a JSON number, normalizes to a decimal string.
/// See `MarketTradeUpdate`'s doc comment for why this exists instead of
/// picking one representation and breaking on the other.
fn de_numeric_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(serde_json::Number),
    }
    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => Ok(s),
        StringOrNumber::Number(n) => Ok(n.to_string()),
    }
}

/// No side field is documented anywhere for market-trade. Kept as a type for
/// lib.rs's re-export and future use, but nothing here populates it, don't
/// invent a long/short mapping without a confirmed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeSide {
    Long,
    Short,
}

#[derive(Debug, Clone)]
pub struct TradeEvent {
    pub market_id: u32,
    pub rate: String,
    pub size: String,
    pub block_timestamp: u64,
    pub tx_hash: String,
    pub side: Option<TradeSide>,
}

// ── statistics (Market Channels table row: fields listed in prose only) ────

/// Payload of `statistics:MARKET_ID:update`. Field set cross-verified
/// 2026-07-19 against `@pendle/sdk-boros@1.5.0`'s `MarketDataResponse` REST
/// type (`backend/secrettune/BorosCoreSDK.d.ts`): all 8 fields below appear
/// there with identical names and typed `number`, which also confirms the
/// f64 typing (previously reasoned by analogy with MarketDataUpdate's
/// documented fields, now independently confirmed). That REST type has
/// more fields than these 8 (`bestBid`, `ammImpliedApr`, `assetMarkPrice`,
/// `dailyVolatility`...), REST likely returns a superset of what's actually
/// pushed over this specific WS channel, not added here since the
/// websocket doc's channel description only lists these 8.
#[derive(Debug, Clone, Deserialize)]
pub struct StatisticsUpdate {
    #[serde(rename = "markApr")]
    pub mark_apr: f64,
    #[serde(rename = "midApr")]
    pub mid_apr: f64,
    #[serde(rename = "lastTradedApr")]
    pub last_traded_apr: f64,
    #[serde(rename = "floatingApr")]
    pub floating_apr: f64,
    #[serde(rename = "volume24h")]
    pub volume_24h: f64,
    #[serde(rename = "notionalOI")]
    pub notional_oi: f64,
    #[serde(rename = "nextSettlementTime")]
    pub next_settlement_time: u64,
    #[serde(rename = "longYieldApr")]
    pub long_yield_apr: f64,
}

// ── market data (Market Data Events table, fully typed in the doc) ─────────

#[derive(Debug, Clone, Deserialize)]
pub struct MarketDataUpdate {
    #[serde(rename = "mId")]
    pub market_id: u32,
    pub bn: u64,
    pub bt: u64,
    pub oi: f64,
    pub mid: f64,
    pub mk: f64,
    pub lt: f64,
    pub nst: u64,
    pub bb: Option<f64>,
    pub ba: Option<f64>,
    pub ai: Option<f64>,
}

/// There's no dedicated "markRate" channel in the real protocol, this is
/// derived from `market-data-update.mk` ("Mark APR (from contract)", per the
/// Market Data Events table), which is the closest documented equivalent.
/// Kept because lib.rs's FeedBus/BorosFeedHandler contract still expects a
/// mark-rate broadcast channel; not touching lib.rs this pass.
#[derive(Debug, Clone)]
pub struct MarkRateEvent {
    pub market_id: u32,
    pub mark_apr: f64,
    pub block_number: u64,
    pub block_timestamp: u64,
}

impl From<&MarketDataUpdate> for MarkRateEvent {
    fn from(m: &MarketDataUpdate) -> Self {
        Self {
            market_id: m.market_id,
            mark_apr: m.mk,
            block_number: m.bn,
            block_timestamp: m.bt,
        }
    }
}

// ── account update events (Account Update Events tables, fully typed) ──────

/// `MarketAcc` is documented as packed `bytes26` in Contracts/CustomTypes.
/// Confirmed 2026-07-19 that it crosses the wire as a hex string: the SDK
/// declares `export type MarketAcc = Hex` (`types/common.d.ts`, `Hex` is
/// viem's `` `0x${string}` `` type). Kept as a plain `String` here rather
/// than a `0x`-prefix-checked newtype, decoding the packed
/// address/accountId/tokenId/marketId fields out of it isn't needed by
/// anything in this crate yet.
pub type MarketAccRaw = String;

#[derive(Debug, Clone, Deserialize)]
pub struct PositionUpdate {
    pub ei: u64,
    pub bn: u64,
    pub bt: u64,
    pub tx: String,
    #[serde(rename = "mId")]
    pub market_id: u32,
    pub ma: MarketAccRaw,
    pub prf: String,
    pub prs: String,
    pub ptf: String,
    pub pts: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderUpdate {
    pub ei: u64,
    pub bn: u64,
    pub bt: u64,
    pub tx: String,
    #[serde(rename = "mId")]
    pub market_id: u32,
    #[serde(rename = "oId")]
    pub order_id: String,
    pub sd: Side,
    pub ps: String,
    pub us: String,
    pub tk: i16,
    pub ot: OrderType,
    pub os: LimitOrderStatus,
    pub efs: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettlementUpdate {
    pub ei: u64,
    pub bn: u64,
    pub bt: u64,
    pub tx: String,
    pub ma: MarketAccRaw,
    #[serde(rename = "mId")]
    pub market_id: u32,
    pub ps: String,
    pub yp: String,
    pub yr: String,
    pub pa: f64,
    pub ra: f64,
    pub fee: String,
}

/// `StatisticsUpdate` doesn't carry its own market_id (the doc's fields
/// don't include one, only the channel name `statistics:MARKET_ID` does),
/// same situation `BookEvent` was in before it got a `market_id` field.
#[derive(Debug, Clone)]
pub struct MarketStatisticsEvent {
    pub market_id: u32,
    pub stats: StatisticsUpdate,
}

/// Carries `account-updates:ROOT_ADDRESS`'s three events
/// (`position-update`/`order-update`/`settlement-update`) on one channel
/// instead of three separate ones: a consumer reconciling account state
/// generally wants all three interleaved in arrival order, not three
/// streams to merge itself.
///
/// `settlement-update` has limited availability: as of this check, Boros
/// only emits it for a handful of allowlisted market makers, not for every
/// user. A caller that isn't on that list will never see this variant,
/// silently, there's no error or rejection, the event just never arrives.
/// `GET /accounts/settlements` (REST) or the legacy `account:ACCOUNT`
/// channel are the fallback for everyone else, not implemented by this
/// crate yet. `settlement-ledger`'s FIndex-driven local settlement doesn't
/// depend on this event either way, it derives everything from fills plus
/// `record_findex`, this is only relevant to anything that expects a push
/// notification the moment a settlement posts.
#[derive(Debug, Clone)]
pub enum AccountUpdateEvent {
    Position(PositionUpdate),
    Order(OrderUpdate),
    Settlement(SettlementUpdate),
}

// ── funding (external exchanges, unrelated to Boros) ────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
pub enum Venue {
    Binance,
    Bybit,
    Hyperliquid,
    Okx,
}

#[derive(Debug, Clone)]
pub struct FundingRateEvent {
    pub venue: Venue,
    pub symbol: String,
    pub rate: f64,
    pub interval_secs: u64,
    pub next_funding_ts: u64,
    pub fetched_at_ms: u64,
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Parse a FixedX18-raw wire string (18-decimal fixed point, e.g. from `sz`,
/// `prf`, `ps`, `yp`...) into the internal FixedX18 type used by tick-math.
pub fn parse_fixed_x18_raw(s: &str) -> Result<FixedX18, FeedError> {
    s.parse::<i128>()
        .map(FixedX18::raw)
        .map_err(|_| FeedError::FixedX18(s.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_trade_update_accepts_string_rate_and_size() {
        let json = r#"{"rate": "0.0523", "size": "1000000000000000000", "blockTimestamp": 12345, "txHash": "0xabc"}"#;
        let update: MarketTradeUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.rate, "0.0523");
        assert_eq!(update.size, "1000000000000000000");
    }

    #[test]
    fn market_trade_update_accepts_numeric_rate_and_size() {
        // REST's MarketTradeResponse types these as `number`, WS might too
        let json = r#"{"rate": 0.0523, "size": 1000, "blockTimestamp": 12345, "txHash": "0xabc"}"#;
        let update: MarketTradeUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.rate, "0.0523");
        assert_eq!(update.size, "1000");
    }

    #[test]
    fn statistics_update_decodes_all_eight_confirmed_fields() {
        let json = r#"{
            "markApr": 0.05, "midApr": 0.048, "lastTradedApr": 0.051,
            "floatingApr": 0.049, "volume24h": 1000000.0, "notionalOI": 5000000.0,
            "nextSettlementTime": 1234567890, "longYieldApr": 0.046
        }"#;
        let update: StatisticsUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.mark_apr, 0.05);
        assert_eq!(update.next_settlement_time, 1234567890);
    }
}
