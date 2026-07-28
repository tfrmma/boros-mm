//! Top-level settings from env vars, same convention as `risk-monitor`/
//! `sidecar-ts`. Per-market config is nested/structured (AS params, maker
//! bounds, market_acc...), doesn't fit flat env vars the way risk-monitor's
//! single `market_ids` list did, so that part loads from a JSON file
//! instead (path given by an env var). Still fully external, still nothing
//! hardcoded, just a more appropriate shape for structured data.

use std::time::Duration;

use quoting_engine::{AvellanedaStoikovParams, MakerRateBounds};
use risk_engine::PreTradeLimits;
use serde::Deserialize;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var: {name}"))
}

fn optional_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn optional_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One market this bot quotes. `market_acc` is the packed `MarketAcc` hex
/// string the contract expects for order placement, provided here rather
/// than computed: the exact bit layout (address/accountId/tokenId/marketId,
/// see `Account.sol`) hasn't been independently verified this session the
/// way `MarketAcc = Hex`'s wire *format* was, only that it packs those
/// fields, not their bit order/widths. Get it from wherever the account
/// was set up (Boros's own UI/SDK), don't derive it here from a guess.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    pub market_id: u32,
    pub market_acc: String,
    /// Orderbook feed granularity (0.1/0.01/0.001/0.0001), a display/feed
    /// choice, NOT the same thing as `tick_step` below (on-chain order
    /// placement granularity, an integer). Conflating these two was almost
    /// a real bug while writing this, they're both called "tick" something
    /// but govern different layers.
    pub feed_tick_size: f64,
    /// On-chain tick granularity for order placement (`tickStep` from
    /// `MarketIMDataResponse`, fetched once at startup, not configured by
    /// hand, see `main.rs`).
    #[serde(skip)]
    pub tick_step: u8,
    pub base_quote_size: f64,
    pub as_params: AvellanedaStoikovParams,
    pub maker_bounds: MakerRateBounds,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketsFile {
    pub zone_name: String,
    pub markets: Vec<MarketConfig>,
}

pub struct MmBotConfig {
    pub api_base_url: String,
    pub execution_endpoint: String,
    pub root_address: String,
    pub account_id: u32,
    pub token_id: u32,
    pub markets_config_path: String,
    /// How often quotes get recomputed and (if they've moved enough)
    /// replaced. Kept separate from `reconcile_interval`, quoting
    /// should react faster than the full account state needs re-fetching.
    pub quote_interval: Duration,
    /// How often account state (positions, cash, per-market margin config)
    /// gets refreshed from REST. Slower than quoting on purpose, this is
    /// the expensive multi-endpoint round trip. Real-time mark rate moves
    /// come from `feed-ingest` between reconciles, not from waiting on this.
    pub reconcile_interval: Duration,
    /// Minimum rate change (absolute, same scale as the quote itself) to
    /// bother cancelling and replacing a resting order. Without this,
    /// every tiny book tick would trigger a cancel/replace, expensive and
    /// pointless.
    pub requote_threshold: f64,
    pub pre_trade_limits: PreTradeLimits,
    /// Local kill-switch floor, independent of `services/risk-monitor`
    /// (defense in depth, not a replacement for it, a separate process
    /// with a wedged event loop shouldn't be this bot's only safety net).
    pub conservative_health_ratio: f64,
    pub retry: rust_bridge::RetryConfig,
    /// `feed-ingest`'s Socket.IO connection details, see
    /// `feed_ingest::config::BorosConfig`'s own doc comment for why
    /// `ws_url` bundles the engine.io path and `namespace` is separate.
    pub ws_url: String,
    pub ws_namespace: String,
}

impl MmBotConfig {
    pub fn from_env() -> Self {
        Self {
            api_base_url: std::env::var("BOROS_API_BASE_URL").unwrap_or_else(|_| "https://api.boros.finance/core".to_owned()),
            execution_endpoint: required("MM_BOT_EXECUTION_ENDPOINT"),
            root_address: required("MM_BOT_ROOT_ADDRESS"),
            account_id: required("MM_BOT_ACCOUNT_ID").parse().expect("MM_BOT_ACCOUNT_ID must be a number"),
            token_id: required("MM_BOT_TOKEN_ID").parse().expect("MM_BOT_TOKEN_ID must be a number"),
            markets_config_path: required("MM_BOT_MARKETS_CONFIG_PATH"),
            quote_interval: Duration::from_millis(optional_u64("MM_BOT_QUOTE_INTERVAL_MS", 2000)),
            reconcile_interval: Duration::from_secs(optional_u64("MM_BOT_RECONCILE_INTERVAL_SECS", 15)),
            requote_threshold: optional_f64("MM_BOT_REQUOTE_THRESHOLD", 0.0005),
            pre_trade_limits: PreTradeLimits {
                max_net_dv01: optional_f64("MM_BOT_MAX_NET_DV01", 10_000.0),
                max_gross_dv01: optional_f64("MM_BOT_MAX_GROSS_DV01", 50_000.0),
                max_notional: optional_f64("MM_BOT_MAX_NOTIONAL", 5_000_000.0),
                min_projected_health_ratio: optional_f64("MM_BOT_MIN_PROJECTED_HEALTH_RATIO", 1.3),
                max_orders_per_window: optional_u64("MM_BOT_MAX_ORDERS_PER_WINDOW", 20) as u32,
                throttle_window_secs: optional_u64("MM_BOT_THROTTLE_WINDOW_SECS", 60) as u32,
            },
            conservative_health_ratio: optional_f64("MM_BOT_CONSERVATIVE_HEALTH_RATIO", 1.15),
            retry: rust_bridge::RetryConfig {
                max_attempts: optional_u64("MM_BOT_EXEC_MAX_ATTEMPTS", 3) as u32,
                initial_backoff: Duration::from_millis(optional_u64("MM_BOT_EXEC_INITIAL_BACKOFF_MS", 100)),
                max_backoff: Duration::from_secs(optional_u64("MM_BOT_EXEC_MAX_BACKOFF_SECS", 2)),
                backoff_multiplier: optional_f64("MM_BOT_EXEC_BACKOFF_MULTIPLIER", 2.0),
            },
            ws_url: required("MM_BOT_WS_URL"),
            ws_namespace: required("MM_BOT_WS_NAMESPACE"),
        }
    }

    pub fn load_markets(&self) -> MarketsFile {
        let raw = std::fs::read_to_string(&self.markets_config_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", self.markets_config_path));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {e}", self.markets_config_path))
    }
}
