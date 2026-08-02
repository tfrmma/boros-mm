//! NDJSON input format: this tool's own, not a Boros-provided export.
//! Boros doesn't document a historical data export at all (checked, no
//! such endpoint exists in the Open API or the WS docs). One
//! `BacktestEvent` per line, ascending `ts_ms`, meant to be produced by a
//! separate recorder tapping `feed-ingest`'s own broadcast channels
//! (`MarkRateEvent`/`MarketTradeUpdate`) and writing them straight to
//! disk, not built here yet.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BacktestEvent {
    pub ts_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A mark rate update for one market, the reference the quoting engine
    /// skews and clamps around.
    MarkRate { market_id: u32, rate: f64 },
    /// A trade print: someone matched at `rate` for `size`. No aggressor
    /// side in the data (Boros's own `MarketTradeUpdate` doesn't carry
    /// one either, see `feed-ingest::event`'s doc comment on that struct),
    /// so fills are simulated by "would this rate have crossed our
    /// resting order", not by aggressor direction. See `fifo_queue`'s
    /// module doc for exactly what that does and doesn't model.
    ///
    /// `size` is currently unused by the fill simulator: a crossed
    /// resting order fills for its own full resting size, not capped by
    /// how much actually traded at that print. Kept in the schema for a
    /// future, more realistic partial-fill model, not dead by oversight.
    Trade { market_id: u32, rate: f64, size: f64 },
}
