use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const GENERATE_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Clone)]
pub struct NodeRpc {
    endpoint: String,
    client: Client,
    request_id: std::sync::Arc<AtomicU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    pub chain: String,
    pub blocks: u64,
    #[serde(default)]
    pub bestblockhash: String,
    #[serde(default)]
    pub verificationprogress: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainCheckpoint {
    pub height: u64,
    pub hash: String,
}

#[derive(Debug, Deserialize, thiserror::Error)]
#[error("RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    message: String,
}

impl NodeRpc {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: Client::new(),
            request_id: Default::default(),
        }
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        self.call_within(CALL_TIMEOUT, method, params).await
    }

    async fn call_within<T: DeserializeOwned>(
        &self,
        timeout: Duration,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .client
            .post(&self.endpoint)
            .timeout(timeout)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|error| {
                let context = if error.is_timeout() {
                    format!("Zakura {method} timed out after {timeout:?}")
                } else {
                    format!("calling Zakura {method}")
                };
                anyhow::Error::new(error).context(context)
            })?;
        let status = response.status();
        let envelope: Value = response.json().await.context("decoding Zakura response")?;
        classify_response(status, method, envelope)
    }

    pub async fn chain_info(&self) -> Result<ChainInfo> {
        self.call("getblockchaininfo", json!([])).await
    }
    pub async fn checkpoint(&self) -> Result<ChainCheckpoint> {
        let info = self.chain_info().await?;
        anyhow::ensure!(
            info.bestblockhash.len() == 64,
            "Zakura returned an invalid best block hash"
        );
        Ok(ChainCheckpoint {
            height: info.blocks,
            hash: info.bestblockhash,
        })
    }
    pub async fn generate(&self, blocks: u32) -> Result<Vec<String>> {
        self.call_within(GENERATE_TIMEOUT, "generate", json!([blocks]))
            .await
    }
    pub async fn block(&self, id: &str) -> Result<Value> {
        self.call("getblock", json!([id, 2])).await
    }
    pub async fn treestate(&self, block_hash: &str) -> Result<Value> {
        self.call("z_gettreestate", json!([block_hash])).await
    }
    pub async fn block_hash(&self, height: u32) -> Result<String> {
        self.call("getblockhash", json!([height])).await
    }
    pub async fn unspent_output(&self, txid: &str, index: u32) -> Result<Option<Value>> {
        self.call("gettxout", json!([txid, index, true])).await
    }
    pub async fn transaction(&self, txid: &str) -> Result<Value> {
        self.call("getrawtransaction", json!([txid, 1])).await
    }
    pub async fn lookup_transaction(&self, txid: &str) -> Result<Option<Value>> {
        match self.transaction(txid).await {
            Ok(transaction) => {
                anyhow::ensure!(
                    transaction.get("txid").and_then(Value::as_str) == Some(txid),
                    "Zakura returned a mismatched transaction id"
                );
                Ok(Some(transaction))
            }
            Err(error) if transaction_missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub async fn transaction_known(&self, txid: &str) -> Result<bool> {
        Ok(self.lookup_transaction(txid).await?.is_some())
    }
    pub async fn mempool(&self) -> Result<Vec<String>> {
        self.call("getrawmempool", json!([])).await
    }
}

fn transaction_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|error| error.code == -5)
}

fn classify_response<T: DeserializeOwned>(
    status: reqwest::StatusCode,
    method: &str,
    envelope: Value,
) -> Result<T> {
    if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
        let error: RpcError =
            serde_json::from_value(error.clone()).context("decoding Zakura RPC error")?;
        return Err(error).with_context(|| format!("Zakura {method} failed"));
    }
    if !status.is_success() {
        bail!("Zakura {method} returned HTTP {status} without a JSON-RPC error");
    }
    let result = envelope
        .get("result")
        .with_context(|| format!("Zakura {method} response did not contain a result"))?;
    serde_json::from_value(result.clone()).context("decoding Zakura result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn call_gives_up_on_a_node_that_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let err = rpc
            .call_within::<Value>(Duration::from_millis(50), "getblockcount", json!([]))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Zakura getblockcount timed out after 50ms");
    }

    #[test]
    fn only_rpc_not_found_means_a_transaction_is_missing() {
        let missing = anyhow::Error::new(RpcError {
            code: -5,
            message: "not found".into(),
        })
        .context("getrawtransaction failed");
        let unavailable = anyhow::Error::new(RpcError {
            code: -28,
            message: "warming up".into(),
        });

        assert!(transaction_missing(&missing));
        assert!(!transaction_missing(&unavailable));
    }
}
