mod payment_proxy;
mod regtest;
mod rpc_proxy;

use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

pub use payment_proxy::{QueuedBroadcastProxy, lose_payment_response};
pub use regtest::{
    FailureRoute, HeightCheckpoint, RecoveryFailureReporter, RecoveryPhase, RegtestStack,
    TerminationSignals,
};
pub use rpc_proxy::{GenerateCounts, GenerateFaultProxy};

const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;
const DIRECT_NODE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
enum HttpFailure {
    #[error("HTTP request timed out")]
    Timeout,
    #[error("HTTP transport failure")]
    Transport,
    #[error("HTTP response returned status {0}")]
    Status(u16),
    #[error("HTTP response exceeded the 32 MiB limit")]
    ResponseTooLarge,
    #[error("HTTP response was not valid JSON")]
    Schema,
    #[error("JSON-RPC response was not an object")]
    RpcNotObject,
    #[error("JSON-RPC response ID did not match the request")]
    RpcIdMismatch,
    #[error("JSON-RPC response contained an error")]
    RpcError,
    #[error("JSON-RPC response did not contain a result")]
    RpcMissingResult,
}

/// Sends one API request without retries and decodes a bounded JSON response.
///
/// A missing body is a GET. A present JSON body is a POST. Error messages name
/// only the transport/status/JSON category and never include response bodies.
pub async fn request_json<T: DeserializeOwned>(
    client: &Client,
    base: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<T> {
    let url = join_url(base, path);
    let request = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    tokio::time::timeout(timeout, async move {
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::Error::from(HttpFailure::Transport))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(HttpFailure::Status(status.as_u16())));
        }
        let body = read_response_body(response).await?;
        serde_json::from_slice(&body).map_err(|_| HttpFailure::Schema.into())
    })
    .await
    .map_err(|_| HttpFailure::Timeout)?
}

/// Sends one strict JSON-RPC request to a direct node endpoint without retries.
pub async fn rpc<T: DeserializeOwned>(
    client: &Client,
    endpoint: &str,
    method: &str,
    params: Value,
) -> Result<T> {
    rpc_with_timeout(client, endpoint, method, params, DIRECT_NODE_TIMEOUT).await
}

pub(crate) async fn rpc_with_timeout<T: DeserializeOwned>(
    client: &Client,
    endpoint: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<T> {
    let id = Uuid::new_v4().to_string();
    let request = json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "method": method,
        "params": params,
    });
    let response: Value = request_json(client, endpoint, "", Some(&request), timeout).await?;
    let object = response.as_object().ok_or(HttpFailure::RpcNotObject)?;
    if object.get("id") != Some(&Value::String(id)) {
        return Err(HttpFailure::RpcIdMismatch.into());
    }
    if object.get("error").is_some_and(|error| !error.is_null()) {
        return Err(HttpFailure::RpcError.into());
    }
    let result = object
        .get("result")
        .cloned()
        .ok_or(HttpFailure::RpcMissingResult)?;
    serde_json::from_value(result).map_err(|_| HttpFailure::Schema.into())
}

pub(crate) async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JSON_BODY_BYTES as u64)
    {
        return Err(HttpFailure::ResponseTooLarge.into());
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| HttpFailure::Transport)? {
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or(HttpFailure::ResponseTooLarge)?;
        if next_len > MAX_JSON_BODY_BYTES {
            return Err(HttpFailure::ResponseTooLarge.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Classifies only a failed read attempt for a caller-owned deadline loop.
///
/// This function does not retry anything itself. Call it only after the loop
/// has rechecked owned-service liveness, and never use it for Send or another
/// mutation whose outcome could be unknown after a transport deadline.
pub(crate) fn is_retryable_read_transport(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<HttpFailure>())
        .any(|failure| matches!(failure, HttpFailure::Transport | HttpFailure::Timeout))
}

/// Classifies only read-only startup probes that may remain pending.
pub(crate) fn is_startup_pending(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<HttpFailure>())
        .any(|failure| {
            matches!(
                failure,
                HttpFailure::Transport | HttpFailure::Timeout | HttpFailure::Status(502 | 503)
            )
        })
}

fn join_url(base: &str, path: &str) -> String {
    if path.is_empty() {
        base.trim_end_matches('/').to_owned()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

pub(crate) fn safe_exit_code(code: Option<i32>) -> String {
    code.map_or_else(|| "unavailable".to_owned(), |code| code.to_string())
}

pub(crate) fn checked_u16(value: &str) -> Result<u16> {
    let port: u16 = value.parse().map_err(|_| anyhow::anyhow!("invalid port"))?;
    if port == 0 {
        bail!("invalid port");
    }
    Ok(port)
}

#[cfg(test)]
mod tests;
