//! Bybit v5 Linear perpetuals funding rate via `tickers.{symbol}` WS stream.
//!
//! Bybit pushes fundingRate + nextFundingTime in the tickers snapshot/delta.
//! Standard funding interval for USDT perps is 8h, verify per symbol if using
//! anything exotic.
//!
//! WS endpoint: wss://stream.bybit.com/v5/public/linear

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

pub struct BybitFundingFeed {
    pub cfg: FundingSourceConfig,
}

#[async_trait]
impl FundingSource for BybitFundingFeed {
    async fn run(self, tx: broadcast::Sender<FundingRateEvent>) -> Result<(), FeedError> {
        let connector = WsConnector {
            url:           self.cfg.ws_url.clone(),
            reconnect:     self.cfg.reconnect.clone(),
            ping_interval: Duration::from_secs(20),
            pong_timeout:  Duration::from_millis(5_000),
            write_buf:     self.cfg.write_buf,
            event_buf:     self.cfg.event_buf,
        };

        let symbols = self.cfg.symbols.clone();
        let mut ws_rx = connector.start();

        while let Some(event) = ws_rx.recv().await {
            match event {
                WsEvent::Connected(sink) => {
                    // Bybit v5: subscribe to tickers for each symbol
                    let args: Vec<String> = symbols.iter()
                        .map(|s| format!("tickers.{s}"))
                        .collect();
                    let req = serde_json::json!({ "op": "subscribe", "args": args });
                    let _ = sink.send(req.to_string()).await;
                }
                WsEvent::Disconnected => {}
                WsEvent::Text(text)   => dispatch(&text, &tx),
            }
        }

        Ok(())
    }
}

fn dispatch(text: &str, tx: &broadcast::Sender<FundingRateEvent>) {
    let msg: BybitTickerMsg = match serde_json::from_str(text) {
        Ok(m)  => m,
        Err(e) => {
            debug!("bybit parse: {e}, {}", &text[..text.len().min(120)]);
            return;
        }
    };

    // op-level response (subscription confirm, heartbeat pong), not a ticker
    let topic = match msg.topic {
        Some(t) => t,
        None    => return,
    };

    if !topic.starts_with("tickers.") {
        return;
    }

    let data = match msg.data {
        Some(d) => d,
        None    => return,
    };

    // fundingRate and nextFundingTime are only present in the snapshot,
    // and occasionally in deltas when they actually change
    let rate_str = match data.funding_rate {
        Some(r) => r,
        None    => return,
    };

    let rate: f64 = match rate_str.parse() {
        Ok(r)  => r,
        Err(e) => { warn!("bybit bad fundingRate '{rate_str}': {e}"); return; }
    };

    let next_ts: u64 = data.next_funding_time
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let _ = tx.send(FundingRateEvent {
        venue:           Venue::Bybit,
        symbol:          data.symbol,
        rate,
        interval_secs:   28_800, // TODO: verify per-symbol; some bybit markets are 4h
        next_funding_ts: next_ts,
        fetched_at_ms:   now_ms,
    });
}

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BybitTickerMsg {
    // op-level fields (subscription result / pong)
    #[allow(dead_code)]
    op:     Option<String>,
    // push event fields
    topic:  Option<String>,
    #[allow(dead_code)]
    ts:     Option<u64>,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    msg_type: Option<String>,
    data:   Option<BybitTickerData>,
}

#[derive(Deserialize)]
struct BybitTickerData {
    #[serde(rename = "symbol")]
    symbol: String,
    #[serde(rename = "fundingRate")]
    funding_rate: Option<String>,
    #[serde(rename = "nextFundingTime")]
    next_funding_time: Option<String>,
}
