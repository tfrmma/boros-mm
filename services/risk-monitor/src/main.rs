//! Independent risk-monitor process. No shared memory, no shared failure
//! domain with whatever's placing/managing orders (`services/mm-bot`,
//! Sprint 4, not built yet), this process only reads, it never places or
//! cancels anything itself. That's a deliberate scope boundary, not an
//! oversight, see the module doc on `risk_engine` for why.
//!
//! Polls Boros's REST API on an interval, computes a shadow health ratio
//! locally with `margin-sim`, compares it against the API's own precomputed
//! number via `risk_engine::check_health_ratio_divergence`, and trips a
//! `KillSwitch` if the real health ratio drops below a conservative
//! threshold. What "tripped" actually DOES beyond logging and exposing
//! state on `/health` is intentionally left as an extension point, see
//! `KillAction` below, wiring it to actually cancel orders via
//! `execution-adapter`'s gRPC interface is follow-up work, not done here.

mod config;
mod rest;
mod shadow;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use risk_engine::{check_health_ratio_divergence, KillSwitch};
use tokio::sync::RwLock;

use config::RiskMonitorConfig;
use rest::BorosRestClient;

/// What to do when the kill switch trips. Default just logs, loudly.
/// Wiring a real implementation (cancel all orders via
/// `execution-adapter`'s gRPC `cancelOrders`, page someone, whatever) is
/// not built here, this crate shouldn't grow a tonic client
/// and start making irreversible calls without that being a clear,
/// separate, reviewed change, not folded quietly into a monitoring loop.
trait KillAction: Send + Sync {
    fn on_trip(&self, reason: &str);
    fn on_reset(&self, reason: &str);
}

struct LoggingKillAction;
impl KillAction for LoggingKillAction {
    fn on_trip(&self, reason: &str) {
        tracing::error!(%reason, "KILL SWITCH TRIPPED, no automated action wired, this is a log-only alert");
    }
    fn on_reset(&self, reason: &str) {
        tracing::warn!(%reason, "kill switch reset");
    }
}

struct SharedState {
    kill_switch: KillSwitch,
    last_shadow_health_ratio: Option<f64>,
    last_real_health_ratio: Option<f64>,
    last_poll_error: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let cfg = RiskMonitorConfig::from_env();
    tracing::info!(
        root = %cfg.root_address, account_id = cfg.account_id, markets = ?cfg.market_ids,
        poll_interval_secs = cfg.poll_interval.as_secs(),
        "risk-monitor starting"
    );

    let client = BorosRestClient::new(cfg.api_base_url.clone());
    let action: Arc<dyn KillAction> = Arc::new(LoggingKillAction);
    let state = Arc::new(RwLock::new(SharedState {
        kill_switch: KillSwitch::new(),
        last_shadow_health_ratio: None,
        last_real_health_ratio: None,
        last_poll_error: None,
    }));

    let health_state = state.clone();
    let listen_addr = cfg.listen_addr.clone();
    tokio::spawn(async move {
        serve_health(&listen_addr, health_state).await;
    });

    let mut interval = tokio::time::interval(cfg.poll_interval);
    loop {
        interval.tick().await;
        if let Err(e) = poll_once(&cfg, &client, &state, &action).await {
            tracing::warn!("poll failed, will retry next interval: {e}");
            state.write().await.last_poll_error = Some(e.to_string());
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error(transparent)]
    Rest(#[from] rest::RestError),
    #[error(transparent)]
    Margin(#[from] margin_sim::MarginError),
}

async fn poll_once(
    cfg: &RiskMonitorConfig,
    client: &BorosRestClient,
    state: &Arc<RwLock<SharedState>>,
    action: &Arc<dyn KillAction>,
) -> Result<(), PollError> {
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    let mut configs = Vec::new();
    let mut market_states = Vec::new();
    for &market_id in &cfg.market_ids {
        let (margin_cfg, market) = shadow::fetch_margin_config(client, market_id).await?;
        let market_state = shadow::market_state_from_response(&market, now_secs)?;
        configs.push((market_id, margin_cfg));
        market_states.push((market_id, market_state));
    }

    let positions_resp = client.get_positions(&cfg.root_address, cfg.account_id).await?;
    let positions = shadow::parse_positions(&positions_resp)?;

    let collateral = client.get_collateral_summary(&cfg.root_address, cfg.account_id, cfg.token_id).await?;
    // net_balance is a human decimal string, same convention as every
    // other numeric field in this REST API family, parsed the same way
    // shadow.rs parses kIM/kMM. This sidesteps ever needing the packed
    // MarketAcc hex: get_market_acc_cash (rest.rs) is the per-market-acc
    // path that would need it, this account-wide summary doesn't.
    let cash = shadow::parse_decimal_string("netBalance", &collateral.collateral.cross_position.net_balance)?;

    let shadow_ratio = shadow::compute_shadow_health_ratio(configs, market_states, &positions, cash, cfg.token_id)?;

    let real_ratio = collateral.collateral.cross_position.margin_ratio;

    let alert = check_health_ratio_divergence(shadow_ratio, real_ratio, &cfg.divergence);
    if let Some(alert) = &alert {
        tracing::warn!(?alert, "shadow/real health ratio divergence detected");
    }

    let mut s = state.write().await;
    s.last_shadow_health_ratio = Some(shadow_ratio);
    s.last_real_health_ratio = Some(real_ratio);
    s.last_poll_error = None;

    let was_tripped = s.kill_switch.is_tripped();
    if real_ratio < cfg.conservative_health_ratio {
        if !was_tripped {
            let reason = format!("real health ratio {real_ratio:.4} < conservative threshold {:.4}", cfg.conservative_health_ratio);
            s.kill_switch.trip(reason.clone());
            action.on_trip(&reason);
        }
    } else if was_tripped {
        let reason = format!("real health ratio recovered to {real_ratio:.4}");
        s.kill_switch.reset(reason.clone());
        action.on_reset(&reason);
    }

    tracing::info!(shadow = shadow_ratio, real = real_ratio, tripped = s.kill_switch.is_tripped(), "poll complete");
    Ok(())
}

/// Bare-bones `/health` endpoint, no framework, this is one route, axum
/// would be a dependency for the sake of one `match` on a request line.
async fn serve_health(addr: &str, state: Arc<RwLock<SharedState>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind health endpoint on {addr}: {e}");
            return;
        }
    };
    tracing::info!("health endpoint listening on {addr}");

    loop {
        let Ok((mut socket, _)) = listener.accept().await else { continue };
        let state = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            if socket.read(&mut buf).await.is_err() {
                return;
            }
            let s = state.read().await;
            let tripped = s.kill_switch.is_tripped();
            let body = serde_json::json!({
                "tripped": tripped,
                "kill_switch_state": format!("{:?}", s.kill_switch.state()),
                "last_shadow_health_ratio": s.last_shadow_health_ratio,
                "last_real_health_ratio": s.last_real_health_ratio,
                "last_poll_error": s.last_poll_error,
            });
            let status = if tripped { "503 Service Unavailable" } else { "200 OK" };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.to_string().len(), body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}
