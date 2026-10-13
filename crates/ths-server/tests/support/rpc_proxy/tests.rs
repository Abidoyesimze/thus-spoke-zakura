use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use super::{GenerateCounts, GenerateFaultProxy};

struct LocalServer {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl LocalServer {
    async fn start(router: Router) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = receiver.await;
                })
                .await;
        });
        Ok(Self {
            url: format!("http://{address}"),
            shutdown: Some(shutdown),
            task,
        })
    }

    async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task).await??;
        Ok(())
    }
}

async fn record_request(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push(request.clone());
    Json(json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": {"forwarded": true},
    }))
}

async fn upstream_http_error(Json(request): Json<Value>) -> Response {
    (
        StatusCode::IM_A_TEAPOT,
        [("x-fixture-error", "preserved")],
        Json(json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": {"code": -32011, "message": "upstream HTTP failure"},
        })),
    )
        .into_response()
}

async fn upstream_json_rpc_error(Json(request): Json<Value>) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": Value::Null,
        "error": {"code": -32012, "message": "upstream JSON-RPC failure"},
    }))
}

fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

async fn post_json(client: &Client, url: &str, request: &Value) -> Result<(StatusCode, Value)> {
    let response = client.post(url).json(request).send().await?;
    let status = response.status();
    Ok((status, response.json().await?))
}

#[tokio::test]
async fn proxy_forwards_unarmed_requests_and_consumes_only_matching_generate_once() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let upstream = LocalServer::start(
        Router::new()
            .fallback(record_request)
            .with_state(Arc::clone(&requests)),
    )
    .await?;
    let mut proxy = GenerateFaultProxy::start(upstream.url.clone()).await?;
    let client = Client::new();

    let unarmed = request("read-one", "getblockchaininfo", json!([]));
    let (status, response) = post_json(&client, proxy.url(), &unarmed).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["id"], "read-one");
    assert_eq!(response["result"], json!({"forwarded": true}));

    proxy.fail_next_generate()?;
    assert!(proxy.fail_next_generate().is_err());

    let read_after_arming = request("read-two", "getrawmempool", json!([]));
    let (status, _) = post_json(&client, proxy.url(), &read_after_arming).await?;
    assert_eq!(status, StatusCode::OK);
    let other_generate = request("generate-two", "generate", json!([2]));
    let (status, _) = post_json(&client, proxy.url(), &other_generate).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        proxy.counts(),
        GenerateCounts {
            rejected: 0,
            forwarded: 1,
        }
    );

    let matching_generate = request("generate-one", "generate", json!([1]));
    let (status, response) = post_json(&client, proxy.url(), &matching_generate).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["id"], "generate-one");
    assert_eq!(response["result"], Value::Null);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        proxy.counts(),
        GenerateCounts {
            rejected: 1,
            forwarded: 1,
        }
    );

    let (status, response) = post_json(&client, proxy.url(), &matching_generate).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["result"], json!({"forwarded": true}));
    assert_eq!(
        proxy.counts(),
        GenerateCounts {
            rejected: 1,
            forwarded: 2,
        }
    );
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        &[
            unarmed,
            read_after_arming,
            other_generate,
            matching_generate
        ]
    );

    proxy.shutdown().await?;
    upstream.shutdown().await
}

#[tokio::test]
async fn separate_proxy_instances_do_not_consume_each_others_faults() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let upstream = LocalServer::start(
        Router::new()
            .fallback(record_request)
            .with_state(Arc::clone(&requests)),
    )
    .await?;
    let mut first = GenerateFaultProxy::start(upstream.url.clone()).await?;
    let mut second = GenerateFaultProxy::start(upstream.url.clone()).await?;
    let client = Client::new();
    let matching_generate = request("isolation", "generate", json!([1]));

    first.fail_next_generate()?;
    let (status, _) = post_json(&client, second.url(), &matching_generate).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first.counts(),
        GenerateCounts {
            rejected: 0,
            forwarded: 0,
        }
    );
    assert_eq!(
        second.counts(),
        GenerateCounts {
            rejected: 0,
            forwarded: 1,
        }
    );

    let (status, response) = post_json(&client, first.url(), &matching_generate).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        first.counts(),
        GenerateCounts {
            rejected: 1,
            forwarded: 0,
        }
    );

    first.shutdown().await?;
    second.shutdown().await?;
    upstream.shutdown().await
}

#[tokio::test]
async fn proxy_preserves_upstream_errors_and_bounds_transport_shutdown() -> Result<()> {
    let client = Client::new();
    let request = request("errors", "getblockchaininfo", json!([]));

    let http_error = LocalServer::start(Router::new().fallback(upstream_http_error)).await?;
    let mut proxy = GenerateFaultProxy::start(http_error.url.clone()).await?;
    let response = client.post(proxy.url()).json(&request).send().await?;
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(response.headers()["x-fixture-error"], "preserved");
    let response: Value = response.json().await?;
    assert_eq!(response["id"], "errors");
    assert_eq!(response["error"]["code"], -32011);
    tokio::time::timeout(Duration::from_secs(2), proxy.shutdown()).await??;
    http_error.shutdown().await?;

    let json_error = LocalServer::start(Router::new().fallback(upstream_json_rpc_error)).await?;
    let mut proxy = GenerateFaultProxy::start(json_error.url.clone()).await?;
    let (status, response) = post_json(&client, proxy.url(), &request).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["id"], "errors");
    assert_eq!(response["error"]["code"], -32012);
    proxy.shutdown().await?;
    json_error.shutdown().await?;

    let mut unavailable = GenerateFaultProxy::start("http://127.0.0.1:9".to_owned()).await?;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        client.post(unavailable.url()).json(&request).send(),
    )
    .await??;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    tokio::time::timeout(Duration::from_secs(2), unavailable.shutdown()).await??;
    Ok(())
}

#[tokio::test]
async fn startup_mining_fault_waits_for_broadcast_bytes_and_is_consumed_once() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let upstream = LocalServer::start(
        Router::new()
            .fallback(record_request)
            .with_state(requests.clone()),
    )
    .await?;
    let mut proxy = GenerateFaultProxy::start(upstream.url.clone()).await?;
    let dir = tempfile::tempdir()?;
    let db = rusqlite::Connection::open(dir.path().join("ths.db"))?;
    db.execute_batch(
        "CREATE TABLE activity(id TEXT,txid TEXT,status TEXT);
        CREATE TABLE idempotency(key TEXT,activity_id TEXT);
        CREATE TABLE prepared_payments(activity_id TEXT,raw_transaction BLOB,expiry_height INTEGER);
        INSERT INTO activity VALUES('original','original-txid','preparing');
        INSERT INTO idempotency VALUES('startup-account-1-ironwood-v1','original');
        INSERT INTO prepared_payments VALUES('original',x'010203',140);",
    )?;
    proxy.fail_startup_auto_mine(dir.path())?;
    let client = Client::new();
    let generate = request("startup-mine", "generate", json!([1]));
    let (_, before) = post_json(&client, proxy.url(), &generate).await?;
    assert_eq!(before["result"], json!({"forwarded": true}));
    assert!(proxy.startup_funding_snapshot().is_err());
    db.execute("UPDATE activity SET status='broadcast'", [])?;
    let (_, other) = post_json(
        &client,
        proxy.url(),
        &request("other-mine", "generate", json!([2])),
    )
    .await?;
    assert_eq!(other["result"], json!({"forwarded": true}));
    let (_, fault) = post_json(&client, proxy.url(), &generate).await?;
    assert_eq!(fault["error"]["code"], -28);
    let snapshot = proxy.startup_funding_snapshot()?;
    assert_eq!(snapshot.id, "original");
    assert_eq!(snapshot.txid, "original-txid");
    assert_eq!(snapshot.raw, vec![1, 2, 3]);
    assert_eq!(snapshot.expiry_height, 140);
    let (_, after) = post_json(&client, proxy.url(), &generate).await?;
    assert_eq!(after["result"], json!({"forwarded": true}));
    assert_eq!(
        proxy.counts(),
        GenerateCounts {
            rejected: 1,
            forwarded: 3
        }
    );
    assert_eq!(requests.lock().unwrap().len(), 3);
    proxy.shutdown().await?;
    upstream.shutdown().await
}

#[tokio::test]
async fn startup_outage_affects_readiness_only_and_can_be_released() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let upstream = LocalServer::start(
        Router::new()
            .fallback(record_request)
            .with_state(requests.clone()),
    )
    .await?;
    let mut proxy = GenerateFaultProxy::start(upstream.url.clone()).await?;
    let client = Client::new();
    proxy.set_startup_unavailable(true)?;
    let read = request("readiness", "getblockchaininfo", json!([]));
    let (_, blocked) = post_json(&client, proxy.url(), &read).await?;
    assert_eq!(blocked["error"]["code"], -28);
    for method in ["getrawtransaction", "generate"] {
        let (_, forwarded) = post_json(
            &client,
            proxy.url(),
            &request("unrelated", method, json!([1])),
        )
        .await?;
        assert_eq!(forwarded["result"], json!({"forwarded": true}));
    }
    proxy.set_startup_unavailable(false)?;
    let (_, released) = post_json(&client, proxy.url(), &read).await?;
    assert_eq!(released["result"], json!({"forwarded": true}));
    assert_eq!(requests.lock().unwrap().len(), 3);
    proxy.shutdown().await?;
    upstream.shutdown().await
}
