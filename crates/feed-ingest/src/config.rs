use crate::event::Venue;

/// Backoff params for the reconnect loop in ws/connection.rs. Field names are
/// load-bearing, connection.rs already reads `initial_delay_ms`,
/// `backoff_multiplier`, `max_delay_ms` directly.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    pub initial_delay_ms: u64,
    pub backoff_multiplier: f64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 500,
            backoff_multiplier: 2.0,
            max_delay_ms: 30_000,
        }
    }
}

/// One market's subscription config for the Boros feed. tick_size drives
/// both the orderbook channel name and how `ia` gets converted to actual APR
/// (see TickSize / OrderbookUpdate in event.rs).
#[derive(Debug, Clone)]
pub struct MarketFeedConfig {
    pub market_id: u32,
    pub tick_size: f64,
    pub subscribe_orderbook: bool,
    pub subscribe_orderbook_amm: bool,
    pub subscribe_trades: bool,
    pub subscribe_statistics: bool,
    pub subscribe_market_data: bool,
}

/// Optional account-level subscription (position/order/settlement updates).
/// root_address must already be lowercased, Boros's channel format is
/// case-sensitive on this (see websocket doc, Account Update Events).
#[derive(Debug, Clone)]
pub struct AccountFeedConfig {
    pub root_address: String,
}

#[derive(Debug, Clone)]
pub struct BorosConfig {
    /// Full base URL including the custom engine.io path, e.g.
    /// "wss://api-boros.pendle.finance/socket/socket.io", rust_socketio
    /// takes the engine.io path from the URL's own path component, there's
    /// no separate builder method for it (verified against ClientBuilder
    /// source, see boros/mod.rs).
    pub ws_url: String,
    /// Socket.IO namespace, e.g. "/pendle-dapp-v3".
    pub namespace: String,
    pub reconnect: ReconnectConfig,
    pub max_reconnect_attempts: Option<u8>,
    pub markets: Vec<MarketFeedConfig>,
    pub account: Option<AccountFeedConfig>,
}

/// One external funding-rate source (Binance/Bybit/Hyperliquid/OKX).
#[derive(Debug, Clone)]
pub struct FundingSourceConfig {
    pub venue: Venue,
    pub ws_url: String,
    pub symbols: Vec<String>,
    pub reconnect: ReconnectConfig,
    pub write_buf: usize,
    pub event_buf: usize,
}

#[derive(Debug, Clone)]
pub struct FeedIngestConfig {
    pub boros: BorosConfig,
    pub funding: Vec<FundingSourceConfig>,
    pub book_channel_capacity: usize,
    pub mark_rate_channel_capacity: usize,
    pub trade_channel_capacity: usize,
    pub funding_channel_capacity: usize,
    /// Added 2026-07-19 alongside FeedBus.statistics/account_updates.
    pub statistics_channel_capacity: usize,
    pub account_channel_capacity: usize,
}
