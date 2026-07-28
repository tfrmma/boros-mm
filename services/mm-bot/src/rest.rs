//! Same verified REST endpoints as `services/risk-monitor/src/rest.rs`
//! (see that file's module doc for how each was confirmed against
//! `@pendle/sdk-boros@1.5.0`'s compiled JS). Duplicated instead of shared
//! from a common crate: these two services are independent
//! processes with no shared failure domain (see mm-bot's and
//! risk-monitor's module docs), a shared internal library dependency
//! would be a minor form of that same coupling. If this duplication grows
//! past these two thin clients, revisit as a real shared crate then, not
//! preemptively.

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

    pub async fn get_market(&self, market_id: u32) -> Result<MarketResponse, RestError> {
        let url = format!("{}/v1/markets/{market_id}", self.base_url);
        Ok(self.http.get(url).send().await?.error_for_status()?.json().await?)
    }

    pub async fn get_positions(&self, root: &str, account_id: u32) -> Result<PositionsInSyncResponse, RestError> {
        let url = format!("{}/v1/pnl/positions", self.base_url);
        Ok(self.http.get(url).query(&[("root", root), ("accountId", &account_id.to_string())]).send().await?.error_for_status()?.json().await?)
    }

    pub async fn get_collateral_summary(&self, user_address: &str, account_id: u32, token_id: u32) -> Result<SingleCollateralSummaryResponse, RestError> {
        let url = format!("{}/v1/collaterals/summary/single", self.base_url);
        Ok(self.http.get(url)
            .query(&[("userAddress", user_address.to_owned()), ("accountId", account_id.to_string()), ("tokenId", token_id.to_string())])
            .send().await?.error_for_status()?.json().await?)
    }
}

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
    pub data: Option<MarketDataFieldsResponse>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionInSyncResponse {
    pub market_id: u32,
    pub notional_size: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PositionsInSyncResponse {
    pub results: Vec<PositionInSyncResponse>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketAccCollateralResponse {
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
