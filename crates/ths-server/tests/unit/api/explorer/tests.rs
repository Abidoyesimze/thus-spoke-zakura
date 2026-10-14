use super::*;
use crate::api::router;
use crate::state::AppState;
use crate::state::tests::state_with_local_wallet;
use axum::body::Body;
use axum::routing::post;
use axum::{
    Json, Router,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
#[test]
fn copies_spent_output_address_and_value_onto_vin() {
    let mut tx = json!({
        "vin": [{"txid": "aa", "vout": 1}],
        "vout": []
    });
    let mut prev = std::collections::HashMap::new();
    prev.insert(
        "aa".into(),
        json!({
            "vout": [
                {"n": 0, "valueZat": 1},
                {
                    "n": 1,
                    "valueZat": 50_000_000,
                    "scriptPubKey": {"addresses": ["tmABC"]}
                }
            ]
        }),
    );
    attach_transparent_prevouts(&mut tx, &prev);
    assert_eq!(tx["vin"][0]["valueZat"], 50_000_000);
    assert_eq!(tx["vin"][0]["scriptPubKey"]["addresses"][0], "tmABC");
}

#[test]
fn leaves_coinbase_inputs_untouched() {
    let mut tx = json!({"vin": [{"coinbase": "00"}]});
    attach_transparent_prevouts(&mut tx, &Default::default());
    assert_eq!(tx["vin"][0]["coinbase"], "00");
    assert!(tx["vin"][0].get("valueZat").is_none());
}

#[test]
fn copies_the_ironwood_root_from_the_treestate() {
    let mut block = json!({"hash": "ab", "finalorchardroot": "orchard-root"});
    add_ironwood_root(
        &mut block,
        &json!({"ironwood": {"commitments": {"finalRoot": "ironwood-root", "finalState": "00"}}}),
    );
    assert_eq!(block["finalironwoodroot"], "ironwood-root");
    assert_eq!(block["finalorchardroot"], "orchard-root");

    // At the genesis block Zakura returns empty Ironwood commitments.
    let mut block = json!({"hash": "ab"});
    add_ironwood_root(&mut block, &json!({"ironwood": {"commitments": {}}}));
    assert!(block.get("finalironwoodroot").is_none());
}

#[tokio::test]
async fn explorer_rejects_malformed_addresses_before_reaching_zakura() {
    let (state, _dir) = state_with_local_wallet();
    let account = state.0.store.account(1).unwrap();
    assert!(require_transparent_address(&account.transparent_address).is_ok());

    let response = router(state)
        .oneshot(
            Request::get("/api/v1/addresses/tmOOOOOOOOOOOOOOOOOOOOOOOO")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_reports_unknown_blocks_and_transactions_as_404() {
    // A node that knows nothing, answering with Zakura's not-found codes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", listener.local_addr().unwrap());
    let not_found = Router::new().route(
        "/",
        post(|Json(req): Json<Value>| async move {
            let code = if req["method"] == "getblock" { -8 } else { -5 };
            Json(json!({"error": {"code": code, "message": "not found"}}))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, not_found).await });

    let (state, _dir) = state_with_local_wallet();
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        node,
        "test".into(),
    );
    let hash = "a".repeat(64);
    for path in [
        "/api/v1/blocks/999999".to_owned(),
        format!("/api/v1/transactions/{hash}"),
        "/api/v1/search?q=999999".to_owned(),
        format!("/api/v1/search?q={hash}"),
    ] {
        let response = router(state.clone())
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn search_does_not_mask_a_block_node_failure_as_404() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", listener.local_addr().unwrap());
    let failing = Router::new().route(
        "/",
        post(|Json(req): Json<Value>| async move {
            let code = if req["method"] == "getblock" { -28 } else { -5 };
            Json(json!({"error": {"code": code, "message": "injected"}}))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, failing).await });

    let (state, _dir) = state_with_local_wallet();
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        node,
        "t".into(),
    );
    let uri = format!("/api/v1/search?q={}", "a".repeat(64));
    let response = router(state)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
