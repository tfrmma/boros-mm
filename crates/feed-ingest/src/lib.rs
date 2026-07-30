pub mod book;
pub mod config;
pub mod error;
pub mod event;
pub mod state;

pub(crate) mod boros;
pub(crate) mod funding;
pub(crate) mod ws;

// flat re-exports for the common consumer path
pub use book::{BookDepth, Level, OrderBook};
pub use config::{
    BorosConfig, FeedIngestConfig, FundingSourceConfig, MarketFeedConfig, ReconnectConfig,
};
pub use error::FeedError;
pub use event::{
    AccountUpdateEvent, BookEvent, BookLevel, FundingRateEvent, MarketStatisticsEvent,
    MarkRateEvent, TradeSide, TradeEvent, Venue,
};
pub use state::BookStateManager;

use tokio::sync::broadcast;

use boros::BorosFeedHandler;
use funding::{BinanceFundingFeed, BybitFundingFeed, FundingSource, HyperliquidFundingFeed, OkxFundingFeed};

// ── FeedBus ───────────────────────────────────────────────────────────────────

/// Broadcast channels from the feed layer. Clone it freely, each clone
/// shares the same underlying senders.
///
/// Subscribe BEFORE calling `start()` or you'll miss the initial snapshots.
/// Practically: call `subscribe_*()` on the FeedBus returned by `start()`,
/// then start your consumer loop. Events are buffered up to the configured
/// channel capacity, if you fall behind, broadcast returns `Lagged`.
///
/// `statistics`/`account_updates` added 2026-07-19, `account_updates` only
/// ever fires if `BorosConfig.account` is set, no subscribers means no
/// `account-updates:ROOT_ADDRESS` channel was subscribed to in the first
/// place, not silent data loss.
#[derive(Clone)]
pub struct FeedBus {
    pub books:            broadcast::Sender<BookEvent>,
    pub mark_rates:       broadcast::Sender<MarkRateEvent>,
    pub trades:           broadcast::Sender<TradeEvent>,
    pub funding:          broadcast::Sender<FundingRateEvent>,
    pub statistics:       broadcast::Sender<MarketStatisticsEvent>,
    pub account_updates:  broadcast::Sender<AccountUpdateEvent>,
}

impl FeedBus {
    pub fn subscribe_books(&self)           -> broadcast::Receiver<BookEvent>             { self.books.subscribe() }
    pub fn subscribe_mark_rates(&self)      -> broadcast::Receiver<MarkRateEvent>         { self.mark_rates.subscribe() }
    pub fn subscribe_trades(&self)          -> broadcast::Receiver<TradeEvent>            { self.trades.subscribe() }
    pub fn subscribe_funding(&self)         -> broadcast::Receiver<FundingRateEvent>      { self.funding.subscribe() }
    pub fn subscribe_statistics(&self)      -> broadcast::Receiver<MarketStatisticsEvent> { self.statistics.subscribe() }
    pub fn subscribe_account_updates(&self) -> broadcast::Receiver<AccountUpdateEvent>    { self.account_updates.subscribe() }

    /// Convenience: get a ready-to-use BookStateManager.
    pub fn book_state(&self) -> BookStateManager {
        BookStateManager::new(self.subscribe_books())
    }
}

// ── start ─────────────────────────────────────────────────────────────────────

/// Wire up all feed handlers and return the broadcast bus.
///
/// Must be called from within a tokio runtime (it calls tokio::spawn internally).
///
/// Subscribe to channels AFTER calling this, the spawned tasks don't push
/// until they connect, but you don't want to miss the first snapshot.
pub fn start(cfg: FeedIngestConfig) -> FeedBus {
    let (book_tx,    _) = broadcast::channel(cfg.book_channel_capacity);
    let (mark_tx,    _) = broadcast::channel(cfg.mark_rate_channel_capacity);
    let (trade_tx,   _) = broadcast::channel(cfg.trade_channel_capacity);
    let (funding_tx, _) = broadcast::channel(cfg.funding_channel_capacity);
    let (stats_tx,   _) = broadcast::channel(cfg.statistics_channel_capacity);
    let (account_tx, _) = broadcast::channel(cfg.account_channel_capacity);

    // Boros CLOB feed
    BorosFeedHandler::new(
        cfg.boros,
        book_tx.clone(),
        mark_tx.clone(),
        trade_tx.clone(),
        stats_tx.clone(),
        account_tx.clone(),
    ).spawn();

    // per-venue funding feeds
    for source in cfg.funding {
        let tx = funding_tx.clone();
        match source.venue {
            Venue::Binance => {
                tokio::spawn(BinanceFundingFeed { cfg: source }.run(tx));
            }
            Venue::Bybit => {
                tokio::spawn(BybitFundingFeed { cfg: source }.run(tx));
            }
            Venue::Hyperliquid => {
                tokio::spawn(HyperliquidFundingFeed { cfg: source }.run(tx));
            }
            Venue::Okx => {
                tokio::spawn(OkxFundingFeed { cfg: source }.run(tx));
            }
        }
    }

    FeedBus {
        books: book_tx,
        mark_rates: mark_tx,
        trades: trade_tx,
        funding: funding_tx,
        statistics: stats_tx,
        account_updates: account_tx,
    }
}
