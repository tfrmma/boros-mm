//! Same convention as `mm-bot`/`risk-monitor`: simple settings from env
//! vars, structured per-market config from a JSON file.

use std::time::Duration;

use feed_ingest::Venue;
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

/// Which external venue/symbol to compare this market's implied rate
/// against for a cross-venue signal. Not every market needs one, a
/// market only participating in calendar-spread detection can leave this
/// out entirely.
#[derive(Debug, Clone, Deserialize)]
pub struct CexReference {
    pub venue: Venue,
    pub symbol: String,
    /// See `arb_engine::detect_cross_venue_signal`'s doc comment, this is
    /// not defaulted anywhere upstream, the right threshold
    /// depends on the CEX leg's own cost structure, which only the
    /// operator configuring this market actually knows.
    pub min_abs_basis: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    pub market_id: u32,
    pub market_acc: String,
    pub feed_tick_size: f64,
    #[serde(skip)]
    pub tick_step: u8,
    pub base_size: f64,
    pub cex_reference: Option<CexReference>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketsFile {
    pub zone_name: String,
    /// Zone-wide, `Curve::detect_butterflies`'s threshold applies across
    /// the whole fitted curve, not per-market, unlike `CexReference::min_abs_basis`.
    pub min_abs_calendar_deviation: f64,
    pub markets: Vec<MarketConfig>,
}

pub struct ArbBotConfig {
    pub api_base_url: String,
    pub execution_endpoint: String,
    pub root_address: String,
    pub account_id: u32,
    pub token_id: u32,
    pub markets_config_path: String,
    /// How often signals get recomputed. Faster than `reconcile_interval`
    /// for the same reason as mm-bot: signal detection should react to
    /// fresh mark rates/funding data, account state doesn't need
    /// refreshing that often.
    pub scan_interval: Duration,
    pub reconcile_interval: Duration,
    /// Minimum time between acting on the same signal twice (same triple
    /// for calendar spreads, same market for cross-venue). Without this,
    /// a signal that stays above threshold for several scan ticks in a
    /// row would get re-entered every tick, not what "detect a basis and
    /// trade it once" means.
    pub signal_cooldown: Duration,
    pub pre_trade_limits: PreTradeLimits,
    pub conservative_health_ratio: f64,
    /// Slippage tolerance for the IOC order used to unwind a reversed
    /// signal. Real cost of taking liquidity now instead of resting an
    /// ALO, the right number depends on the market's typical depth, not
    /// something this bot should guess.
    pub unwind_ioc_slippage: f64,
    pub retry: rust_bridge::RetryConfig,
    pub ws_url: String,
    pub ws_namespace: String,
}

impl ArbBotConfig {
    pub fn from_env() -> Self {
        Self {
            api_base_url: std::env::var("BOROS_API_BASE_URL").unwrap_or_else(|_| "https://api.boros.finance/core".to_owned()),
            execution_endpoint: required("ARB_BOT_EXECUTION_ENDPOINT"),
            root_address: required("ARB_BOT_ROOT_ADDRESS"),
            account_id: required("ARB_BOT_ACCOUNT_ID").parse().expect("ARB_BOT_ACCOUNT_ID must be a number"),
            token_id: required("ARB_BOT_TOKEN_ID").parse().expect("ARB_BOT_TOKEN_ID must be a number"),
            markets_config_path: required("ARB_BOT_MARKETS_CONFIG_PATH"),
            scan_interval: Duration::from_millis(optional_u64("ARB_BOT_SCAN_INTERVAL_MS", 3000)),
            reconcile_interval: Duration::from_secs(optional_u64("ARB_BOT_RECONCILE_INTERVAL_SECS", 15)),
            signal_cooldown: Duration::from_secs(optional_u64("ARB_BOT_SIGNAL_COOLDOWN_SECS", 300)),
            pre_trade_limits: PreTradeLimits {
                max_net_dv01: optional_f64("ARB_BOT_MAX_NET_DV01", 10_000.0),
                max_gross_dv01: optional_f64("ARB_BOT_MAX_GROSS_DV01", 50_000.0),
                max_notional: optional_f64("ARB_BOT_MAX_NOTIONAL", 5_000_000.0),
                min_projected_health_ratio: optional_f64("ARB_BOT_MIN_PROJECTED_HEALTH_RATIO", 1.3),
                max_orders_per_window: optional_u64("ARB_BOT_MAX_ORDERS_PER_WINDOW", 20) as u32,
                throttle_window_secs: optional_u64("ARB_BOT_THROTTLE_WINDOW_SECS", 60) as u32,
            },
            conservative_health_ratio: optional_f64("ARB_BOT_CONSERVATIVE_HEALTH_RATIO", 1.15),
            unwind_ioc_slippage: optional_f64("ARB_BOT_UNWIND_IOC_SLIPPAGE", 0.002),
            retry: rust_bridge::RetryConfig {
                max_attempts: optional_u64("ARB_BOT_EXEC_MAX_ATTEMPTS", 3) as u32,
                initial_backoff: Duration::from_millis(optional_u64("ARB_BOT_EXEC_INITIAL_BACKOFF_MS", 100)),
                max_backoff: Duration::from_secs(optional_u64("ARB_BOT_EXEC_MAX_BACKOFF_SECS", 2)),
                backoff_multiplier: optional_f64("ARB_BOT_EXEC_BACKOFF_MULTIPLIER", 2.0),
            },
            ws_url: required("ARB_BOT_WS_URL"),
            ws_namespace: required("ARB_BOT_WS_NAMESPACE"),
        }
    }

    pub fn load_markets(&self) -> MarketsFile {
        let raw = std::fs::read_to_string(&self.markets_config_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", self.markets_config_path));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse {}: {e}", self.markets_config_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markets_file_parses_market_without_cex_reference() {
        let json = r#"{
            "zone_name": "btc-perp",
            "min_abs_calendar_deviation": 0.002,
            "markets": [
                { "market_id": 1, "market_acc": "0xabc", "feed_tick_size": 0.01, "base_size": 100.0, "cex_reference": null }
            ]
        }"#;
        let parsed: MarketsFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.zone_name, "btc-perp");
        assert_eq!(parsed.markets.len(), 1);
        assert!(parsed.markets[0].cex_reference.is_none());
        // tick_step is #[serde(skip)], must default to 0 regardless of what's in the JSON
        assert_eq!(parsed.markets[0].tick_step, 0);
    }

    #[test]
    fn markets_file_parses_market_with_cex_reference() {
        let json = r#"{
            "zone_name": "eth-perp",
            "min_abs_calendar_deviation": 0.001,
            "markets": [
                { "market_id": 2, "market_acc": "0xdef", "feed_tick_size": 0.01, "base_size": 50.0,
                  "cex_reference": { "venue": "Binance", "symbol": "ETHUSDT", "min_abs_basis": 0.015 } }
            ]
        }"#;
        let parsed: MarketsFile = serde_json::from_str(json).unwrap();
        let cex = parsed.markets[0].cex_reference.as_ref().expect("cex_reference should be present");
        assert_eq!(cex.venue, Venue::Binance);
        assert_eq!(cex.symbol, "ETHUSDT");
        assert_eq!(cex.min_abs_basis, 0.015);
    }

    #[test]
    fn markets_file_rejects_unknown_venue() {
        let json = r#"{
            "zone_name": "z", "min_abs_calendar_deviation": 0.0,
            "markets": [
                { "market_id": 1, "market_acc": "0x1", "feed_tick_size": 0.01, "base_size": 1.0,
                  "cex_reference": { "venue": "Deribit", "symbol": "X", "min_abs_basis": 0.01 } }
            ]
        }"#;
        let result: Result<MarketsFile, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Venue enum has no Deribit variant, this must fail to parse, not silently default");
    }

    #[test]
    fn markets_file_rejects_missing_required_field() {
        let json = r#"{
            "zone_name": "z", "min_abs_calendar_deviation": 0.0,
            "markets": [ { "market_id": 1, "feed_tick_size": 0.01, "base_size": 1.0 } ]
        }"#;
        let result: Result<MarketsFile, _> = serde_json::from_str(json);
        assert!(result.is_err(), "market_acc is required, missing it must not silently default to empty string");
    }
}
