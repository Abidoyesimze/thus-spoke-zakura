use crate::api::router;
use crate::state::tests::state_with_local_wallet;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
#[tokio::test]
async fn status_counts_accounts_without_reading_the_seed() {
    let (state, dir) = state_with_local_wallet();
    rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .execute("UPDATE metadata SET value='not-hex' WHERE key='seed'", [])
        .unwrap();

    let response = router(state)
        .oneshot(
            Request::get("/api/v1/status")
                .header("host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["account_count"], 5);
}

#[tokio::test]
async fn non_loopback_host_cannot_access_the_seed_endpoint() {
    let (state, _dir) = state_with_local_wallet();
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/dev/seed")
                .header("host", "attacker.example")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation":"I understand this seed is for regtest only"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        error,
        serde_json::json!({
            "error": {
                "message": "Host must be a loopback address",
                "status": 403
            }
        })
    );
}

use super::*;
use crate::wallet::WALLET_BIRTHDAY_HEIGHT;

#[test]
fn public_dashboard_endpoint_uses_configured_port_or_loopback_default() {
    assert_eq!(
        public_dashboard_endpoint(Some("http://127.0.0.1:32805")),
        "http://127.0.0.1:32805"
    );
    assert_eq!(public_dashboard_endpoint(None), "http://127.0.0.1:8080");
}

fn wallet_status(
    state: &'static str,
    fully_scanned_height: Option<u64>,
    error: Option<&str>,
) -> WalletSyncStatus {
    WalletSyncStatus {
        state,
        fully_scanned_height,
        fully_scanned_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_height: fully_scanned_height,
        last_success_at: Some(123),
        error: error.map(str::to_owned),
    }
}

fn settlement_for_test(
    node: Option<u64>,
    lightwalletd: Option<u64>,
    wallet: &WalletSyncStatus,
) -> SettlementStatus {
    let checkpoint = |height| ChainCheckpoint {
        height,
        hash: "canonical".to_owned(),
    };
    settlement_status(
        node.map(checkpoint).as_ref(),
        lightwalletd.map(checkpoint).as_ref(),
        wallet,
        true,
    )
}

#[test]
fn settlement_requires_all_three_components_at_the_same_tip() {
    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(105), None),
    );
    assert!(status.settled);
    assert_eq!(status.settlement_blocker, None);

    let status = settlement_for_test(
        Some(105),
        Some(104),
        &wallet_status("ready", Some(105), None),
    );
    assert!(!status.settled);
    assert_eq!(status.settlement_blocker, Some("height_mismatch"));

    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(106), None),
    );
    assert!(
        !status.settled,
        "a wallet ahead of the current tip needs reconciliation"
    );

    let canonical = ChainCheckpoint {
        height: 105,
        hash: "canonical".to_owned(),
    };
    let replacement = ChainCheckpoint {
        height: 105,
        hash: "replacement".to_owned(),
    };
    let wallet = wallet_status("ready", Some(105), None);
    let status = settlement_status(Some(&canonical), Some(&replacement), &wallet, true);
    assert_eq!(status.settlement_blocker, Some("chain_mismatch"));

    let status = settlement_status(Some(&replacement), Some(&replacement), &wallet, true);
    assert!(
        !status.settled,
        "a same-height reorg needs wallet reconciliation"
    );
    assert_eq!(status.settlement_blocker, Some("wallet_behind"));
}

#[test]
fn settlement_reports_the_first_blocking_component() {
    let cases = [
        (
            None,
            Some(105),
            wallet_status("ready", Some(105), None),
            "node_unavailable",
        ),
        (
            Some(105),
            None,
            wallet_status("ready", Some(105), None),
            "lightwalletd_unavailable",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("error", Some(104), Some("scan failed")),
            "wallet_error",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("syncing", Some(104), None),
            "wallet_syncing",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", None, None),
            "wallet_behind",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", Some(104), None),
            "wallet_behind",
        ),
    ];

    for (node, lightwalletd, wallet, expected) in cases {
        let status = settlement_for_test(node, lightwalletd, &wallet);
        assert!(!status.settled);
        assert_eq!(status.settlement_blocker, Some(expected));
    }
}

#[test]
fn settlement_allows_a_canonical_tip_before_the_wallet_birthday() {
    let tip = ChainCheckpoint {
        height: u64::from(WALLET_BIRTHDAY_HEIGHT) - 1,
        hash: "canonical".to_owned(),
    };
    let mut wallet = wallet_status("ready", None, None);
    wallet.observed_height = Some(tip.height);
    wallet.observed_hash = Some(tip.hash.clone());
    assert!(settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
    wallet.observed_hash = Some("orphaned".to_owned());
    assert!(!settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
}

#[test]
fn settlement_waits_for_startup_after_the_components_agree() {
    let tip = ChainCheckpoint {
        height: 101,
        hash: "canonical".to_owned(),
    };
    let wallet = wallet_status("ready", Some(101), None);
    let status = settlement_status(Some(&tip), Some(&tip), &wallet, false);
    assert!(!status.settled);
    assert_eq!(status.settlement_blocker, Some("starting"));

    let status = settlement_status(None, Some(&tip), &wallet, false);
    assert_eq!(status.settlement_blocker, Some("node_unavailable"));

    assert!(settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
}

use crate::state::tests::{api_get, counting_rpc};
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn activity_lookup_by_key_returns_every_state_regardless_of_list_limits() {
    let (state, _dir) = state_with_local_wallet();
    let store = &state.0.store;
    let preparing = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            1_000,
            "lookup-preparing",
            Some(""),
        )
        .unwrap();
    let pending = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "transparent",
            2_000,
            "lookup-pending",
            None,
        )
        .unwrap();
    store
        .record_prepared(&pending.id, "pending-txid", b"raw", 140)
        .unwrap();
    store.mark_broadcast(&pending.id, "pending-txid").unwrap();
    let failed = store
        .claim_transfer(
            2,
            crate::db::Recipient::Account(1),
            "ironwood",
            "ironwood",
            3_000,
            "lookup-failed",
            None,
        )
        .unwrap();
    store.fail(&failed.id, "insufficient funds").unwrap();
    let confirmed = store
        .claim_faucet(3, "ironwood", 4_000, "lookup-confirmed")
        .unwrap();
    store
        .record_prepared(&confirmed.id, "confirmed-txid", b"raw", 140)
        .unwrap();
    store
        .confirm(&confirmed.id, "confirmed-txid", "block")
        .unwrap();
    store
        .claim_address_faucet("tm-external", 5, "lookup-external")
        .unwrap();

    let (status, listed) = api_get(&state, "/api/v1/activity?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["status"].clone(),
                row["txid"].clone(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                json!(confirmed.id),
                json!("confirmed"),
                json!("confirmed-txid")
            ),
            (json!(failed.id), json!("failed"), json!("")),
            (json!(pending.id), json!("broadcast"), json!("pending-txid")),
            (json!(preparing.id), json!("preparing"), json!("")),
        ]
    );

    for index in 0..101 {
        store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(3),
                "ironwood",
                "ironwood",
                1,
                &format!("filler-{index:03}"),
                None,
            )
            .unwrap();
    }
    let (_, listed) = api_get(&state, "/api/v1/activity?limit=100").await;
    assert_eq!(listed.as_array().unwrap().len(), 100);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != json!(preparing.id))
    );

    for (key, id, status, txid, last_error) in [
        (
            "lookup-preparing",
            &preparing.id,
            "preparing",
            "",
            Value::Null,
        ),
        (
            "lookup-pending",
            &pending.id,
            "broadcast",
            "pending-txid",
            Value::Null,
        ),
        (
            "lookup-failed",
            &failed.id,
            "failed",
            "",
            json!("insufficient funds"),
        ),
        (
            "lookup-confirmed",
            &confirmed.id,
            "confirmed",
            "confirmed-txid",
            Value::Null,
        ),
    ] {
        let (code, found) = api_get(
            &state,
            &format!("/api/v1/activity?limit=1&idempotency_key={key}"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{key}");
        let found = found.as_array().unwrap();
        assert_eq!(found.len(), 1, "{key}");
        assert_eq!(found[0]["id"], json!(id), "{key}");
        assert_eq!(found[0]["status"], status, "{key}");
        assert_eq!(found[0]["txid"], txid, "{key}");
        assert_eq!(found[0]["last_error"], last_error, "{key}");
    }
    for key in ["lookup-missing", "lookup-external"] {
        assert_eq!(
            api_get(&state, &format!("/api/v1/activity?idempotency_key={key}")).await,
            (StatusCode::OK, json!([]))
        );
    }
    let (code, _) = api_get(&state, "/api/v1/activity?idempotency_key=").await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stored_status_reads_have_no_wallet_or_node_side_effects() {
    let (state, dir) = state_with_local_wallet();
    let (rpc_url, calls) = counting_rpc(true).await;
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        rpc_url,
        "test".into(),
    );
    let store = &state.0.store;
    let send = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            1_000,
            "read-send",
            None,
        )
        .unwrap();
    store
        .claim_faucet(2, "transparent", 2_000, "read-internal")
        .unwrap();
    let external = store
        .claim_address_faucet("tm-external", 3_000, "read-external")
        .unwrap();
    let activities = store.unresolved_activities().unwrap();
    let faucets = store.unresolved_address_faucets().unwrap();

    for uri in [
        "/api/v1/activity",
        "/api/v1/activity?idempotency_key=read-send",
        "/api/v1/faucet/address?idempotency_key=read-internal",
        "/api/v1/faucet/address?idempotency_key=read-external",
        "/api/v1/faucet/address?idempotency_key=read-missing",
    ] {
        api_get(&state, uri).await;
    }

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.unresolved_activities().unwrap(), activities);
    assert_eq!(store.unresolved_address_faucets().unwrap(), faucets);
    for id in [&send.id, &external.id] {
        assert!(store.prepared_transaction(id).is_err());
        assert!(state.0.wallet.prepared(id).await.unwrap().is_none());
    }
    drop(dir);
}

use crate::state::tests::api_request;
#[tokio::test]
async fn health_and_chain_effects_wait_until_startup_finishes() {
    let (state, _dir) = state_with_local_wallet();
    let (rpc_url, calls) = counting_rpc(true).await;
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        rpc_url,
        "test".into(),
    );
    state.0.wallet_snapshot.write().await.status.last_success_at = Some(1);
    let account = state.0.store.account(2).unwrap();

    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
    assert_eq!(health["node"]["blocks"], 101);
    let health_calls = calls.load(Ordering::SeqCst);

    for (uri, body) in [
        ("/api/v1/send", valid_send()),
        (
            "/api/v1/faucet",
            json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 1_000, "idempotency_key": "startup-faucet"}),
        ),
        (
            "/api/v1/faucet/address",
            json!({"address": account.transparent_address, "amount_zatoshi": 1_000, "idempotency_key": "startup-address"}),
        ),
        ("/api/v1/mine", json!({"blocks": 1})),
        (
            "/api/v1/mining/jobs",
            json!({"blocks": 1, "idempotency_key": "startup-mining"}),
        ),
    ] {
        let (code, body) = api_request(&state, "POST", uri, body).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(
            body["error"]["message"],
            "the environment is still starting"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), health_calls);
    assert!(state.0.store.activities(100).unwrap().is_empty());
    assert!(
        state
            .0
            .store
            .unresolved_address_faucets()
            .unwrap()
            .is_empty()
    );
    assert!(state.0.mining.latest().await.is_none());

    state.mark_started();
    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(health["ok"], true);
}

use crate::api::payments::tests::valid_send;
