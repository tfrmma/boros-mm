//! Spins up an in-process mock `ExecutionService` (the generated server
//! trait, not a real sidecar) and drives `ExecutionClient` against it. This
//! is the only way to actually prove `call_with_retry` retries when it
//! should and stops when it shouldn't, instead of trusting the logic by
//! inspection.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tonic::{transport::Server, Request, Response, Status};

use rust_bridge::{
    client::RetryConfig,
    proto::{
        self,
        execution_service_server::{ExecutionService, ExecutionServiceServer},
    },
    ExecutionClient,
};

/// A mock server whose behavior is scripted per-test via `fail_times` +
/// `fail_code`: the first `fail_times` calls to `place_order` return that
/// error code, then it succeeds.
struct ScriptedMock {
    calls: Arc<AtomicUsize>,
    fail_times: usize,
    fail_code: &'static str,
}

#[tonic::async_trait]
impl ExecutionService for ScriptedMock {
    async fn place_order(&self, _req: Request<proto::PlaceOrderRequest>) -> Result<Response<proto::PlaceOrderResponse>, Status> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            return Err(Status::unknown(format!("{}: scripted failure #{n}", self.fail_code)));
        }
        Ok(Response::new(proto::PlaceOrderResponse {
            tx_hash: "0xdeadbeef".to_string(),
            order_id: None,
            filled_size: "0".to_string(),
            placed_size: Some("1000000000000000000".to_string()),
            status: proto::TxStatus::Processed as i32,
        }))
    }

    async fn cancel_orders(&self, _req: Request<proto::CancelOrdersRequest>) -> Result<Response<proto::CancelOrdersResponse>, Status> {
        unimplemented!("not exercised by these tests")
    }

    async fn get_tx_status(&self, _req: Request<proto::GetTxStatusRequest>) -> Result<Response<proto::GetTxStatusResponse>, Status> {
        unimplemented!("not exercised by these tests")
    }
}

async fn spawn_mock(fail_times: usize, fail_code: &'static str) -> (Arc<AtomicUsize>, String) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = ScriptedMock { calls: calls.clone(), fail_times, fail_code };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(ExecutionServiceServer::new(mock))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // give the server a moment to start accepting
    tokio::time::sleep(Duration::from_millis(50)).await;
    (calls, format!("http://{addr}"))
}

fn fast_retry_config() -> RetryConfig {
    RetryConfig {
        max_attempts: 5,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
        backoff_multiplier: 2.0,
    }
}

#[tokio::test]
async fn retries_on_retriable_error_and_eventually_succeeds() {
    let (calls, endpoint) = spawn_mock(2, "BLOCKCHAIN_RPC_ERROR").await;
    let mut client = ExecutionClient::connect(endpoint, fast_retry_config()).await.unwrap();

    let result = client
        .place_order(
            "0xroot".into(), 1, oms_core::Side::Long, tick_math::FixedX18::from_f64(1.0),
            Some(100), None, oms_core::TimeInForce::Gtc,
        )
        .await;

    assert!(result.is_ok(), "expected eventual success after retriable failures, got {result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "expected 2 failures + 1 success = 3 calls");
}

#[tokio::test]
async fn stops_immediately_on_fatal_error_no_retry() {
    let (calls, endpoint) = spawn_mock(10, "INSUFFICIENT_MARGIN").await;
    let mut client = ExecutionClient::connect(endpoint, fast_retry_config()).await.unwrap();

    let result = client
        .place_order(
            "0xroot".into(), 1, oms_core::Side::Long, tick_math::FixedX18::from_f64(1.0),
            Some(100), None, oms_core::TimeInForce::Gtc,
        )
        .await;

    assert!(result.is_err(), "expected immediate failure on a Fatal-classified error");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a Fatal error must not be retried at all");
}

#[tokio::test]
async fn unknown_error_also_not_retried_by_default() {
    let (calls, endpoint) = spawn_mock(10, "SomeCodeWeHaveNeverSeenBefore").await;
    let mut client = ExecutionClient::connect(endpoint, fast_retry_config()).await.unwrap();

    let result = client
        .place_order(
            "0xroot".into(), 1, oms_core::Side::Long, tick_math::FixedX18::from_f64(1.0),
            Some(100), None, oms_core::TimeInForce::Gtc,
        )
        .await;

    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "Unknown-classified errors must default to no-retry, same as Fatal");
}

#[tokio::test]
async fn exhausts_retries_and_reports_failure_if_never_succeeds() {
    let (calls, endpoint) = spawn_mock(100, "EXTERNAL_SERVICE_ERROR").await;
    let mut config = fast_retry_config();
    config.max_attempts = 3;
    let mut client = ExecutionClient::connect(endpoint, config).await.unwrap();

    let result = client
        .place_order(
            "0xroot".into(), 1, oms_core::Side::Long, tick_math::FixedX18::from_f64(1.0),
            Some(100), None, oms_core::TimeInForce::Gtc,
        )
        .await;

    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 3, "must stop at max_attempts even though every attempt is retriable");
}
