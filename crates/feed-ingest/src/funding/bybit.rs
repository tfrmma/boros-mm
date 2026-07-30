//! Bybit v5 Linear perpetuals funding rate via `tickers.{symbol}` WS stream.
//!
//! Bybit pushes fundingRate + nextFundingTime + fundingIntervalHour in the
//! tickers snapshot/delta, per the current published example
//! (`bybit-exchange.github.io/docs/v5/websocket/public/ticker`). Read
//! directly, not assumed uniform: some Bybit markets settle every 4h or 1h
//! instead of the common 8h. 8h is only the fallback for a delta message
//! that updates the rate without repeating the interval field.
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

    // real per-symbol field now (see module doc), 8h is only the fallback
    // for a delta that updates the rate without repeating this field
    let interval_secs: u64 = data.funding_interval_hour
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|hours| hours * 3_600)
        .unwrap_or(28_800);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let _ = tx.send(FundingRateEvent {
        venue:           Venue::Bybit,
        symbol:          data.symbol,
        rate,
        interval_secs,
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
    #[serde(rename = "fundingIntervalHour")]
    funding_interval_hour: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_reads_the_real_funding_interval_hour_field() {
        let (tx, mut rx) = broadcast::channel(4);
        let snapshot = serde_json::json!({
            "topic": "tickers.BTCUSDT", "type": "snapshot",
            "data": { "symbol": "BTCUSDT", "fundingRate": "-0.005", "nextFundingTime": "1760342400000", "fundingIntervalHour": "8" }
        }).to_string();
        dispatch(&snapshot, &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.interval_secs, 8 * 3_600);
    }

    #[tokio::test]
    async fn dispatch_reads_a_non_default_interval_instead_of_assuming_8h() {
        let (tx, mut rx) = broadcast::channel(4);
        // exactly the case the module doc warns about: not every market is 8h
        let snapshot = serde_json::json!({
            "topic": "tickers.SOMEUSDT", "type": "snapshot",
            "data": { "symbol": "SOMEUSDT", "fundingRate": "0.0001", "nextFundingTime": "0", "fundingIntervalHour": "4" }
        }).to_string();
        dispatch(&snapshot, &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.interval_secs, 4 * 3_600);
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_8h_only_when_the_field_is_genuinely_missing() {
        let (tx, mut rx) = broadcast::channel(4);
        // a delta that updates the rate without repeating fundingIntervalHour
        let delta = serde_json::json!({
            "topic": "tickers.BTCUSDT", "type": "delta",
            "data": { "symbol": "BTCUSDT", "fundingRate": "0.0002" }
        }).to_string();
        dispatch(&delta, &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.interval_secs, 28_800);
    }

    #[tokio::test]
    async fn dispatch_ignores_op_level_messages() {
        let (tx, mut rx) = broadcast::channel(4);
        let ack = serde_json::json!({ "op": "subscribe", "success": true }).to_string();
        dispatch(&ack, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_ignores_a_ticker_delta_with_no_funding_rate() {
        let (tx, mut rx) = broadcast::channel(4);
        // e.g. a price-only tick, funding fields aren't in every delta
        let delta = serde_json::json!({
            "topic": "tickers.BTCUSDT", "type": "delta",
            "data": { "symbol": "BTCUSDT" }
        }).to_string();
        dispatch(&delta, &tx);
        assert!(rx.try_recv().is_err());
    }
}
