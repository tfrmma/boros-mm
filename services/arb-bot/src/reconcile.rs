//! Periodic REST reconciliation, separate from the (faster) signal-scan
//! cycle. Refreshes account-wide position/cash state and each market's
//! margin config + mark rate, and computes this bot's OWN shadow health
//! ratio for its local kill switch, independent of `services/risk-monitor`
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

/// Pure parsing/derivation half of `reconcile_market`, split out so it can
/// be unit tested against a fixture response instead of a live REST call
/// (same split `services/risk-monitor/src/shadow.rs` uses between
/// `fetch_margin_config`'s network call and `market_state_from_response`).
fn market_config_from_response(market: &crate::rest::MarketResponse, now_secs: u64) -> Result<(MarginConfig, MarketState, u8), RestError> {
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
    let ttm = market.im_data.maturity.saturating_sub(now_secs) as u32;

    Ok((margin_config, MarketState { mark_rate, time_to_maturity_secs: ttm }, market.im_data.tick_step))
}

/// Refresh one market's `MarginConfig` and `MarketState` from REST.
/// `k_i_thresh` derived from `iTickThresh`/`tickStep` via `tick_to_rate`,
/// same derivation as `risk-monitor/src/shadow.rs`, see that file's doc
/// comment for the citation (`TICK_BASE` = `1.00005` matching the public
/// "boros-docs" `RateFloor` formula).
pub async fn reconcile_market(client: &BorosRestClient, market_id: u32) -> Result<(MarginConfig, MarketState, u8), RestError> {
    let market = client.get_market(market_id).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    market_config_from_response(&market, now)
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
            .flat_map(|r| r.resting_legs.iter().map(move |leg| margin_sim::OpenOrder {
                market_id: MarketId(r.config.market_id),
                side: match leg.side {
                    oms_core::Side::Long => margin_sim::OrderSide::Long,
                    oms_core::Side::Short => margin_sim::OrderSide::Short,
                },
                size: FixedX18::from_f64(r.config.base_size),
                rate: leg.rate,
            }))
            .collect(),
        last_settled_at: 0,
    };

    Ok(engine.compute_account_state(&margin_account)?.health_ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MarketConfig;
    use crate::rest::{MarketConfigResponse, MarketDataFieldsResponse, MarketImDataResponse, LiqSettingsResponse, MarketResponse};
    use crate::state::MarketRuntime;

    #[test]
    fn parse_decimal_handles_typical_rate_values() {
        assert_eq!(parse_decimal("x", "0.05").unwrap(), FixedX18::from_f64(0.05));
        assert_eq!(parse_decimal("x", "-0.02").unwrap(), FixedX18::from_f64(-0.02));
    }

    #[test]
    fn parse_decimal_rejects_garbage() {
        assert!(parse_decimal("x", "not a number").is_err());
    }

    fn market_response_fixture(maturity: u64, mark_apr: Option<f64>) -> MarketResponse {
        MarketResponse {
            market_id: 1,
            token_id: 3,
            im_data: MarketImDataResponse { maturity, tick_step: 1, i_tick_thresh: 20 },
            config: MarketConfigResponse {
                liq_settings: LiqSettingsResponse { base: "0.25".to_owned(), slope: "0.5".to_owned(), fee_rate: "0.1".to_owned() },
                k_im: "0.1".to_owned(),
                k_mm: "0.05".to_owned(),
                t_thresh: 86_400,
            },
            data: mark_apr.map(|mark_apr| MarketDataFieldsResponse { mark_apr }),
        }
    }

    #[test]
    fn market_config_from_response_saturates_ttm_at_zero_for_matured_markets() {
        let market = market_response_fixture(1000, Some(0.05));
        let (_, state, _) = market_config_from_response(&market, 2000).unwrap();
        assert_eq!(state.time_to_maturity_secs, 0, "matured market must not go negative");
    }

    #[test]
    fn market_config_from_response_computes_positive_ttm() {
        let market = market_response_fixture(5000, Some(0.05));
        let (_, state, _) = market_config_from_response(&market, 2000).unwrap();
        assert_eq!(state.time_to_maturity_secs, 3000);
    }

    #[test]
    fn market_config_from_response_defaults_mark_rate_to_zero_without_data_field() {
        // unlike risk-monitor's shadow.rs (which errors), arb-bot's own reconcile
        // has always defaulted a missing `data` field to a zero mark rate, see
        // the original (pre-split) function body, kept as-is by this refactor
        let market = market_response_fixture(5000, None);
        let (_, state, _) = market_config_from_response(&market, 2000).unwrap();
        assert_eq!(state.mark_rate, FixedX18::ZERO);
    }

    #[test]
    fn market_config_from_response_carries_token_id_and_tick_step() {
        let market = market_response_fixture(5000, Some(0.05));
        let (margin_config, _, tick_step) = market_config_from_response(&market, 2000).unwrap();
        assert_eq!(margin_config.token_id, TokenId(3));
        assert_eq!(tick_step, 1);
    }

    #[test]
    fn market_config_from_response_rejects_invalid_decimal_fields() {
        let mut market = market_response_fixture(5000, Some(0.05));
        market.config.k_im = "garbage".to_owned();
        assert!(market_config_from_response(&market, 2000).is_err());
    }

    fn runtime_fixture(market_id: u32, mark_rate: f64, ttm_secs: u32) -> MarketRuntime {
        MarketRuntime::new(
            MarketConfig { market_id, market_acc: "0xacc".to_owned(), feed_tick_size: 0.01, tick_step: 1, base_size: 100.0, cex_reference: None },
            MarginConfig { k_im: FixedX18::from_f64(0.1), k_mm: FixedX18::from_f64(0.05), k_i_thresh: FixedX18::from_f64(0.001), t_thresh: 86_400, token_id: TokenId(0) },
            MarketState { mark_rate: FixedX18::from_f64(mark_rate), time_to_maturity_secs: ttm_secs },
        )
    }

    #[test]
    fn compute_local_health_ratio_is_infinite_with_no_positions() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 0.05, 30 * 86_400));
        let account = AccountState::default();

        let health_ratio = compute_local_health_ratio(&runtimes, &account, 0).unwrap();
        assert!(health_ratio.is_infinite(), "an account with no positions/orders should have no maintenance margin requirement");
    }

    #[test]
    fn compute_local_health_ratio_finite_with_an_open_position() {
        let mut runtimes = HashMap::new();
        runtimes.insert(1, runtime_fixture(1, 0.05, 30 * 86_400));
        let mut account = AccountState::default();
        account.positions.insert(1, FixedX18::from_f64(1000.0));

        let health_ratio = compute_local_health_ratio(&runtimes, &account, 0).unwrap();
        assert!(health_ratio.is_finite());
    }
}
