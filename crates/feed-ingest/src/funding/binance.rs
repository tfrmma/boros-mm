//! Binance USDM Futures funding rate via `<symbol>@markPrice` WS stream.
//!
//! Binance pushes mark price + next funding rate every 3 seconds.
//! No REST polling needed.
//!
//! WS endpoint: wss://fstream.binance.com/stream?streams=<sym>@markPrice/...

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

pub struct BinanceFundingFeed {
    pub cfg: FundingSourceConfig,
}

#[async_trait]
impl FundingSource for BinanceFundingFeed {
    async fn run(self, tx: broadcast::Sender<FundingRateEvent>) -> Result<(), FeedError> {
        // combined stream URL: /stream?streams=btcusdt@markPrice/ethusdt@markPrice
        let streams: String = self
            .cfg
            .symbols
            .iter()
            .map(|s| format!("{}@markPrice", s.to_lowercase()))
            .collect::<Vec<_>>()
            .join("/");

        let url = format!("{}?streams={streams}", self.cfg.ws_url);

        let connector = WsConnector {
            url:           url.clone(),
            reconnect:     self.cfg.reconnect.clone(),
            ping_interval: Duration::from_secs(20),
            pong_timeout:  Duration::from_millis(5_000),
            write_buf:     self.cfg.write_buf,
            event_buf:     self.cfg.event_buf,
        };

        let mut ws_rx = connector.start();

        while let Some(event) = ws_rx.recv().await {
            match event {
                WsEvent::Connected(_) => debug!("binance funding ws connected"),
                WsEvent::Disconnected => warn!("binance funding ws disconnected"),
                WsEvent::Text(text)   => dispatch(&text, &tx),
            }
        }

        Ok(())
    }
}

fn dispatch(text: &str, tx: &broadcast::Sender<FundingRateEvent>) {
    // combined stream wraps events: {"stream":"btcusdt@markPrice","data":{...}}
    let envelope: StreamEnvelope = match serde_json::from_str(text) {
        Ok(e)  => e,
        Err(e) => {
            debug!("binance parse: {e}, {}", &text[..text.len().min(120)]);
            return;
        }
    };

    if envelope.data.event_type != "markPriceUpdate" {
        return;
    }

    let rate: f64 = match envelope.data.funding_rate.parse() {
        Ok(r)  => r,
        Err(e) => {
            warn!("binance bad funding_rate '{}': {e}", envelope.data.funding_rate);
            return;
        }
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let ev = FundingRateEvent {
        venue:           Venue::Binance,
        symbol:          envelope.data.symbol,
        rate,
        interval_secs:   28_800, // binance is 8h
        next_funding_ts: envelope.data.next_funding_time,
        fetched_at_ms:   now_ms,
    };

    // broadcast; ignore if no consumers (Lagged is also fine, we don't block)
    let _ = tx.send(ev);
}

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamEnvelope {
    #[allow(dead_code)]
    stream: String,
    data:   MarkPriceEvent,
}

#[derive(Deserialize)]
struct MarkPriceEvent {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s")]
    symbol: String,
    /// Next funding rate as a decimal string, e.g. "0.00010000"
    #[serde(rename = "r")]
    funding_rate: String,
    /// Next funding time, unix ms
    #[serde(rename = "T")]
    next_funding_time: u64,
}
