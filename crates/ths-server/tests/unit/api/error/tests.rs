use super::*;
use crate::api::{
    error::{ApiError, not_found},
    explorer::NO_BLOCK,
};
use crate::{rpc::RpcError, wallet::PaymentError};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
#[derive(Clone, Default)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs<T>(operation: impl FnOnce() -> T) -> (T, String) {
    let output = LogWriter::default();
    let writer = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    // Keep capture scoped to the synchronous operation so parallel tests and
    // tasks running on other threads cannot write into this test's log.
    let result = tracing::subscriber::with_default(subscriber, operation);
    let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    (result, logs)
}

#[tokio::test]
async fn api_error_logs_full_rpc_error_chain() {
    let error: RpcError =
        serde_json::from_value(json!({"code": -28, "message": "warming up"})).unwrap();
    let error = anyhow::Error::new(error)
        .context("Zakura getblock failed")
        .context("loading block details");
    let (response, logs) = capture_logs(|| ApiError::from(error).into_response());

    assert_eq!(logs.lines().count(), 1, "{logs}");
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(
        logs.contains(
            "error=loading block details: Zakura getblock failed: RPC error -28: warming up"
        ),
        "{logs}"
    );
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error": {"message": "loading block details", "status": 500}})
    );
}

#[tokio::test]
async fn api_error_logs_skip_expected_failures() {
    for (error, status) in [
        (
            anyhow::Error::new(IdempotencyConflict),
            StatusCode::CONFLICT,
        ),
        (
            anyhow::Error::new(PaymentError::InsufficientFunds {
                available: 100_000_000,
                required: 100_010_000,
            }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            anyhow::Error::new(PaymentError::TransparentMemo),
            StatusCode::BAD_REQUEST,
        ),
        (
            anyhow::Error::new(PaymentError::TreasuryExhausted),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            anyhow::Error::new(PaymentError::ScanRequired),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let error = error.context("payment could not complete");
        let (response, logs) = capture_logs(|| ApiError::from(error).into_response());
        assert!(logs.is_empty(), "{status}: {logs}");
        assert_eq!(response.status(), status);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": {"message": "payment could not complete", "status": status.as_u16()}})
        );
    }
}

#[tokio::test]
async fn api_error_logs_skip_explorer_misses() {
    for code in [-5, -8] {
        let error: RpcError =
            serde_json::from_value(json!({"code": code, "message": "not found"})).unwrap();
        let error = anyhow::Error::new(error).context("Zakura getblock failed");
        let (response, logs) = capture_logs(|| not_found(error, NO_BLOCK).into_response());
        assert!(logs.is_empty(), "{code}: {logs}");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": {"message": NO_BLOCK, "status": 404}})
        );
    }
}

#[test]
fn a_dry_treasury_is_reported_as_unavailable() {
    assert_eq!(
        ApiError::from(anyhow::Error::new(PaymentError::TreasuryExhausted)).status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn an_unaffordable_send_is_reported_as_a_client_error() {
    assert_eq!(
        ApiError::from(anyhow::Error::new(PaymentError::InsufficientFunds {
            available: 100_000_000,
            required: 100_010_000,
        }))
        .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
