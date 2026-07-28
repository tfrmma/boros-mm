//! `ExecutionClient`, the thing `mm-bot`/`arb-bot` actually holds. Wraps
//! the tonic-generated `ExecutionServiceClient` with retry/backoff, but
//! only for requests classified `Retriable` (see `error_class`). A `Fatal`
//! or `Unknown` rejection returns immediately: retrying a margin
//! rejection or an order-already-filled error with the identical payload
//! doesn't become safer by waiting.

use std::time::Duration;

use tonic::transport::Channel;

use crate::{
    error::BridgeError,
    error_class::{classify, ErrorClass},
    proto,
    types::{fixed_from_string, fixed_to_string, order_id_from_string, order_id_to_string, side_to_proto, tif_to_proto},
};

/// Nothing here is a magic number picked for this project specifically:
/// every field is a policy the caller sets, matching this workspace's own
/// "nothing hardcoded that doesn't have to be" principle. There is no
/// built-in default beyond what `RetryConfig::conservative()` documents as
/// a starting point, not a recommendation.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
}

impl RetryConfig {
    /// A starting point for local development, not a production
    /// recommendation: a real deployment's retry policy depends on the
    /// caller's own latency budget (a market maker's order placement has a
    /// very different acceptable retry window than an overnight
    /// reconciliation job).
    pub fn conservative() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            backoff_multiplier: 2.0,
        }
    }
}

pub struct ExecutionClient {
    inner: proto::execution_service_client::ExecutionServiceClient<Channel>,
    retry: RetryConfig,
}

/// Result of a single placed order, translated back into workspace domain
/// types from the wire representation.
#[derive(Debug, Clone)]
pub struct PlaceOrderOutcome {
    pub tx_hash: String,
    pub order_id: Option<oms_core::OrderId>,
    pub filled_size: tick_math::FixedX18,
    pub placed_size: Option<tick_math::FixedX18>,
    pub status: proto::TxStatus,
}

impl ExecutionClient {
    pub async fn connect(endpoint: String, retry: RetryConfig) -> Result<Self, BridgeError> {
        let channel = Channel::from_shared(endpoint)
            .map_err(|e| BridgeError::InvalidResponse(format!("bad endpoint: {e}")))?
            .connect()
            .await?;
        Ok(Self { inner: proto::execution_service_client::ExecutionServiceClient::new(channel), retry })
    }

    /// For tests: wrap an already-connected channel (e.g. an in-process
    /// mock server) instead of dialing a real endpoint.
    pub fn from_channel(channel: Channel, retry: RetryConfig) -> Self {
        Self { inner: proto::execution_service_client::ExecutionServiceClient::new(channel), retry }
    }

    pub async fn place_order(
        &mut self,
        market_acc: String,
        market_id: u32,
        side: oms_core::Side,
        size: tick_math::FixedX18,
        limit_tick: Option<i32>,
        slippage: Option<f64>,
        tif: oms_core::TimeInForce,
    ) -> Result<PlaceOrderOutcome, BridgeError> {
        let req = proto::PlaceOrderRequest {
            market_acc,
            market_id,
            side: side_to_proto(side).into(),
            size: fixed_to_string(size),
            limit_tick,
            slippage,
            tif: tif_to_proto(tif).into(),
        };

        let resp = self.call_with_retry("PlaceOrder", || {
            let mut client = self.inner.clone();
            let req = req.clone();
            async move { client.place_order(req).await }
        }).await?;

        Ok(PlaceOrderOutcome {
            tx_hash: resp.tx_hash,
            order_id: resp.order_id.as_deref().map(order_id_from_string).transpose()?,
            filled_size: fixed_from_string(&resp.filled_size)?,
            placed_size: resp.placed_size.as_deref().map(fixed_from_string).transpose()?,
            status: proto::TxStatus::try_from(resp.status).unwrap_or(proto::TxStatus::Unspecified),
        })
    }

    pub async fn cancel_orders(
        &mut self,
        market_acc: String,
        market_id: u32,
        cancel_all: bool,
        order_ids: Vec<oms_core::OrderId>,
    ) -> Result<(String, proto::TxStatus), BridgeError> {
        let req = proto::CancelOrdersRequest {
            market_acc,
            market_id,
            cancel_all,
            order_ids: order_ids.into_iter().map(order_id_to_string).collect(),
        };

        let resp = self.call_with_retry("CancelOrders", || {
            let mut client = self.inner.clone();
            let req = req.clone();
            async move { client.cancel_orders(req).await }
        }).await?;

        Ok((resp.tx_hash, proto::TxStatus::try_from(resp.status).unwrap_or(proto::TxStatus::Unspecified)))
    }

    pub async fn get_tx_status(&mut self, agent: String, nonce: String) -> Result<proto::GetTxStatusResponse, BridgeError> {
        let req = proto::GetTxStatusRequest { agent, nonce };
        self.call_with_retry("GetTxStatus", || {
            let mut client = self.inner.clone();
            let req = req.clone();
            async move { client.get_tx_status(req).await }
        }).await
    }

    /// Shared retry loop: call `f`, and on a rejection classified
    /// `Retriable`, back off and try again up to `retry.max_attempts`. Any
    /// other outcome (`Fatal`, `Unknown`, transport error, or success)
    /// returns immediately.
    async fn call_with_retry<T, F, Fut>(&self, rpc_name: &str, mut f: F) -> Result<T, BridgeError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let mut backoff = self.retry.initial_backoff;
        let mut last_err: Option<BridgeError> = None;

        for attempt in 1..=self.retry.max_attempts {
            match f().await {
                Ok(resp) => return Ok(resp.into_inner()),
                Err(status) => {
                    let (code, message) = extract_code(&status);
                    let class = classify(&code);
                    tracing::warn!(rpc = rpc_name, attempt, code = %code, ?class, "execution call rejected");

                    if class != ErrorClass::Retriable || attempt == self.retry.max_attempts {
                        return Err(BridgeError::Rejected { code, message, class });
                    }

                    last_err = Some(BridgeError::Rejected { code, message, class });
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(
                        Duration::from_secs_f64(backoff.as_secs_f64() * self.retry.backoff_multiplier),
                        self.retry.max_backoff,
                    );
                }
            }
        }

        // unreachable in practice (the loop always returns on the last
        // attempt above), but keeps the function total instead of relying
        // on that invariant silently
        Err(last_err.unwrap_or(BridgeError::RetriesExhausted { attempts: self.retry.max_attempts, last: "no attempts made".into() }))
    }
}

/// sidecar-ts encodes the Boros error code as a `"<code>: <message>"`
/// prefix on the gRPC `Status` message (see `execution.proto`'s module
/// doc and `sidecar-ts/src/errorMapping.ts`), a simplification instead of
/// the full `google.rpc.ErrorInfo` status-details pattern, to keep the
/// wire contract small. Revisit if/when this needs to carry structured
/// metadata beyond a code + message.
fn extract_code(status: &tonic::Status) -> (String, String) {
    let msg = status.message();
    match msg.split_once(": ") {
        Some((code, rest)) => (code.to_string(), rest.to_string()),
        None => ("UNKNOWN".to_string(), msg.to_string()),
    }
}
