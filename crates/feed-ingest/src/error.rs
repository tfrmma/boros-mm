use thiserror::Error;

#[derive(Debug, Error)]
pub enum FeedError {
    #[error("websocket closed")]
    WsClosed,

    #[error("pong timeout")]
    PongTimeout,

    #[error("failed to forward event to consumer channel")]
    BusSend,

    #[error("websocket transport: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("json decode: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket.io: {0}")]
    SocketIo(#[from] rust_socketio::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("bad FixedX18 raw value '{0}'")]
    FixedX18(String),
}
