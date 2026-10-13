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

#[tokio::test]
async fn explicit_availability_statuses_are_retryable_without_a_json_rpc_body() {
    for (status, body, retryable) in [
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "temporarily unavailable",
            true,
        ),
        (axum::http::StatusCode::BAD_GATEWAY, "{", true),
        (axum::http::StatusCode::GATEWAY_TIMEOUT, "{}", true),
        (axum::http::StatusCode::TOO_MANY_REQUESTS, "busy", true),
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "{", false),
        (axum::http::StatusCode::OK, "{", false),
        (
            axum::http::StatusCode::OK,
            "{\"result\":{},\"error\":null}",
            false,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || async move { (status, body) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = rpc.chain_info().await.unwrap_err();
        assert_eq!(
            crate::bootstrap::startup_retryable(&error),
            retryable,
            "{status}: {error:#}"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
