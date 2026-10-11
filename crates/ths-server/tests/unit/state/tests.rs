use crate::api::router;
use crate::{db::Store, state::AppState, wallet::RealWallet};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
pub(crate) fn state_with_local_wallet() -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(dir.path().join("server.db")).expect("open store");
    store.initialize().expect("initialize store");
    let wallet =
        RealWallet::open(dir.path(), &store.seed().expect("wallet seed")).expect("open wallet");
    (
        AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into()),
        dir,
    )
}

#[tokio::test]
async fn account_reads_return_the_snapshot_without_contacting_lightwalletd() {
    let (state, _dir) = state_with_local_wallet();
    {
        let mut snapshot = state.0.wallet_snapshot.write().await;
        snapshot.accounts[0].ironwood_zatoshi = 400_000_000;
    }
    let response = router(state)
        .oneshot(
            Request::get("/api/v1/accounts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let accounts = value.as_array().expect("account array");
    assert_eq!(
        accounts
            .iter()
            .map(|a| a["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5],
    );
    assert_eq!(accounts[0]["ironwood_zatoshi"], 400_000_000);
    let expected_keys = std::collections::BTreeSet::from([
        "id",
        "name",
        "unified_address",
        "transparent_address",
        "transparent_zatoshi",
        "ironwood_zatoshi",
        "unified_full_viewing_key",
    ]);
    for account in accounts {
        let keys = account
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys, expected_keys);
        assert!(
            account["unified_full_viewing_key"]
                .as_str()
                .is_some_and(|key| !key.is_empty())
        );
    }
}

pub(crate) fn offline_state() -> (AppState, tempfile::TempDir) {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
    (state, dir)
}
