//! Arbitrage bot for one Boros zone: calendar-spread signals within the
//! zone's own curve (`curve_engine::Curve::detect_butterflies`) and
//! cross-venue basis signals against external funding rates
//! (`arb_engine::detect_cross_venue_signal`, fed by `feed-ingest`'s
//! Binance/Bybit/Hyperliquid funding feeds).
//!
//! Runs its own kill switch, independent of `services/risk-monitor` and
//! `services/mm-bot` (same reasoning as both of those, see mm-bot's
//! module doc, not repeated here).
//!
//! Known scope cuts, not oversights:
//! - No automatic unwind. A signal that reverses, or a kill switch trip,
//!   leaves existing legs resting/filled, doesn't close them. Closing a
//!   calendar spread or a cross-venue position is a real decision (has
//!   the basis actually mean-reverted, or moved further against you) left
//!   to the operator.
//! - Calendar spread leg placement isn't atomic, see `signal_cycle.rs`'s
//!   module doc for exactly what that risks.
//! - The CEX hedge leg of a cross-venue trade is never placed by this
//!   bot, only logged as needed, matches `arb_engine::CrossVenueSignal`'s
//!   own documented scope (not this crate's job).
//! - `recent_order_count` for `check_pre_trade`'s throttle is fixed at 0,
//!   same known no-op as `mm-bot`, not wired to a real sliding window yet.

mod config;
mod reconcile;
mod rest;
mod signal_cycle;
mod state;

use std::collections::HashMap;

use feed_ingest::Venue;
use margin_sim::MarketId;
use risk_engine::KillSwitch;
use rust_bridge::ExecutionClient;

use config::ArbBotConfig;
use rest::BorosRestClient;
use state::{AccountState, MarketRuntime, SignalCooldowns};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = ArbBotConfig::from_env();
    let markets_file = cfg.load_markets();
    let zone_name = markets_file.zone_name;
    let min_abs_calendar_deviation = markets_file.min_abs_calendar_deviation;
    let rest = BorosRestClient::new(cfg.api_base_url.clone());

    tracing::info!(
        zone = %zone_name,
        markets = markets_file.markets.len(),
        scan_interval_ms = cfg.scan_interval.as_millis(),
        "arb-bot starting"
    );

    let mut runtimes: HashMap<u32, MarketRuntime> = HashMap::new();
    for market_cfg in markets_file.markets {
        let market_id = market_cfg.market_id;
        match reconcile::reconcile_market(&rest, market_id).await {
            Ok((margin_config, market_state, tick_step)) => {
                let mut market_cfg = market_cfg;
                market_cfg.tick_step = tick_step;
                runtimes.insert(market_id, MarketRuntime::new(market_cfg, margin_config, market_state));
            }
            Err(e) => tracing::error!(market_id, "failed to fetch initial market config, skipping: {e}"),
        }
    }

    if runtimes.is_empty() {
        tracing::error!("no markets initialized successfully, nothing to scan, exiting");
        return;
    }

    // feed-ingest: real-time mark rates (for the zone curve) plus external
    // funding rates (for cross-venue signals). Funding sources are only
    // added for venues actually referenced by a configured market, no
    // point running a Hyperliquid feed if nothing in this zone compares
    // against it.
    let mut funding_sources: Vec<(Venue, String)> = Vec::new();
    for r in runtimes.values() {
        if let Some(cex) = &r.config.cex_reference {
            if !funding_sources.iter().any(|(v, s)| *v == cex.venue && *s == cex.symbol) {
                funding_sources.push((cex.venue, cex.symbol.clone()));
            }
        }
    }

    let feed_cfg = feed_ingest::FeedIngestConfig {
        boros: feed_ingest::BorosConfig {
            ws_url: cfg.ws_url.clone(),
            namespace: cfg.ws_namespace.clone(),
            reconnect: feed_ingest::ReconnectConfig::default(),
            max_reconnect_attempts: None,
            markets: runtimes.values().map(|r| feed_ingest::MarketFeedConfig {
                market_id: r.config.market_id,
                tick_size: r.config.feed_tick_size,
                subscribe_orderbook: false,
                subscribe_orderbook_amm: false,
                subscribe_trades: false,
                subscribe_statistics: false,
                subscribe_market_data: true,
            }).collect(),
            account: None,
        },
        funding: funding_sources.iter().map(|(venue, symbol)| feed_ingest::FundingSourceConfig {
            venue: *venue,
            ws_url: default_funding_ws_url(*venue),
            symbols: vec![symbol.clone()],
            reconnect: feed_ingest::ReconnectConfig::default(),
            write_buf: 16,
            event_buf: 64,
        }).collect(),
        book_channel_capacity: 1,
        mark_rate_channel_capacity: 256,
        trade_channel_capacity: 1,
        funding_channel_capacity: 256,
        statistics_channel_capacity: 1,
        account_channel_capacity: 1,
    };
    let feed_bus = feed_ingest::start(feed_cfg);
    let mut mark_rate_rx = feed_bus.subscribe_mark_rates();
    let mut funding_rx = feed_bus.subscribe_funding();

    let mut execution = match ExecutionClient::connect(cfg.execution_endpoint.clone(), cfg.retry.clone()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to connect to execution-adapter at {}: {e}", cfg.execution_endpoint);
            return;
        }
    };

    let mut account = AccountState::default();
    let mut kill_switch = KillSwitch::new();
    let mut cooldowns = SignalCooldowns::default();
    let mut funding_rates: HashMap<(Venue, String), f64> = HashMap::new();
    let recent_order_count: u32 = 0; // TODO: same known no-op as mm-bot, see module doc

    let mut scan_interval = tokio::time::interval(cfg.scan_interval);
    let mut reconcile_interval = tokio::time::interval(cfg.reconcile_interval);
    reconcile_interval.reset_immediately();

    loop {
        tokio::select! {
            _ = scan_interval.tick() => {
                while let Ok(ev) = mark_rate_rx.try_recv() {
                    if let Some(runtime) = runtimes.get_mut(&ev.market_id) {
                        runtime.market_state.mark_rate = tick_math::FixedX18::from_f64(ev.mark_apr);
                    }
                }
                while let Ok(ev) = funding_rx.try_recv() {
                    funding_rates.insert((ev.venue, ev.symbol.clone()), ev.rate);
                }

                let market_states: HashMap<MarketId, margin_sim::MarketState> =
                    runtimes.iter().map(|(&id, r)| (MarketId(id), r.market_state)).collect();

                signal_cycle::run_calendar_scan(
                    &mut runtimes, &zone_name, min_abs_calendar_deviation, &mut cooldowns, cfg.signal_cooldown,
                    cfg.token_id, &account, &market_states, &cfg.pre_trade_limits, recent_order_count,
                    &mut execution, kill_switch.is_tripped(),
                ).await;

                signal_cycle::run_cross_venue_scan(
                    &mut runtimes, &funding_rates, &mut cooldowns, cfg.signal_cooldown,
                    cfg.token_id, &account, &market_states, &cfg.pre_trade_limits, recent_order_count,
                    &mut execution, kill_switch.is_tripped(),
                ).await;
            }

            _ = reconcile_interval.tick() => {
                match reconcile::reconcile_account(&rest, &cfg.root_address, cfg.account_id).await {
                    Ok(mut fresh) => {
                        match rest.get_collateral_summary(&cfg.root_address, cfg.account_id, cfg.token_id).await {
                            Ok(summary) => match summary.collateral.cross_position.net_balance.parse::<f64>() {
                                Ok(v) => fresh.cash = tick_math::FixedX18::from_f64(v),
                                Err(_) => tracing::warn!("failed to parse netBalance, keeping previous cash value"),
                            },
                            Err(e) => tracing::warn!("failed to fetch collateral summary: {e}"),
                        }
                        account = fresh;
                    }
                    Err(e) => tracing::error!("account reconcile failed, keeping previous state: {e}"),
                }

                for (&market_id, runtime) in runtimes.iter_mut() {
                    match reconcile::reconcile_market(&rest, market_id).await {
                        Ok((margin_config, market_state, tick_step)) => {
                            runtime.margin_config = margin_config;
                            runtime.market_state.time_to_maturity_secs = market_state.time_to_maturity_secs;
                            runtime.config.tick_step = tick_step;
                        }
                        Err(e) => tracing::warn!(market_id, "market reconcile failed, keeping previous config: {e}"),
                    }
                }

                match reconcile::compute_local_health_ratio(&runtimes, &account, cfg.token_id) {
                    Ok(health_ratio) => {
                        if health_ratio < cfg.conservative_health_ratio {
                            if !kill_switch.is_tripped() {
                                let reason = format!("local health ratio {health_ratio:.4} below conservative threshold {:.4}", cfg.conservative_health_ratio);
                                kill_switch.trip(reason.clone());
                                tracing::error!(health_ratio, "KILL SWITCH TRIPPED: {reason}");
                            }
                        } else if kill_switch.is_tripped() {
                            kill_switch.reset("local health ratio recovered above threshold".to_owned());
                            tracing::warn!(health_ratio, "kill switch reset, health ratio recovered");
                        }
                        tracing::info!(health_ratio, tripped = kill_switch.is_tripped(), "reconcile complete");
                    }
                    Err(e) => tracing::error!("local health ratio calc failed: {e}"),
                }
            }
        }
    }
}

/// `feed_ingest::FundingSourceConfig` needs a `ws_url` per venue,
/// `mm-bot` never had to build one of these (it doesn't touch funding
/// feeds at all). Checked each against the real value the corresponding
/// `funding/*.rs` file's own code expects (not just assumed to match):
/// Bybit and Hyperliquid matched their doc comments exactly, Binance's
/// `run()` does `format!("{}?streams={streams}", cfg.ws_url)`, so this
/// needs to be the bare `/stream` path, not a full URL with `/ws`.
fn default_funding_ws_url(venue: Venue) -> String {
    match venue {
        // binance.rs's run() does format!("{}?streams={streams}", cfg.ws_url),
        // so this needs to be the bare /stream path, not the final URL
        Venue::Binance => "wss://fstream.binance.com/stream".to_owned(),
        Venue::Bybit => "wss://stream.bybit.com/v5/public/linear".to_owned(),
        Venue::Hyperliquid => "wss://api.hyperliquid.xyz/ws".to_owned(),
        Venue::Okx => panic!("OKX funding feed not implemented yet (see feed-ingest's lib.rs), can't configure a CexReference against it"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_funding_ws_url_binance_is_bare_stream_path() {
        // binance.rs's run() appends "?streams=...", the base can't already have a query string or path segment
        assert_eq!(default_funding_ws_url(Venue::Binance), "wss://fstream.binance.com/stream");
    }

    #[test]
    fn default_funding_ws_url_bybit() {
        assert_eq!(default_funding_ws_url(Venue::Bybit), "wss://stream.bybit.com/v5/public/linear");
    }

    #[test]
    fn default_funding_ws_url_hyperliquid() {
        assert_eq!(default_funding_ws_url(Venue::Hyperliquid), "wss://api.hyperliquid.xyz/ws");
    }

    #[test]
    #[should_panic(expected = "OKX funding feed not implemented")]
    fn default_funding_ws_url_okx_panics_not_implemented() {
        default_funding_ws_url(Venue::Okx);
    }
}
