//! All configuration comes from the environment, same convention as
//! `execution-adapter/sidecar-ts/src/config.ts`, nothing about which
//! account, which markets, or how aggressive the kill switch is gets
//! hardcoded here.

use std::time::Duration;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var: {name}"))
}

fn optional_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn optional_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub struct RiskMonitorConfig {
    /// Boros core REST base URL. Real default confirmed 2026-07-19 by
    /// reading `@pendle/sdk-boros@1.5.0`'s compiled `BorosCoreSDK.js`
    /// (`baseURL: 'https://api.boros.finance/core'`), overridable, this is
    /// a default value, not a hardcoded assumption the rest of the code
    /// depends on.
    pub api_base_url: String,
    /// The account this process watches. One risk-monitor instance per
    /// account, not per bot, matches "independent process, no shared
    /// failure domain with the quoting/execution side" from the crate doc.
    pub root_address: String,
    pub account_id: u32,
    pub token_id: u32,
    /// Markets this account has (or might have) positions in, needed to
    /// fetch each market's `MarginConfig` for the shadow calc. Not
    /// auto-discovered, an account with a position risk-monitor doesn't
    /// know about is exactly the failure mode this service exists to
    /// avoid quietly having.
    pub market_ids: Vec<u32>,
    pub poll_interval: Duration,
    pub divergence: risk_engine::DivergenceConfig,
    /// Trip the kill switch if the REAL (API-reported) health ratio drops
    /// below this. Kept separate from `margin-sim`'s
    /// `LIQUIDATION_HEALTH_RATIO` (1.0, the on-chain liquidation
    /// boundary), this is meant to fire earlier, while there's still time
    /// to react.
    pub conservative_health_ratio: f64,
    /// Bare-bones `/health` TCP endpoint address, see main.rs's
    /// `serve_health`. No framework dependency for one route.
    pub listen_addr: String,
}

impl RiskMonitorConfig {
    pub fn from_env() -> Self {
        let market_ids = required("RISK_MONITOR_MARKET_IDS")
            .split(',')
            .map(|s| s.trim().parse::<u32>().unwrap_or_else(|_| panic!("invalid market id in RISK_MONITOR_MARKET_IDS: {s}")))
            .collect();

        Self {
            api_base_url: std::env::var("BOROS_API_BASE_URL").unwrap_or_else(|_| "https://api.boros.finance/core".to_owned()),
            root_address: required("RISK_MONITOR_ROOT_ADDRESS"),
            account_id: required("RISK_MONITOR_ACCOUNT_ID").parse().expect("RISK_MONITOR_ACCOUNT_ID must be a number"),
            token_id: required("RISK_MONITOR_TOKEN_ID").parse().expect("RISK_MONITOR_TOKEN_ID must be a number"),
            market_ids,
            poll_interval: Duration::from_secs(optional_u64("RISK_MONITOR_POLL_INTERVAL_SECS", 10)),
            divergence: risk_engine::DivergenceConfig {
                max_health_ratio_relative_diff: optional_f64("RISK_MONITOR_MAX_HEALTH_DIVERGENCE_PCT", 0.05),
                max_mark_rate_abs_diff: optional_f64("RISK_MONITOR_MAX_MARK_RATE_ABS_DIFF", 0.001),
            },
            conservative_health_ratio: optional_f64("RISK_MONITOR_CONSERVATIVE_HEALTH_RATIO", 1.15),
            listen_addr: std::env::var("RISK_MONITOR_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_owned()),
        }
    }
}
