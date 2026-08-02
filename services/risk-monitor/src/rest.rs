//! Boros core REST client. Types and paths below all confirmed 2026-07-19
//! by reading `@pendle/sdk-boros@1.5.0`'s compiled `BorosCoreSDK.js`
//! directly (not just the `.d.ts` declarations, the JS is what actually
//! has the request paths), not the TS SDK itself, this is a plain
//! `reqwest` client hitting the same REST API the SDK wraps. No signing
//! needed for any of these, they're all read-only GETs.

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RestError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("failed to parse a FixedX18-raw string field '{0}'")]
    BadFixedString(String),
}

pub struct BorosRestClient {
    http: reqwest::Client,
    base_url: String,
}

impl BorosRestClient {
    pub fn new(base_url: String) -> Self {
        Self { http: reqwest::Client::new(), base_url }
    }

    /// `GET /v1/markets/{marketId}`, confirmed via
    /// `marketsControllerGetMarketInfo` (`BorosCoreSDK.js:180-185`).
    pub async fn get_market(&self, market_id: u32) -> Result<MarketResponse, RestError> {
        let url = format!("{}/v1/markets/{market_id}", self.base_url);
        Ok(self.http.get(url).send().await?.error_for_status()?.json().await?)
    }

    /// `GET /v1/pnl/positions?root=...&accountId=...`, confirmed via
    /// `pnlControllerGetPositionsInSync` (`BorosCoreSDK.js:632-638`).
    pub async fn get_positions(&self, root: &str, account_id: u32) -> Result<PositionsInSyncResponse, RestError> {
        let url = format!("{}/v1/pnl/positions", self.base_url);
        Ok(self.http.get(url).query(&[("root", root), ("accountId", &account_id.to_string())]).send().await?.error_for_status()?.json().await?)
    }

    /// `GET /v1/pnl/market-acc-cashes?marketAccs=...`, confirmed via
    /// `pnlControllerGetMarketAccCashes` (`BorosCoreSDK.js:639-645`).
    /// `market_acc` is the packed `MarketAcc` hex string (see
    /// `feed-ingest::event::MarketAccRaw`'s doc comment for how that's
    /// confirmed to be a plain hex string, same convention here).
    ///
    /// Not called anywhere yet, `main.rs`'s poll loop sources cash from
    /// the collateral summary's `netBalance` instead, which sidesteps
    /// needing to construct the packed `MarketAcc` hex at all. Kept here
    /// as a correct, documented capability instead of deleted, since the
    /// two aren't necessarily interchangeable for every future use (this
    /// one is per-market-acc, the other is an account-wide cross summary).
    #[allow(dead_code)]
    pub async fn get_market_acc_cash(&self, market_acc: &str) -> Result<MarketAccCashesResponse, RestError> {
        let url = format!("{}/v1/pnl/market-acc-cashes", self.base_url);
        Ok(self.http.get(url).query(&[("marketAccs", market_acc)]).send().await?.error_for_status()?.json().await?)
    }

    /// `GET /v1/collaterals/summary/single?userAddress=...&accountId=...&tokenId=...`,
    /// confirmed via `collateralControllerGetSingleCollateral`
    /// (`BorosCoreSDK.js:671-677`). This is where the REAL (Boros's own
    /// precomputed) `marginRatio` comes from, this call's whole purpose is
    /// producing the "real" side of the shadow-vs-real comparison.
    pub async fn get_collateral_summary(&self, user_address: &str, account_id: u32, token_id: u32) -> Result<SingleCollateralSummaryResponse, RestError> {
        let url = format!("{}/v1/collaterals/summary/single", self.base_url);
        Ok(self.http.get(url)
            .query(&[("userAddress", user_address.to_owned()), ("accountId", account_id.to_string()), ("tokenId", token_id.to_string())])
            .send().await?.error_for_status()?.json().await?)
    }
}

// ── response types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketImDataResponse {
    pub maturity: u64,
    pub tick_step: u8,
    pub i_tick_thresh: i16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiqSettingsResponse {
    pub base: String,
    pub slope: String,
    pub fee_rate: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketConfigResponse {
    pub liq_settings: LiqSettingsResponse,
    pub k_im: String,
    pub k_mm: String,
    pub t_thresh: u32,
}

/// Only the one field this service needs out of the ~20 on the real
/// `MarketDataResponse` (see `feed-ingest::event::StatisticsUpdate` for the
/// rest of that type, cross-verified against the same SDK response
/// 2026-07-19). `markApr` is the same TWAP oracle rate `quoting-engine`
/// and `margin-sim` both key off of.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketDataFieldsResponse {
    pub mark_apr: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketResponse {
    pub market_id: u32,
    pub token_id: u32,
    pub im_data: MarketImDataResponse,
    pub config: MarketConfigResponse,
    /// Optional on the real type (`data?: MarketDataResponse`), no query
    /// param found to force it on, treated as "might not be there" rather
    /// than assumed always populated.
    pub data: Option<MarketDataFieldsResponse>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionInSyncResponse {
    pub market_id: u32,
    pub notional_size: String,
}

/// Confirmed in docs.pendle.finance/boros-dev/Backend/api: `GET
/// /accounts/active-positions`'s response "includes syncStatus
/// (blockNumber + timestamp)". Same shape as
/// `feed-ingest::event::SyncStatus`, this crate doesn't depend on
/// `feed-ingest` (it's REST-only, no WS), so this is its own small copy
/// instead of pulling in that whole crate for one struct.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub block_number: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionsInSyncResponse {
    pub results: Vec<PositionInSyncResponse>,
    pub sync_status: SyncStatus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketAccCashResponse {
    pub market_acc: String,
    pub cash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketAccCashesResponse {
    pub results: Vec<MarketAccCashResponse>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketAccCollateralResponse {
    pub is_cross: bool,
    pub net_balance: String,
    pub maint_margin: String,
    pub margin_ratio: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollateralSummaryResponse {
    pub cross_position: MarketAccCollateralResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SingleCollateralSummaryResponse {
    pub collateral: CollateralSummaryResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_in_sync_response_parses_sync_status_alongside_results() {
        // shape per docs.pendle.finance/boros-dev/Backend/api: "Response
        // includes syncStatus (blockNumber + timestamp)"
        let json = r#"{
            "results": [ { "marketId": 1, "notionalSize": "1000.0" } ],
            "syncStatus": { "blockNumber": 123456789, "timestamp": 1760342400 }
        }"#;
        let parsed: PositionsInSyncResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.sync_status.block_number, 123_456_789);
        assert_eq!(parsed.sync_status.timestamp, 1_760_342_400);
    }

    #[test]
    fn positions_in_sync_response_requires_sync_status() {
        // if this ever regresses to an Option or gets dropped silently,
        // this test catches it: missing syncStatus should fail to parse,
        // not silently default last_settled_at back to 0
        let json = r#"{ "results": [] }"#;
        let result: Result<PositionsInSyncResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
