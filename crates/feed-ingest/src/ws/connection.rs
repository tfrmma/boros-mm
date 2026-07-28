use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::{config::ReconnectConfig, error::FeedError};

/// Caller sends text frames to the live WS through this.
/// Dropped automatically on disconnect, the caller's send will just fail.
pub type WsSink = mpsc::Sender<String>;

/// Events emitted by the reconnect loop to the caller.
pub enum WsEvent {
    /// A fresh connection is up. Caller should re-subscribe using this writer.
    Connected(WsSink),
    /// Connection dropped. No action needed, reconnect loop handles it.
    Disconnected,
    /// A text frame received from the peer.
    Text(String),
}

/// Manages a WebSocket endpoint with automatic reconnect.
///
/// Start it with `start()`, get back an event receiver. On `Connected(sink)`,
/// use the sink to subscribe. On `Disconnected`, re-subscribe when the next
/// `Connected` arrives, the old sink is already dead.
pub struct WsConnector {
    pub url:                String,
    pub reconnect:          ReconnectConfig,
    pub ping_interval:      Duration,
    pub pong_timeout:       Duration,
    pub write_buf:          usize,
    pub event_buf:          usize,
}

impl WsConnector {
    pub fn start(self) -> mpsc::Receiver<WsEvent> {
        let (event_tx, event_rx) = mpsc::channel(self.event_buf);
        tokio::spawn(async move {
            reconnect_loop(self, event_tx).await;
        });
        event_rx
    }
}

async fn reconnect_loop(cfg: WsConnector, event_tx: mpsc::Sender<WsEvent>) {
    let mut delay_ms = cfg.reconnect.initial_delay_ms;

    loop {
        info!(url = %cfg.url, "ws connecting");

        match connect_async(&cfg.url).await {
            Ok((ws, _resp)) => {
                delay_ms = cfg.reconnect.initial_delay_ms;

                let (write_tx, write_rx) = mpsc::channel::<String>(cfg.write_buf);

                if event_tx.send(WsEvent::Connected(write_tx)).await.is_err() {
                    debug!("event consumer dropped, stopping reconnect loop");
                    return;
                }

                let result = run_session(
                    ws,
                    write_rx,
                    &event_tx,
                    cfg.ping_interval,
                    cfg.pong_timeout,
                ).await;

                match &result {
                    Ok(_)  => debug!("ws session ended cleanly"),
                    Err(e) => warn!("ws session error: {e}"),
                }

                if event_tx.send(WsEvent::Disconnected).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!(url = %cfg.url, "connect failed: {e}");
            }
        }

        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        delay_ms = ((delay_ms as f64) * cfg.reconnect.backoff_multiplier) as u64;
        delay_ms = delay_ms.min(cfg.reconnect.max_delay_ms);
    }
}

/// Runs one WS session until disconnect or error.
async fn run_session<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    mut write_rx: mpsc::Receiver<String>,
    event_tx: &mpsc::Sender<WsEvent>,
    ping_interval: Duration,
    pong_timeout: Duration,
) -> Result<(), FeedError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut ping_tick = tokio::time::interval(ping_interval);
    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            biased; // process incoming msgs before pings to reduce latency

            maybe = ws_rx.next() => {
                match maybe {
                    Some(Ok(msg)) => {
                        on_message(msg, event_tx, &mut ws_tx, &mut last_pong).await?;
                    }
                    Some(Err(e)) => return Err(FeedError::from(e)),
                    None         => return Err(FeedError::WsClosed),
                }
            }

            _ = ping_tick.tick() => {
                if last_pong.elapsed() > pong_timeout {
                    warn!("pong timeout after {:?}", pong_timeout);
                    return Err(FeedError::PongTimeout);
                }
                ws_tx.send(Message::Ping(vec![])).await.map_err(FeedError::from)?;
            }

            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        ws_tx.send(Message::Text(text)).await.map_err(FeedError::from)?;
                    }
                    // write end dropped = caller closed the session intentionally
                    None => return Ok(()),
                }
            }
        }
    }
}

async fn on_message<S>(
    msg: Message,
    event_tx: &mpsc::Sender<WsEvent>,
    ws_tx: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<S>, Message>,
    last_pong: &mut Instant,
) -> Result<(), FeedError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match msg {
        Message::Text(s) => {
            if event_tx.send(WsEvent::Text(s)).await.is_err() {
                return Err(FeedError::BusSend);
            }
        }
        Message::Ping(data) => {
            // protocol-level pong (server pings us)
            ws_tx.send(Message::Pong(data)).await.map_err(FeedError::from)?;
        }
        Message::Pong(_) => {
            *last_pong = Instant::now();
        }
        Message::Close(_) => {
            return Err(FeedError::WsClosed);
        }
        // binary frames not expected from Boros
        Message::Binary(_) | Message::Frame(_) => {}
    }
    Ok(())
}
