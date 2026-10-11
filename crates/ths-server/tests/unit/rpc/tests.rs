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
