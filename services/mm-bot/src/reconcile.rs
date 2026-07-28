//! Periodic REST reconciliation, separate from the (faster) quote cycle.
//! Refreshes account-wide position/cash state and each market's margin
//! config + mark rate, and computes this bot's OWN shadow health ratio
//! for its local kill switch, independent of `services/risk-monitor`
//! (defense in depth, see main.rs's module doc for why that's on purpose,
//! not duplication for its own sake).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use margin_sim::{MarginAccount, MarginConfig, MarginEngine, MarginMode, MarketId, MarketState, Position, SubaccountId, TokenId};
use tick_math::FixedX18;

use crate::rest::{BorosRestClient, RestError};
use crate::state::{AccountState, MarketRuntime};

fn parse_decimal(field: &str, s: &str) -> Result<FixedX18, RestError> {
    // same deliberate f64-roundtrip tradeoff as risk-monitor's shadow.rs,
    // this is a shadow/monitoring comparison, not on-chain-critical math
    s.parse::<f64>().map(FixedX18::from_f64).map_err(|_| RestError::BadFixedString(format!("{field}={s}")))
}

pub async fn reconcile_account(client: &BorosRestClient, root: &str, account_id: u32) -> Result<AccountState, RestError> {
    let positions_resp = client.get_positions(root, account_id).await?;
    let mut positions = HashMap::with_capacity(positions_resp.results.len());
    for p in &positions_resp.results {
        positions.insert(p.market_id, parse_decimal("notionalSize", &p.notional_size)?);
    }
    Ok(AccountState { cash: FixedX18::ZERO, positions }) // cash filled in by caller from collateral summary, see main.rs
}

/// Refresh one market's `MarginConfig` and `MarketState` from REST.
/// `k_i_thresh` derived from `iTickThresh`/`tickStep` via `tick_to_rate`,
/// same derivation as `risk-monitor/src/shadow.rs`, see that file's doc
/// comment for the citation (`TICK_BASE` = `1.00005` matching the public
/// "boros-docs" `RateFloor` formula).
pub async fn reconcile_market(client: &BorosRestClient, market_id: u32) -> Result<(MarginConfig, MarketState, u8), RestError> {
    let market = client.get_market(market_id).await?;

    let k_i_thresh = tick_math::tick_to_rate(market.im_data.i_tick_thresh, market.im_data.tick_step)
        .map_err(|_| RestError::BadFixedString(format!("iTickThresh={} tickStep={}", market.im_data.i_tick_thresh, market.im_data.tick_step)))?;

    let margin_config = MarginConfig {
        k_im: parse_decimal("kIM", &market.config.k_im)?,
        k_mm: parse_decimal("kMM", &market.config.k_mm)?,
        k_i_thresh,
        t_thresh: market.config.t_thresh,
        token_id: TokenId(market.token_id),
    };

    let mark_rate = match &market.data {
        Some(d) => FixedX18::from_f64(d.mark_apr),
        None => FixedX18::ZERO,
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let ttm = market.im_data.maturity.saturating_sub(now) as u32;

    Ok((margin_config, MarketState { mark_rate, time_to_maturity_secs: ttm }, market.im_data.tick_step))
}

/// This bot's own shadow health ratio, independent of `services/risk-monitor`.
/// Same margin-sim computation, run from this process's own REST-fetched
/// state, not shared memory or an RPC to the other process, on purpose.
pub fn compute_local_health_ratio(
    runtimes: &HashMap<u32, MarketRuntime>,
    account: &AccountState,
    token_id: u32,
) -> Result<f64, margin_sim::MarginError> {
    let configs: HashMap<MarketId, MarginConfig> = runtimes.iter().map(|(&id, r)| (MarketId(id), r.margin_config)).collect();
    let market_states: HashMap<MarketId, MarketState> = runtimes.iter().map(|(&id, r)| (MarketId(id), r.market_state)).collect();
    let engine = MarginEngine::new(configs, market_states);

    let margin_account = MarginAccount {
        subaccount_id: SubaccountId::DEFAULT,
        token_id: TokenId(token_id),
        margin_mode: MarginMode::Cross,
        cash: account.cash,
        positions: runtimes.keys()
            .map(|&id| Position { market_id: MarketId(id), size: account.position(id) })
            .filter(|p| !p.size.is_zero())
            .collect(),
        open_orders: runtimes.values()
            .flat_map(|r| {
                let bid = r.resting_bid.map(|(_, rate)| margin_sim::OpenOrder {
                    market_id: MarketId(r.config.market_id),
                    side: margin_sim::OrderSide::Long,
                    size: FixedX18::from_f64(r.config.base_quote_size),
                    rate,
                });
                let ask = r.resting_ask.map(|(_, rate)| margin_sim::OpenOrder {
                    market_id: MarketId(r.config.market_id),
                    side: margin_sim::OrderSide::Short,
                    size: FixedX18::from_f64(r.config.base_quote_size),
                    rate,
                });
                [bid, ask].into_iter().flatten()
            })
            .collect(),
        last_settled_at: 0,
    };

    Ok(engine.compute_account_state(&margin_account)?.health_ratio)
}
