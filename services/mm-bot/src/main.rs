//! Multi-market maker for one Boros zone. Ties together `feed-ingest`
//! (real-time book/mark data), `curve-engine` (cross-maturity reference
//! curve), `quoting-engine` (Avellaneda-Stoikov quotes), `margin-sim` +
//! `risk-engine` (pre-trade checks and a local kill switch), `oms-core`
//! (order tracking), and `execution-adapter`'s gRPC client (actually
//! placing/cancelling orders).
//!
//! Runs its own kill switch, independent of `services/risk-monitor`. Not
//! duplication for its own sake: `risk-monitor` is a separate process by
//! design specifically so a wedged `mm-bot` event loop doesn't take the
//! safety net down with it, that only works if `mm-bot` ALSO has its own
//! floor and doesn't rely solely on an external process reacting in time.
//! Both watching the same thing independently is the point, not a bug.
//!
//! Known scope cuts, not oversights:
//! - One order per side per market (no ladder of multiple resting levels).
//! - No cost-basis tracking across fills, `InventoryState::avg_locked_fixed_rate`
//!   is always `None`, matches `carry_weight: 0.0` being the documented
//!   safe default for `AvellanedaStoikovParams` anyway.

mod config;
mod quote_cycle;
mod reconcile;
mod rest;
mod state;

use std::collections::HashMap;

use margin_sim::{MarginEngine, MarketId};
use risk_engine::KillSwitch;
use rust_bridge::ExecutionClient;

use config::MmBotConfig;
use rest::BorosRestClient;
use state::{AccountState, MarketRuntime, OrderRateTracker};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = MmBotConfig::from_env();
    let markets_file = cfg.load_markets();
    let zone_name = markets_file.zone_name;
    let rest = BorosRestClient::new(cfg.api_base_url.clone());

    tracing::info!(
        zone = %zone_name,
        markets = markets_file.markets.len(),
        quote_interval_ms = cfg.quote_interval.as_millis(),
        "mm-bot starting"
    );

    // initial per-market state: fetch margin config + market state + real
    // tick_step from REST before doing anything else, quoting against
    // stale/zeroed config would be worse than not quoting at all
    let mut runtimes: HashMap<u32, MarketRuntime> = HashMap::new();
    for market_cfg in markets_file.markets {
        let market_id = market_cfg.market_id;
        match reconcile::reconcile_market(&rest, market_id).await {
            Ok((margin_config, market_state, tick_step)) => {
                let mut market_cfg = market_cfg;
                market_cfg.tick_step = tick_step;
                runtimes.insert(market_id, MarketRuntime::new(market_cfg, margin_config, market_state));
            }
            Err(e) => {
                tracing::error!(market_id, "failed to fetch initial market config, skipping this market entirely: {e}");
            }
        }
    }

    if runtimes.is_empty() {
        tracing::error!("no markets initialized successfully, nothing to quote, exiting");
        return;
    }

    // feed-ingest: real-time mark rate updates between reconciles
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
        funding: vec![],
        book_channel_capacity: 64,
        mark_rate_channel_capacity: 256,
        trade_channel_capacity: 1,
        funding_channel_capacity: 1,
        statistics_channel_capacity: 1,
        account_channel_capacity: 1,
    };
    let feed_bus = feed_ingest::start(feed_cfg);
    let mut mark_rate_rx = feed_bus.subscribe_mark_rates();

    let execution = match ExecutionClient::connect(cfg.execution_endpoint.clone(), cfg.retry.clone()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to connect to execution-adapter at {}: {e}", cfg.execution_endpoint);
            return;
        }
    };
    let mut execution = execution;

    let mut account = AccountState::default();
    let mut kill_switch = KillSwitch::new();
    let mut order_tracker = OrderRateTracker::default();

    let mut quote_interval = tokio::time::interval(cfg.quote_interval);
    let mut reconcile_interval = tokio::time::interval(cfg.reconcile_interval);
    // fire the first reconcile immediately instead of waiting a full
    // interval, quoting with a default (all-zero) AccountState until then
    // would mean pre-trade checks running against a fake empty account
    reconcile_interval.reset_immediately();

    loop {
        tokio::select! {
            _ = quote_interval.tick() => {
                // drain any mark rate updates that arrived since the last tick
                while let Ok(ev) = mark_rate_rx.try_recv() {
                    if let Some(runtime) = runtimes.get_mut(&ev.market_id) {
                        runtime.market_state.mark_rate = margin_sim_mark_rate(ev.mark_apr);
                    }
                }

                let market_states: HashMap<MarketId, margin_sim::MarketState> =
                    runtimes.iter().map(|(&id, r)| (MarketId(id), r.market_state)).collect();
                let configs: HashMap<MarketId, margin_sim::MarginConfig> =
                    runtimes.iter().map(|(&id, r)| (MarketId(id), r.margin_config)).collect();
                let margin_engine = MarginEngine::new(configs, market_states.clone());

                quote_cycle::run_cycle(
                    &mut runtimes,
                    &zone_name,
                    cfg.token_id,
                    &account,
                    &margin_engine,
                    &market_states,
                    &cfg.pre_trade_limits,
                    cfg.requote_threshold,
                    &mut order_tracker,
                    &mut execution,
                    kill_switch.is_tripped(),
                ).await;
            }

            _ = reconcile_interval.tick() => {
                match reconcile::reconcile_account(&rest, &cfg.root_address, cfg.account_id).await {
                    Ok(mut fresh) => {
                        match rest.get_collateral_summary(&cfg.root_address, cfg.account_id, cfg.token_id).await {
                            Ok(summary) => {
                                match summary.collateral.cross_position.net_balance.parse::<f64>() {
                                    Ok(v) => fresh.cash = tick_math::FixedX18::from_f64(v),
                                    Err(_) => tracing::warn!("failed to parse netBalance, keeping previous cash value"),
                                }
                            }
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
                            // don't clobber a mark_rate that's been kept
                            // fresher by feed-ingest in between reconciles,
                            // only take REST's time_to_maturity (which REST
                            // and feed-ingest don't disagree about, that one's
                            // fine to overwrite)
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

/// `market-data-update`'s `mk` field, which `MarkRateEvent::mark_apr`
/// carries, is already an APR ratio (same scale as everything else this
/// codebase calls a "rate"), this is just the type conversion, not a
/// unit conversion.
fn margin_sim_mark_rate(mark_apr: f64) -> tick_math::FixedX18 {
    tick_math::FixedX18::from_f64(mark_apr)
}
