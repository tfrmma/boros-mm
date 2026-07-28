//! Hyperliquid funding rate via `activeAssetCtx` WS subscription.
//!
//! Hyperliquid uses 1-hour funding intervals. The `funding` field in the ctx
//! is the HOURLY rate (not annualized, not 8h). annualized = rate * 8760.
//!
//! No separate REST polling needed, HL pushes updates to activeAssetCtx
//! whenever funding or market conditions change.
//!
//! WS endpoint: wss://api.hyperliquid.xyz/ws

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    config::FundingSourceConfig,
    error::FeedError,
    event::{FundingRateEvent, Venue},
    ws::{WsConnector, WsEvent},
};
use super::FundingSource;

pub struct HyperliquidFundingFeed {
    pub cfg: FundingSourceConfig,
}

#[async_trait]
impl FundingSource for HyperliquidFundingFeed {
    async fn run(self, tx: broadcast::Sender<FundingRateEvent>) -> Result<(), FeedError> {
        let connector = WsConnector {
            url:           self.cfg.ws_url.clone(),
            reconnect:     self.cfg.reconnect.clone(),
            ping_interval: Duration::from_secs(30),
            pong_timeout:  Duration::from_millis(10_000),
            write_buf:     self.cfg.write_buf,
            event_buf:     self.cfg.event_buf,
        };

        let symbols = self.cfg.symbols.clone();
        let mut ws_rx = connector.start();

        while let Some(event) = ws_rx.recv().await {
            match event {
                WsEvent::Connected(sink) => {
                    // one subscription per coin, HL doesn't support batch activeAssetCtx
                    for coin in &symbols {
                        let sub = serde_json::json!({
                            "method": "subscribe",
                            "subscription": { "type": "activeAssetCtx", "coin": coin }
                        });
                        let _ = sink.send(sub.to_string()).await;
                    }
                }
                WsEvent::Disconnected => {}
                WsEvent::Text(text)   => dispatch(&text, &tx),
            }
        }

        Ok(())
    }
}

fn dispatch(text: &str, tx: &broadcast::Sender<FundingRateEvent>) {
    let msg: HlMsg = match serde_json::from_str(text) {
        Ok(m)  => m,
        Err(e) => {
            debug!("hl parse: {e}, {}", &text[..text.len().min(120)]);
            return;
        }
    };

    // filter to channel events only
    let ev = match msg {
        HlMsg::Event(e) if e.channel == "activeAssetCtx" => e,
        _ => return,
    };

    let coin    = ev.data.coin;
    let funding = match ev.data.ctx.funding {
        Some(f) => f,
        None    => return,
    };

    let rate: f64 = match funding.parse() {
        Ok(r)  => r,
        Err(e) => { warn!("hl bad funding '{funding}': {e}"); return; }
    };

    // HL funding settles at the top of each hour. next_funding_ts is approximate.
    // We don't have the exact timestamp from the WS push, so compute it ourselves.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // next hourly boundary in unix ms
    const HOUR_MS: u64 = 3_600_000;
    let next_funding_ts = (now_ms / HOUR_MS + 1) * HOUR_MS;

    let _ = tx.send(FundingRateEvent {
        venue:           Venue::Hyperliquid,
        symbol:          coin,
        rate,
        interval_secs:   3_600, // HL is 1h
        next_funding_ts,
        fetched_at_ms:   now_ms,
    });
}

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(untagged)]
enum HlMsg {
    Event(HlEvent),
    // subscription confirmations, errors, etc. never inspected, this variant
    // exists purely so #[serde(untagged)] has somewhere to land instead of
    // failing to parse non-Event messages
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Deserialize)]
struct HlEvent {
    channel: String,
    data:    HlAssetCtxData,
}

#[derive(Deserialize)]
struct HlAssetCtxData {
    coin: String,
    ctx:  HlCtx,
}

#[derive(Deserialize)]
struct HlCtx {
    funding:         Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "markPx")]
    mark_px:         Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "openInterest")]
    open_interest:   Option<String>,
}
