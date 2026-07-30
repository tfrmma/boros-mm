//! OKX v5 public `funding-rate` channel.
//!
//! Connection/subscribe mechanics are solid: public endpoint
//! `wss://ws.okx.com:8443/ws/v5/public`, no auth needed, standard v5
//! envelope (`{"op":"subscribe","args":[{"channel":"funding-rate","instId":...}]}`).
//! `symbols` in config must be OKX's own instrument ID format
//! (`BTC-USDT-SWAP`, not `BTCUSDT`), this crate doesn't reformat them.
//!
//! What's NOT independently confirmed against a live captured payload: the
//! exact field names on the WS push itself. `instId`/`fundingRate` are used
//! consistently across OKX's v5 API (REST and WS alike per their own
//! consistency convention), `nextFundingTime`/`fundingTime` are the REST
//! `/api/v5/public/funding-rate` field names, assumed to carry over to the
//! WS push the same way. Before trusting this the way bybit.rs/binance.rs
//! are trusted, check a real captured payload against the fields below.
//!
//! Funding interval varies by instrument on OKX (moved some pairs to 4h/2h/1h
//! collection in recent changes), don't assume 8h uniformly, `interval_secs`
//! is derived from `next_funding_ts - fetched_at_ms` instead of hardcoded.

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

pub struct OkxFundingFeed {
    pub cfg: FundingSourceConfig,
}

#[async_trait]
impl FundingSource for OkxFundingFeed {
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
                    let args: Vec<serde_json::Value> = symbols.iter()
                        .map(|inst_id| serde_json::json!({ "channel": "funding-rate", "instId": inst_id }))
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
    // OKX sends "pong" as a bare string, not JSON, and event-only messages
    // (subscribe ack, channel-conn-count) that have no "data" array
    if text == "pong" {
        return;
    }

    let msg: OkxMsg = match serde_json::from_str(text) {
        Ok(m)  => m,
        Err(e) => {
            debug!("okx parse: {e}, {}", &text[..text.len().min(120)]);
            return;
        }
    };

    let Some(arg) = msg.arg else { return }; // event-only message (subscribe ack, error, ...)
    if arg.channel != "funding-rate" {
        return;
    }

    let Some(entries) = msg.data else { return };

    for entry in entries {
        let rate: f64 = match entry.funding_rate.parse() {
            Ok(r)  => r,
            Err(e) => { warn!("okx bad fundingRate '{}': {e}", entry.funding_rate); continue; }
        };

        let next_ts: u64 = entry.next_funding_time.parse().unwrap_or(0);
        let this_ts: u64 = entry.funding_time.parse().unwrap_or(0);
        // derived, not hardcoded, OKX funding intervals aren't uniformly 8h
        // across every instrument, see module doc
        let interval_secs = if next_ts > this_ts { (next_ts - this_ts) / 1000 } else { 0 };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let _ = tx.send(FundingRateEvent {
            venue:           Venue::Okx,
            symbol:          entry.inst_id,
            rate,
            interval_secs,
            next_funding_ts: next_ts,
            fetched_at_ms:   now_ms,
        });
    }
}

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OkxMsg {
    arg:  Option<OkxArg>,
    data: Option<Vec<OkxFundingEntry>>,
}

#[derive(Deserialize)]
struct OkxArg {
    channel: String,
}

#[derive(Deserialize)]
struct OkxFundingEntry {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "fundingTime")]
    funding_time: String,
    #[serde(rename = "nextFundingTime")]
    next_funding_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(inst_id: &str, rate: &str, funding_time_ms: u64, next_funding_time_ms: u64) -> String {
        serde_json::json!({
            "arg": { "channel": "funding-rate", "instId": inst_id },
            "data": [{
                "instId": inst_id,
                "fundingRate": rate,
                "fundingTime": funding_time_ms.to_string(),
                "nextFundingTime": next_funding_time_ms.to_string(),
            }]
        }).to_string()
    }

    #[tokio::test]
    async fn dispatch_parses_a_well_formed_funding_rate_push() {
        let (tx, mut rx) = broadcast::channel(4);
        dispatch(&push("BTC-USDT-SWAP", "0.0001", 1_700_000_000_000, 1_700_028_800_000), &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.venue, Venue::Okx);
        assert_eq!(ev.symbol, "BTC-USDT-SWAP");
        assert!((ev.rate - 0.0001).abs() < 1e-12);
        assert_eq!(ev.next_funding_ts, 1_700_028_800_000);
    }

    #[tokio::test]
    async fn dispatch_derives_interval_secs_from_the_two_timestamps_instead_of_assuming_8h() {
        let (tx, mut rx) = broadcast::channel(4);
        // a 4h interval, not the common-case 8h, exactly the case the module
        // doc warns not to assume away
        dispatch(&push("XRP-USDT-SWAP", "0.00005", 0, 4 * 3_600_000), &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.interval_secs, 4 * 3_600);
    }

    #[tokio::test]
    async fn dispatch_ignores_bare_pong() {
        let (tx, mut rx) = broadcast::channel(4);
        dispatch("pong", &tx);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_ignores_subscribe_ack_with_no_data_field() {
        let (tx, mut rx) = broadcast::channel(4);
        let ack = serde_json::json!({ "event": "subscribe", "arg": { "channel": "funding-rate", "instId": "BTC-USDT-SWAP" } }).to_string();
        dispatch(&ack, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_ignores_a_different_channel() {
        let (tx, mut rx) = broadcast::channel(4);
        let other = serde_json::json!({ "arg": { "channel": "tickers" }, "data": [{}] }).to_string();
        dispatch(&other, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_ignores_malformed_json_without_panicking() {
        let (tx, mut rx) = broadcast::channel(4);
        dispatch("not json at all {{{", &tx);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_skips_one_bad_entry_but_still_sends_the_rest() {
        let (tx, mut rx) = broadcast::channel(4);
        let msg = serde_json::json!({
            "arg": { "channel": "funding-rate" },
            "data": [
                { "instId": "BAD-SWAP", "fundingRate": "not-a-number", "fundingTime": "0", "nextFundingTime": "0" },
                { "instId": "BTC-USDT-SWAP", "fundingRate": "0.0002", "fundingTime": "0", "nextFundingTime": "28800000" },
            ]
        }).to_string();
        dispatch(&msg, &tx);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.symbol, "BTC-USDT-SWAP");
        assert!(rx.try_recv().is_err()); // only the one good entry
    }
}
