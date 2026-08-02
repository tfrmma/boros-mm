//! Drives one replay: feeds `MarkRate`/`Trade` events into `quoting-engine`
//! and `FifoBook`, tracks position and mark-to-market PnL.
//!
//! Two real, named simplifications beyond `fifo_queue`'s own (see that
//! module's doc):
//! - `time_to_maturity_secs` is fixed for the whole replay, not decayed
//!   tick by tick. Real backtests spanning more than a few hours should
//!   feed it as a config sweep across multiple runs instead, or this gets
//!   added properly, not faked here.
//! - PnL is mark-to-market via `size * mark_rate * ttm_years` (same
//!   formula as `margin_sim::Position::value`), not real upfront-fixed-
//!   cost settlement accounting (`oms_core::calc_upfront_fixed_cost`).
//!   Fine for comparing strategies against each other on the same tape,
//!   not a substitute for `settlement-ledger`-accurate accounting.

use std::collections::HashMap;

use oms_core::Side;
use quoting_engine::{AvellanedaStoikovParams, InventoryState, MakerRateBounds, QuoteError, QuotingEngine};
use tick_math::FixedX18;

use crate::event::{BacktestEvent, EventKind};
use crate::fifo_queue::FifoBook;

const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 3_600.0;

#[derive(Debug, Clone, Copy)]
pub struct MarketConfig {
    pub k_i_thresh: FixedX18,
    pub bounds: MakerRateBounds,
    pub time_to_maturity_secs: u32,
    pub quote_size: f64,
    pub requote_threshold: f64,
}

#[derive(Debug, Default, Clone, Copy)]
struct MarketState {
    mark_rate: FixedX18,
    position: f64,
    avg_locked_fixed_rate: Option<f64>,
    resting_bid_rate: Option<FixedX18>,
    resting_ask_rate: Option<FixedX18>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MarketResult {
    pub final_position: f64,
    pub mark_to_market_pnl: f64,
    pub fill_count: u32,
    pub quote_count: u32,
}

pub struct BacktestEngine {
    configs: HashMap<u32, MarketConfig>,
    engine: QuotingEngine,
    books: HashMap<u32, FifoBook>,
    states: HashMap<u32, MarketState>,
    results: HashMap<u32, MarketResult>,
}

impl BacktestEngine {
    pub fn new(params: AvellanedaStoikovParams, configs: HashMap<u32, MarketConfig>) -> Result<Self, QuoteError> {
        let engine = QuotingEngine::new(params)?;
        Ok(Self {
            configs,
            engine,
            books: HashMap::new(),
            states: HashMap::new(),
            results: HashMap::new(),
        })
    }

    pub fn run(mut self, events: impl IntoIterator<Item = BacktestEvent>) -> HashMap<u32, MarketResult> {
        for event in events {
            match event.kind {
                EventKind::MarkRate { market_id, rate } => self.on_mark_rate(market_id, rate),
                EventKind::Trade { market_id, rate, size: _ } => self.on_trade(market_id, rate),
            }
        }
        self.finalize()
    }

    fn on_mark_rate(&mut self, market_id: u32, rate: f64) {
        let Some(&cfg) = self.configs.get(&market_id) else { return }; // unconfigured market, skip

        let state = self.states.entry(market_id).or_default();
        state.mark_rate = FixedX18::from_f64(rate);

        let net_dv01 = state.position.abs() * (cfg.time_to_maturity_secs as f64 / SECONDS_PER_YEAR) * 0.0001 * state.position.signum();
        let inventory = InventoryState { net_dv01, avg_locked_fixed_rate: state.avg_locked_fixed_rate };

        let Ok(quote) = self.engine.quote(state.mark_rate, state.mark_rate, cfg.k_i_thresh, &inventory, &cfg.bounds) else { return };

        let book = self.books.entry(market_id).or_default();
        let result = self.results.entry(market_id).or_default();

        if !rate_close_enough(state.resting_bid_rate, quote.bid_rate, cfg.requote_threshold) {
            book.set_bid(quote.bid_rate, cfg.quote_size);
            state.resting_bid_rate = Some(quote.bid_rate);
            result.quote_count += 1;
        }
        if !rate_close_enough(state.resting_ask_rate, quote.ask_rate, cfg.requote_threshold) {
            book.set_ask(quote.ask_rate, cfg.quote_size);
            state.resting_ask_rate = Some(quote.ask_rate);
            result.quote_count += 1;
        }
    }

    fn on_trade(&mut self, market_id: u32, trade_rate: f64) {
        let Some(book) = self.books.get_mut(&market_id) else { return };
        let fills = book.on_trade(FixedX18::from_f64(trade_rate));
        if fills.is_empty() {
            return;
        }

        let state = self.states.entry(market_id).or_default();
        let result = self.results.entry(market_id).or_default();

        for fill in fills {
            let signed_size = match fill.side { Side::Long => fill.size, Side::Short => -fill.size };
            state.position += signed_size;
            state.avg_locked_fixed_rate = Some(fill.rate.to_f64()); // last-fill, not size-weighted, see module doc for scope
            result.fill_count += 1;

            match fill.side { Side::Long => state.resting_bid_rate = None, Side::Short => state.resting_ask_rate = None }
        }
    }

    fn finalize(self) -> HashMap<u32, MarketResult> {
        let mut results = self.results;
        for (market_id, state) in &self.states {
            let cfg = match self.configs.get(market_id) { Some(c) => c, None => continue };
            let ttm_years = cfg.time_to_maturity_secs as f64 / SECONDS_PER_YEAR;
            let result = results.entry(*market_id).or_default();
            result.final_position = state.position;
            result.mark_to_market_pnl = state.position * state.mark_rate.to_f64() * ttm_years;
        }
        results
    }
}

fn rate_close_enough(current: Option<FixedX18>, target: FixedX18, threshold: f64) -> bool {
    match current {
        None => false,
        Some(c) => (c.to_f64() - target.to_f64()).abs() < threshold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> AvellanedaStoikovParams {
        // gamma/kappa at a realistic scale: the liquidity term in
        // optimal_spread is (2/gamma)*ln(1+gamma/kappa), which blows up
        // for small gamma relative to kappa regardless of horizon, this
        // ratio (gamma/kappa ~ 0.067) keeps the resulting spread in a
        // sane few-bps-to-percent range instead of >100%
        AvellanedaStoikovParams { gamma: 50.0, sigma: 0.02, kappa: 750.0, horizon_secs: 3_600, carry_weight: 0.0 }
    }

    fn wide_bounds() -> MakerRateBounds {
        // wide enough that clamp_bid/clamp_ask never bind in these tests,
        // isolating what's actually under test (the replay wiring)
        MakerRateBounds { lo_upper_slope_base1e4: 30_000, lo_upper_const_base1e4: 10_000, lo_lower_slope_base1e4: 30_000, lo_lower_const_base1e4: 10_000 }
    }

    fn one_market_config() -> HashMap<u32, MarketConfig> {
        let mut configs = HashMap::new();
        configs.insert(1, MarketConfig {
            k_i_thresh: FixedX18::from_f64(0.001),
            bounds: wide_bounds(),
            time_to_maturity_secs: 30 * 86_400,
            quote_size: 100.0,
            requote_threshold: 0.0001,
        });
        configs
    }

    #[test]
    fn a_mark_rate_event_produces_an_initial_two_sided_quote() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 1, rate: 0.05 } }];
        let results = engine.run(events);
        assert_eq!(results[&1].quote_count, 2); // first quote: both bid and ask are new
    }

    #[test]
    fn a_mark_rate_event_for_an_unconfigured_market_is_ignored() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 999, rate: 0.05 } }];
        let results = engine.run(events);
        assert!(!results.contains_key(&999));
    }

    #[test]
    fn a_trade_crossing_the_resting_bid_fills_it_and_moves_position_long() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![
            BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 1, rate: 0.05 } },
            // a trade well below the reservation rate crosses the bid regardless of the exact quote
            BacktestEvent { ts_ms: 1, kind: EventKind::Trade { market_id: 1, rate: 0.0, size: 100.0 } },
        ];
        let results = engine.run(events);
        assert_eq!(results[&1].fill_count, 1);
        assert!(results[&1].final_position > 0.0, "a filled bid should leave a long position");
    }

    #[test]
    fn a_trade_crossing_the_resting_ask_fills_it_and_moves_position_short() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![
            BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 1, rate: 0.05 } },
            BacktestEvent { ts_ms: 1, kind: EventKind::Trade { market_id: 1, rate: 1.0, size: 100.0 } },
        ];
        let results = engine.run(events);
        assert_eq!(results[&1].fill_count, 1);
        assert!(results[&1].final_position < 0.0, "a filled ask should leave a short position");
    }

    #[test]
    fn a_trade_on_an_unquoted_market_produces_no_fill() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![BacktestEvent { ts_ms: 0, kind: EventKind::Trade { market_id: 1, rate: 0.05, size: 100.0 } }];
        let results = engine.run(events);
        assert!(results.get(&1).map(|r| r.fill_count).unwrap_or(0) == 0);
    }

    #[test]
    fn a_mark_rate_move_within_the_requote_threshold_does_not_requote() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![
            BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 1, rate: 0.05 } },
            // a microscopic move, smaller than requote_threshold (0.0001)
            BacktestEvent { ts_ms: 1, kind: EventKind::MarkRate { market_id: 1, rate: 0.05000001 } },
        ];
        let results = engine.run(events);
        assert_eq!(results[&1].quote_count, 2, "second tick should not add any new quotes");
    }

    #[test]
    fn a_mark_rate_move_past_the_requote_threshold_does_requote() {
        let engine = BacktestEngine::new(params(), one_market_config()).unwrap();
        let events = vec![
            BacktestEvent { ts_ms: 0, kind: EventKind::MarkRate { market_id: 1, rate: 0.05 } },
            BacktestEvent { ts_ms: 1, kind: EventKind::MarkRate { market_id: 1, rate: 0.06 } },
        ];
        let results = engine.run(events);
        assert!(results[&1].quote_count > 2, "a real rate move should trigger a requote");
    }
}
