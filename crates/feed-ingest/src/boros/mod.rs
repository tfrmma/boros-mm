//! Boros CLOB feed over Socket.IO, not raw WS. Protocol: `emit('subscribe',
//! channel)`, then listen on `{channel}:update` (or a fixed event name for
//! market-data/account-updates). Snapshot-only orderbook, no sequence
//! numbers. Verified against
//! docs.pendle.finance/boros-dev/Backend/websocket.
//!
//! Uses the `rust_socketio` crate instead of hand-rolling Engine.IO framing
//! on top of `ws/connection.rs`'s tokio-tungstenite loop. The engine.io path
//! (`/socket/socket.io`, non-default) comes from the URL string passed to
//! `ClientBuilder::new`, not a separate builder option.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::FutureExt;
use rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Event, Payload,
};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{
    config::BorosConfig,
    error::FeedError,
    event::{
        AccountUpdateEvent, BookEvent, MarketDataUpdate, MarketStatisticsEvent, MarketTradeUpdate,
        MarkRateEvent, OrderUpdate, OrderbookUpdate, PositionUpdate, SettlementUpdate,
        StatisticsUpdate, TickSize, TradeEvent,
    },
};

/// Which channel a given `*:update` event name resolves to, precomputed once
/// from config so the dispatch function doesn't re-parse channel strings on
/// every message.
#[derive(Clone)]
enum Route {
    Orderbook { market_id: u32, tick_size: TickSize, include_amm: bool },
    MarketTrade { market_id: u32 },
    Statistics { market_id: u32 },
}

struct SubscriptionPlan {
    /// channel names to `emit('subscribe', ..)` on connect
    channels: Vec<String>,
    /// exact update-event-name -> route, for the dynamic (per-market) events
    routes: HashMap<String, Route>,
}

fn build_plan(cfg: &BorosConfig) -> SubscriptionPlan {
    let mut channels = Vec::new();
    let mut routes = HashMap::new();

    for m in &cfg.markets {
        if m.subscribe_orderbook {
            match TickSize::new(m.tick_size) {
                Ok(tick_size) => {
                    let channel = format!("orderbook:{}:{}", m.market_id, m.tick_size);
                    routes.insert(
                        format!("{channel}:update"),
                        Route::Orderbook { market_id: m.market_id, tick_size, include_amm: false },
                    );
                    channels.push(channel);
                }
                Err(e) => error!("market {} orderbook subscription skipped: {e}", m.market_id),
            }
        }
        if m.subscribe_orderbook_amm {
            match TickSize::new(m.tick_size) {
                Ok(tick_size) => {
                    let channel = format!("orderbook-include-amm:{}:{}", m.market_id, m.tick_size);
                    // per the doc this event name is the fixed "orderbook-include-amm-update",
                    // not "{channel}:update" like the plain orderbook channel
                    routes.insert(
                        "orderbook-include-amm-update".to_owned(),
                        Route::Orderbook { market_id: m.market_id, tick_size, include_amm: true },
                    );
                    channels.push(channel);
                }
                Err(e) => error!("market {} orderbook-amm subscription skipped: {e}", m.market_id),
            }
        }
        if m.subscribe_trades {
            let channel = format!("market-trade:{}", m.market_id);
            routes.insert(format!("{channel}:update"), Route::MarketTrade { market_id: m.market_id });
            channels.push(channel);
        }
        if m.subscribe_statistics {
            let channel = format!("statistics:{}", m.market_id);
            routes.insert(format!("{channel}:update"), Route::Statistics { market_id: m.market_id });
            channels.push(channel);
        }
        if m.subscribe_market_data {
            // fixed event name "market-data-update" regardless of market_id,
            // payload itself carries mId, handled separately below, not via `routes`
            channels.push(format!("market-data:{}", m.market_id));
        }
    }

    if let Some(acc) = &cfg.account {
        channels.push(format!("account-updates:{}", acc.root_address));
    }

    SubscriptionPlan { channels, routes }
}

pub struct BorosFeedHandler {
    cfg: BorosConfig,
    book_tx: broadcast::Sender<BookEvent>,
    mark_tx: broadcast::Sender<MarkRateEvent>,
    trade_tx: broadcast::Sender<TradeEvent>,
    stats_tx: broadcast::Sender<MarketStatisticsEvent>,
    account_tx: broadcast::Sender<AccountUpdateEvent>,
}

impl BorosFeedHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: BorosConfig,
        book_tx: broadcast::Sender<BookEvent>,
        mark_tx: broadcast::Sender<MarkRateEvent>,
        trade_tx: broadcast::Sender<TradeEvent>,
        stats_tx: broadcast::Sender<MarketStatisticsEvent>,
        account_tx: broadcast::Sender<AccountUpdateEvent>,
    ) -> Self {
        Self { cfg, book_tx, mark_tx, trade_tx, stats_tx, account_tx }
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            if let Err(e) = self.run().await {
                error!("boros feed handler exited: {e}");
            }
        });
    }

    async fn run(self) -> Result<(), FeedError> {
        let plan = Arc::new(build_plan(&self.cfg));
        let book_tx = self.book_tx;
        let mark_tx = self.mark_tx;
        let trade_tx = self.trade_tx;
        let stats_tx = self.stats_tx;
        let account_tx = self.account_tx;

        let subscribe_plan = plan.clone();
        let on_connect = move |_payload: Payload, socket: Client| {
            let plan = subscribe_plan.clone();
            async move {
                info!("boros socket.io connected, subscribing {} channels", plan.channels.len());
                for channel in &plan.channels {
                    if let Err(e) = socket.emit("subscribe", json!(channel)).await {
                        error!("subscribe to {channel} failed: {e}");
                    }
                }
            }
            .boxed()
        };

        let dispatch_plan = plan.clone();
        let on_any = move |event: Event, payload: Payload, _socket: Client| {
            let plan = dispatch_plan.clone();
            let book_tx = book_tx.clone();
            let mark_tx = mark_tx.clone();
            let trade_tx = trade_tx.clone();
            let stats_tx = stats_tx.clone();
            let account_tx = account_tx.clone();
            async move {
                let name = String::from(event);
                let value = match payload {
                    Payload::Text(mut values) if !values.is_empty() => values.remove(0),
                    Payload::Text(_) => {
                        debug!("empty Text payload for {name}, skipping");
                        return;
                    }
                    // Payload::String is deprecated upstream in favor of Payload::Text,
                    // but the crate hasn't removed it, still handling it defensively
                    // in case a server or an older client on the other end sends it
                    #[allow(deprecated)]
                    Payload::String(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!("non-JSON String payload for {name}: {e}");
                            return;
                        }
                    },
                    Payload::Binary(_) => {
                        debug!("unexpected binary payload for {name}, skipping");
                        return;
                    }
                };

                dispatch(&name, value, &plan, &book_tx, &mark_tx, &trade_tx, &stats_tx, &account_tx);
            }
            .boxed()
        };

        let mut builder = ClientBuilder::new(self.cfg.ws_url.clone())
            .namespace(self.cfg.namespace.clone())
            .reconnect(true)
            .reconnect_delay(self.cfg.reconnect.initial_delay_ms, self.cfg.reconnect.max_delay_ms)
            .reconnect_on_disconnect(true)
            .on("connect", on_connect)
            .on_any(on_any)
            .on("error", |err, _| {
                async move { warn!("boros socket.io reported an error event: {err:?}") }.boxed()
            });

        if let Some(attempts) = self.cfg.max_reconnect_attempts {
            builder = builder.max_reconnect_attempts(attempts);
        }

        // connect() blocks until the initial handshake completes and spawns its
        // own polling task internally; we just need to keep this task alive so
        // the client (and its callbacks) don't get dropped.
        let _client = builder.connect().await?;

        std::future::pending::<()>().await;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    name: &str,
    value: serde_json::Value,
    plan: &SubscriptionPlan,
    book_tx: &broadcast::Sender<BookEvent>,
    mark_tx: &broadcast::Sender<MarkRateEvent>,
    trade_tx: &broadcast::Sender<TradeEvent>,
    stats_tx: &broadcast::Sender<MarketStatisticsEvent>,
    account_tx: &broadcast::Sender<AccountUpdateEvent>,
) {
    if let Some(route) = plan.routes.get(name) {
        match route {
            Route::Orderbook { market_id, tick_size, include_amm } => {
                match serde_json::from_value::<OrderbookUpdate>(value) {
                    Ok(u) => {
                        let _ = book_tx.send(BookEvent {
                            market_id: *market_id,
                            tick_size: *tick_size,
                            include_amm: *include_amm,
                            long: u.long,
                            short: u.short,
                            sync_status: u.sync_status,
                        });
                    }
                    Err(e) => debug!("orderbook decode failed for {name}: {e}"),
                }
            }
            Route::MarketTrade { market_id } => {
                match serde_json::from_value::<MarketTradeUpdate>(value) {
                    Ok(t) => {
                        let _ = trade_tx.send(TradeEvent {
                            market_id: *market_id,
                            rate: t.rate,
                            size: t.size,
                            block_timestamp: t.block_timestamp,
                            tx_hash: t.tx_hash,
                            side: None,
                        });
                    }
                    Err(e) => debug!("market-trade decode failed for {name}: {e}"),
                }
            }
            Route::Statistics { market_id } => {
                match serde_json::from_value::<StatisticsUpdate>(value) {
                    Ok(stats) => {
                        let _ = stats_tx.send(MarketStatisticsEvent { market_id: *market_id, stats });
                    }
                    Err(e) => debug!("statistics decode failed for {name}: {e}"),
                }
            }
        }
        return;
    }

    match name {
        "market-data-update" => match serde_json::from_value::<MarketDataUpdate>(value) {
            Ok(m) => {
                let _ = mark_tx.send(MarkRateEvent::from(&m));
            }
            Err(e) => debug!("market-data-update decode failed: {e}"),
        },
        "position-update" => match serde_json::from_value::<PositionUpdate>(value) {
            Ok(p) => {
                let _ = account_tx.send(AccountUpdateEvent::Position(p));
            }
            Err(e) => debug!("position-update decode failed: {e}"),
        },
        "order-update" => match serde_json::from_value::<OrderUpdate>(value) {
            Ok(o) => {
                let _ = account_tx.send(AccountUpdateEvent::Order(o));
            }
            Err(e) => debug!("order-update decode failed: {e}"),
        },
        "settlement-update" => match serde_json::from_value::<SettlementUpdate>(value) {
            Ok(s) => {
                let _ = account_tx.send(AccountUpdateEvent::Settlement(s));
            }
            Err(e) => debug!("settlement-update decode failed: {e}"),
        },
        other => debug!("unhandled boros event: {other}"),
    }
}
