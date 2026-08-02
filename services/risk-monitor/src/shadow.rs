//! Builds `margin-sim` inputs from the REST responses and runs the shadow
//! health-ratio calc, independent of whatever Boros's own backend computed
//! for the "real" side of the comparison.
//!
//! Doesn't track open orders. `margin-sim`'s `health_ratio` is
//! `total_value / total_mm` (maintenance margin only), and MM doesn't
//! depend on resting orders the way IM does (IM needs worst-case order
//! netting, MM doesn't, per `MarginViewUtils.sol::_calcMM`, it takes
//! `signedSize` directly, no order list).
//! An empty `open_orders` here doesn't understate the number this service
//! actually watches.

use margin_sim::{MarginAccount, MarginConfig, MarginEngine, MarginMode, MarketId, MarketState, Position, SubaccountId, TokenId};
use tick_math::FixedX18;

use crate::rest::{BorosRestClient, MarketResponse, RestError};

pub(crate) fn parse_decimal_string(field_name: &str, s: &str) -> Result<FixedX18, RestError> {
    // REST returns human decimal strings ("0.05"), not the raw-integer
    // convention feed-ingest's parse_fixed_x18_raw expects. f64 roundtrip
    // loses a few bits of precision, acceptable for a shadow/monitoring
    // comparison, this service isn't doing on-chain-critical settlement
    // math, it's sanity-checking against Boros's own precomputed number.
    s.parse::<f64>().map(FixedX18::from_f64).map_err(|_| RestError::BadFixedString(format!("{field_name}={s}")))
}

/// Fetch this market's config and convert it into `margin_sim::MarginConfig`.
///
/// `k_i_thresh` is NOT returned directly by the API as a rate, only
/// `iTickThresh`/`tickStep` (raw tick units). Derived here via
/// `tick_to_rate`, matching the `RateFloor = 1.00005^(iTickThresh*tickStep)-1`
/// formula from the public "boros-docs" detailed-calculations page.
/// `tick_math::TICK_BASE` is the same `1.00005`.
pub async fn fetch_margin_config(client: &BorosRestClient, market_id: u32) -> Result<(MarginConfig, MarketResponse), RestError> {
    let market = client.get_market(market_id).await?;

    let k_i_thresh = tick_math::tick_to_rate(market.im_data.i_tick_thresh, market.im_data.tick_step)
        .map_err(|_| RestError::BadFixedString(format!("iTickThresh={} tickStep={}", market.im_data.i_tick_thresh, market.im_data.tick_step)))?;

    let cfg = MarginConfig {
        k_im: parse_decimal_string("kIM", &market.config.k_im)?,
        k_mm: parse_decimal_string("kMM", &market.config.k_mm)?,
        k_i_thresh,
        t_thresh: market.config.t_thresh,
        token_id: TokenId(market.token_id),
    };
    Ok((cfg, market))
}

/// Builds `margin_sim::MarketState` from the same `MarketResponse` fetch
/// as the config, `time_to_maturity_secs` derived from `maturity` (an
/// absolute epoch timestamp) minus now, saturating at 0 for matured
/// markets instead of going negative.
pub fn market_state_from_response(market: &MarketResponse, now_secs: u64) -> Result<MarketState, RestError> {
    let mark_rate = match &market.data {
        Some(d) => FixedX18::from_f64(d.mark_apr),
        None => return Err(RestError::BadFixedString(format!("market {} has no `data` field, can't get markApr", market.market_id))),
    };
    let time_to_maturity_secs = market.im_data.maturity.saturating_sub(now_secs) as u32;
    Ok(MarketState { mark_rate, time_to_maturity_secs })
}

/// Build the account, run `compute_account_state`, return the shadow
/// health ratio. `mark_rate`/`time_to_maturity_secs` per market come from
/// the caller (this module doesn't fetch market data itself, keeping
/// REST-fetching centralized in `main.rs`'s poll loop instead of spread
/// across two modules).
///
/// `last_settled_at` is the active-positions response's own
/// `syncStatus.timestamp` (see `PositionsInSyncResponse`), the real
/// on-chain-synced-as-of timestamp Boros's API reports for that response,
/// not something this crate computes itself.
pub fn compute_shadow_health_ratio(
    configs: Vec<(u32, MarginConfig)>,
    market_states: Vec<(u32, MarketState)>,
    positions: &[(u32, FixedX18)],
    cash: FixedX18,
    token_id: u32,
    last_settled_at: u64,
) -> Result<f64, margin_sim::MarginError> {
    let engine = MarginEngine::new(
        configs.into_iter().map(|(id, cfg)| (MarketId(id), cfg)).collect(),
        market_states.into_iter().map(|(id, st)| (MarketId(id), st)).collect(),
    );

    let account = MarginAccount {
        subaccount_id: SubaccountId::DEFAULT,
        token_id: TokenId(token_id),
        margin_mode: MarginMode::Cross,
        cash,
        positions: positions.iter().map(|(id, size)| Position { market_id: MarketId(*id), size: *size }).collect(),
        open_orders: vec![], // see module doc, MM doesn't need these
        last_settled_at,
    };

    Ok(engine.compute_account_state(&account)?.health_ratio)
}

/// `notionalSize` isn't as directly confirmed as `kIM`/`kMM`/`netBalance`
/// were, no live payload seen for this specific field. Treated as a human
/// decimal string (not raw-scaled) by consistency with every other numeric
/// string field in this same REST API family (`netBalance`, `maintMargin`,
/// `cash` are all decimal strings, not raw integers), not because this
/// exact field was independently checked. If a real payload shows this
/// field is actually raw-scaled, this is the line to fix.
pub fn parse_positions(positions: &crate::rest::PositionsInSyncResponse) -> Result<Vec<(u32, FixedX18)>, RestError> {
    positions.results.iter()
        .map(|p| parse_decimal_string("notionalSize", &p.notional_size).map(|size| (p.market_id, size)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::{MarketConfigResponse, MarketDataFieldsResponse, MarketImDataResponse, LiqSettingsResponse, PositionInSyncResponse, PositionsInSyncResponse};

    #[test]
    fn parse_decimal_string_handles_typical_rate_values() {
        assert_eq!(parse_decimal_string("x", "0.05").unwrap(), FixedX18::from_f64(0.05));
        assert_eq!(parse_decimal_string("x", "-0.02").unwrap(), FixedX18::from_f64(-0.02));
    }

    #[test]
    fn parse_decimal_string_rejects_garbage() {
        assert!(parse_decimal_string("x", "not a number").is_err());
    }

    #[test]
    fn parse_positions_extracts_market_id_and_size_pairs() {
        let resp = PositionsInSyncResponse {
            results: vec![
                PositionInSyncResponse { market_id: 1, notional_size: "100.5".to_owned() },
                PositionInSyncResponse { market_id: 2, notional_size: "-50.0".to_owned() },
            ],
            sync_status: crate::rest::SyncStatus { block_number: 1, timestamp: 1_760_000_000 },
        };
        let parsed = parse_positions(&resp).unwrap();
        assert_eq!(parsed, vec![(1, FixedX18::from_f64(100.5)), (2, FixedX18::from_f64(-50.0))]);
    }

    #[test]
    fn market_state_from_response_saturates_at_zero_for_matured_markets() {
        let market = market_response_fixture(/* maturity */ 1000, /* now */ 2000);
        let state = market_state_from_response(&market, 2000).unwrap();
        assert_eq!(state.time_to_maturity_secs, 0, "matured market must not go negative");
    }

    #[test]
    fn market_state_from_response_errors_without_data_field() {
        let mut market = market_response_fixture(2000, 1000);
        market.data = None;
        assert!(market_state_from_response(&market, 1000).is_err());
    }

    fn market_response_fixture(maturity: u64, _now: u64) -> crate::rest::MarketResponse {
        crate::rest::MarketResponse {
            market_id: 1,
            token_id: 0,
            im_data: MarketImDataResponse { maturity, tick_step: 1, i_tick_thresh: 20 },
            config: MarketConfigResponse {
                liq_settings: LiqSettingsResponse { base: "0.25".to_owned(), slope: "0.5".to_owned(), fee_rate: "0.1".to_owned() },
                k_im: "0.1".to_owned(),
                k_mm: "0.05".to_owned(),
                t_thresh: 86400,
            },
            data: Some(MarketDataFieldsResponse { mark_apr: 0.05 }),
        }
    }
}
