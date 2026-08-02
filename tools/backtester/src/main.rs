//! `backtester <events.ndjson> <config.json>`
//!
//! `config.json` shape:
//! ```json
//! {
//!   "sigma": 0.02, "horizon_secs": 3600, "carry_weight": 0.0,
//!   "gamma_sweep": [0.05, 0.1, 0.2],
//!   "kappa_sweep": [1.0, 1.5, 2.0],
//!   "markets": {
//!     "1": {
//!       "k_i_thresh": 0.001,
//!       "bounds": { "lo_upper_slope_base1e4": 100, "lo_upper_const_base1e4": 5,
//!                   "lo_lower_slope_base1e4": 100, "lo_lower_const_base1e4": 5 },
//!       "time_to_maturity_secs": 2592000, "quote_size": 100.0, "requote_threshold": 0.0001
//!     }
//!   }
//! }
//! ```
//! `gamma_sweep`/`kappa_sweep` are optional, a single-element list (or
//! omitting the sweep and setting `gamma`/`kappa` directly) runs one
//! backtest instead of a grid.

mod engine;
mod event;
mod fifo_queue;

use std::collections::HashMap;
use std::fs;

use engine::{BacktestEngine, MarketConfig};
use event::BacktestEvent;
use quoting_engine::AvellanedaStoikovParams;
use serde::Deserialize;
use tick_math::FixedX18;

#[derive(Deserialize)]
struct RawConfig {
    sigma: f64,
    horizon_secs: u32,
    carry_weight: f64,
    #[serde(default)]
    gamma: Option<f64>,
    #[serde(default)]
    kappa: Option<f64>,
    #[serde(default)]
    gamma_sweep: Vec<f64>,
    #[serde(default)]
    kappa_sweep: Vec<f64>,
    markets: HashMap<u32, RawMarketConfig>,
}

#[derive(Deserialize)]
struct RawMarketConfig {
    k_i_thresh: f64,
    bounds: quoting_engine::MakerRateBounds,
    time_to_maturity_secs: u32,
    quote_size: f64,
    requote_threshold: f64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, events_path, config_path] = args.as_slice() else {
        eprintln!("usage: backtester <events.ndjson> <config.json>");
        std::process::exit(1);
    };

    let events_text = fs::read_to_string(events_path).unwrap_or_else(|e| { eprintln!("reading {events_path}: {e}"); std::process::exit(1); });
    let events: Vec<BacktestEvent> = events_text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).map_err(|e| eprintln!("skipping malformed line: {e}")).ok())
        .collect();
    if events.is_empty() {
        eprintln!("no valid events parsed from {events_path}");
        std::process::exit(1);
    }

    let config_text = fs::read_to_string(config_path).unwrap_or_else(|e| { eprintln!("reading {config_path}: {e}"); std::process::exit(1); });
    let raw: RawConfig = serde_json::from_str(&config_text).unwrap_or_else(|e| { eprintln!("parsing {config_path}: {e}"); std::process::exit(1); });

    let market_configs: HashMap<u32, MarketConfig> = raw.markets.iter().map(|(&id, m)| (id, MarketConfig {
        k_i_thresh: FixedX18::from_f64(m.k_i_thresh),
        bounds: m.bounds,
        time_to_maturity_secs: m.time_to_maturity_secs,
        quote_size: m.quote_size,
        requote_threshold: m.requote_threshold,
    })).collect();

    let gammas = if !raw.gamma_sweep.is_empty() { raw.gamma_sweep.clone() } else { vec![raw.gamma.unwrap_or_else(|| { eprintln!("config needs either \"gamma\" or \"gamma_sweep\""); std::process::exit(1); })] };
    let kappas = if !raw.kappa_sweep.is_empty() { raw.kappa_sweep.clone() } else { vec![raw.kappa.unwrap_or_else(|| { eprintln!("config needs either \"kappa\" or \"kappa_sweep\""); std::process::exit(1); })] };

    println!("{:>8} {:>8} {:>10} {:>14} {:>10} {:>8}", "gamma", "kappa", "market", "pnl", "position", "fills");
    for &gamma in &gammas {
        for &kappa in &kappas {
            let params = AvellanedaStoikovParams { gamma, sigma: raw.sigma, kappa, horizon_secs: raw.horizon_secs, carry_weight: raw.carry_weight };
            let engine = match BacktestEngine::new(params, market_configs.clone()) {
                Ok(e) => e,
                Err(e) => { eprintln!("gamma={gamma} kappa={kappa}: invalid params: {e}"); continue; }
            };
            let results = engine.run(events.clone());
            let mut market_ids: Vec<&u32> = results.keys().collect();
            market_ids.sort();
            for market_id in market_ids {
                let r = &results[market_id];
                println!("{gamma:>8.4} {kappa:>8.4} {market_id:>10} {:>14.6} {:>10.2} {:>8}", r.mark_to_market_pnl, r.final_position, r.fill_count);
            }
        }
    }
}
