mod binance;
mod bybit;
mod hyperliquid;

pub use binance::BinanceFundingFeed;
pub use bybit::BybitFundingFeed;
pub use hyperliquid::HyperliquidFundingFeed;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{error::FeedError, event::FundingRateEvent};

/// Implemented by each venue's funding feed. `run` owns the connection loop
/// and pushes rates onto `tx` until the connection dies for good (reconnect
/// handling, if any, happens inside `run` via ws::WsConnector).
#[async_trait]
pub trait FundingSource {
    async fn run(self, tx: broadcast::Sender<FundingRateEvent>) -> Result<(), FeedError>;
}
