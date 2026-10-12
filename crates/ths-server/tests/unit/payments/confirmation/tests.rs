use super::*;
use crate::db::Store;
use serde_json::json;
#[test]
fn confirmed_block_hash_requires_confirmations_and_blockhash() {
    assert_eq!(
        confirmed_block_hash(&json!({
            "txid": "abc",
            "confirmations": 1,
            "blockhash": "0".repeat(64)
        })),
        Some("0000000000000000000000000000000000000000000000000000000000000000")
    );
    assert_eq!(
        confirmed_block_hash(&json!({
            "txid": "abc",
            "confirmations": 0,
            "blockhash": "0".repeat(64)
        })),
        None
    );
    assert_eq!(
        confirmed_block_hash(&json!({"txid": "abc", "confirmations": 3})),
        None
    );
    assert_eq!(
        confirmed_block_hash(&json!({"txid": "abc", "blockhash": "0".repeat(64)})),
        None
    );
    assert_eq!(
        confirmed_block_hash(&json!({
            "txid": "abc",
            "confirmations": 1,
            "blockhash": ""
        })),
        None
    );
    assert_eq!(confirmed_block_hash(&json!({"txid": "abc"})), None);
}

#[test]
fn stale_confirmation_does_not_confirm_a_replacement_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claimed = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    store
        .record_prepared(&claimed.id, "old-txid", b"old transaction", 140)
        .unwrap();
    let old = store.activity_for_key("same").unwrap().unwrap();
    store.reset_for_retry(&claimed.id, "old-txid").unwrap();
    store
        .record_prepared(&claimed.id, "new-txid", b"new transaction", 180)
        .unwrap();

    let current = apply_confirmation(
        &store,
        &old,
        &serde_json::json!({
            "confirmations": 1,
            "blockhash": "old-block",
        }),
    )
    .unwrap();

    assert_eq!(current.txid, "new-txid");
    assert_eq!(current.status, "prepared");
    assert_eq!(current.block_hash, None);
}

#[test]
fn apply_confirmation_uses_node_blockhash_not_a_generate_hash() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let pending = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "issue-67", None)
        .unwrap();
    let pending = store
        .record_prepared(&pending.id, "txid-abc", b"raw transaction", 140)
        .unwrap();
    let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();
    assert_eq!(pending.status, "broadcast");

    let generate_hash = "generate-hash-that-must-not-be-stored";
    let mempool = json!({"txid": "txid-abc", "confirmations": 0});
    let still = apply_confirmation(&store, &pending, &mempool).unwrap();
    assert_eq!(still.status, "broadcast");
    assert_eq!(still.block_hash, None);

    let mined = json!({
        "txid": "txid-abc",
        "confirmations": 1,
        "blockhash": "b".repeat(64)
    });
    let confirmed = apply_confirmation(&store, &pending, &mined).unwrap();
    let expected_hash = "b".repeat(64);
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(
        confirmed.block_hash.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_ne!(confirmed.block_hash.as_deref(), Some(generate_hash));
    assert_eq!(confirmed.txid, "txid-abc");
}

use crate::state::tests::state_with_local_wallet;
use crate::wallet::RealWallet;
use axum::{Json, Router, routing::post};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::broadcast;

fn lifecycle_claim(store: &Store, key: &str) -> Activity {
    store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, key, None)
        .unwrap()
}

fn confirmation_claim(store: &Store, key: &str) -> Activity {
    let claim = lifecycle_claim(store, key);
    let prepared = store
        .record_prepared(&claim.id, key, b"signed", 140)
        .unwrap();
    store.mark_broadcast(&prepared.id, key).unwrap()
}

#[tokio::test]
async fn payment_confirmation_loop_recovers_reopened_records_without_wallet_sync() {
    let (state, dir) = state_with_local_wallet();
    confirmation_claim(&state.0.store, "reopened-txid");
    drop(state);
    let calls = Arc::new(AtomicUsize::new(0));
    let rpc_calls = calls.clone();
    let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
        let calls = rpc_calls.clone();
        async move {
            if request["method"] == "getblockchaininfo" {
                return Json(json!({"result": {"chain": "regtest", "blocks": 2, "bestblockhash": "00".repeat(32)}, "error": null, "id": request["id"]}));
            }
            assert_eq!(request["method"], "getrawtransaction");
            assert_eq!(request["params"][0], "reopened-txid");
            let response = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                json!({"result": null, "error": {"code": -5, "message": "not found"}, "id": request["id"]})
            } else {
                json!({"result": {"txid": "reopened-txid", "confirmations": 1, "blockhash": "inclusion-block"}, "error": null, "id": request["id"]})
            };
            Json(response)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let store = Store::open(dir.path().join("server.db")).unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, rpc_url, "test".into());
    let mut updates = state.0.events.subscribe();
    // The production loop must progress even while synchronization owns
    // its guard, and must not require a new block or a payment POST.
    let _sync = state.0.wallet_sync.lock().await;
    let worker = tokio::spawn(payment_confirmation_loop(state.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(updates.recv().await.unwrap(), "wallet");
    })
    .await
    .unwrap();
    let confirmed = state
        .0
        .store
        .activity_for_key("reopened-txid")
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(confirmed.txid, "reopened-txid");
    assert_eq!(confirmed.block_hash.as_deref(), Some("inclusion-block"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn payment_confirmation_loop_survives_request_confirmation_cancellation() {
    let (state, dir) = state_with_local_wallet();
    let claim = confirmation_claim(&state.0.store, "cancelled-request-txid");
    let store = state.0.store.clone();
    drop(state);
    let (entered, lookup_started) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release_lookup, released) = tokio::sync::oneshot::channel();
    let released = Arc::new(Mutex::new(Some(released)));
    let calls = Arc::new(AtomicUsize::new(0));
    let rpc_calls = calls.clone();
    let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
        let entered = entered.clone();
        let released = released.clone();
        let calls = rpc_calls.clone();
        async move {
            if request["method"] == "getblockchaininfo" {
                return Json(json!({"result": {"chain": "regtest", "blocks": 2, "bestblockhash": "00".repeat(32)}, "error": null, "id": request["id"]}));
            }
            assert_eq!(request["method"], "getrawtransaction");
            assert_eq!(request["params"][0], "cancelled-request-txid");
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let released = released.lock().unwrap().take().unwrap();
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                // Cancellation happens after the request-side lookup starts,
                // before it can confirm, auto-mine, or notify subscribers.
                tokio::time::timeout(Duration::from_secs(10), released)
                    .await
                    .unwrap()
                    .unwrap();
            }
            Json(json!({
                "result": {"txid": "cancelled-request-txid", "confirmations": 1, "blockhash": "recovered-block"},
                "error": null,
                "id": request["id"]
            }))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, rpc_url, "test".into());
    let mut updates = state.0.events.subscribe();
    let request_state = state.clone();
    let request_claim = claim.clone();
    let request =
        tokio::spawn(async move { confirm_after_mining(&request_state, request_claim).await });
    let lookup_started = tokio::time::timeout(Duration::from_secs(5), lookup_started).await;
    request.abort();
    let request_result = request.await;
    let lookup_released = release_lookup.send(());
    let pending = state.0.store.activity_for_key("cancelled-request-txid");
    let signed = state.0.store.prepared_transaction(&claim.id);
    let no_request_update = updates.try_recv();
    // Request cancellation must not cancel server-owned work. Holding the
    // sync guard also proves recovery does not depend on wallet scanning.
    let _sync = state.0.wallet_sync.lock().await;
    let worker = tokio::spawn(payment_confirmation_loop(state.clone()));
    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await;
    worker.abort();
    let worker_result = worker.await;
    server.abort();
    let server_result = server.await;

    lookup_started.unwrap().unwrap();
    lookup_released.unwrap();
    assert!(request_result.unwrap_err().is_cancelled());
    let pending = pending.unwrap().unwrap();
    assert_eq!(pending.id, claim.id);
    assert_eq!(pending.txid, claim.txid);
    assert_eq!(pending.status, "broadcast");
    assert!(pending.block_hash.is_none());
    assert_eq!(signed.unwrap().raw_transaction, b"signed");
    assert!(matches!(
        no_request_update,
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(update.unwrap().unwrap(), "wallet");
    let confirmed = state
        .0
        .store
        .activity_for_key("cancelled-request-txid")
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.id, claim.id);
    assert_eq!(confirmed.txid, claim.txid);
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(confirmed.block_hash.as_deref(), Some("recovered-block"));
    assert_eq!(
        state
            .0
            .store
            .prepared_transaction(&claim.id)
            .unwrap()
            .raw_transaction,
        b"signed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(worker_result.unwrap_err().is_cancelled());
    assert!(server_result.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn payment_confirmation_pass_continues_after_unknown_or_unavailable_transactions() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    for key in ["missing-txid", "unavailable-txid", "included-txid"] {
        confirmation_claim(&store, key);
    }
    let changed = reconcile_unconfirmed_with(
        &store,
        &mut None,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        |txid| async move {
            match txid.as_str() {
                "missing-txid" => Ok(None),
                "unavailable-txid" => anyhow::bail!("node unavailable"),
                "included-txid" => Ok(Some(json!({
                    "confirmations": 1, "blockhash": "canonical-block"
                }))),
                _ => unreachable!(),
            }
        },
    )
    .await
    .unwrap();
    assert!(changed);
    for key in ["missing-txid", "unavailable-txid"] {
        let activity = store.activity_for_key(key).unwrap().unwrap();
        assert_eq!(activity.status, "broadcast");
        assert_eq!(activity.txid, key);
        assert!(activity.block_hash.is_none());
        assert_eq!(
            store
                .prepared_transaction(&activity.id)
                .unwrap()
                .raw_transaction,
            b"signed"
        );
    }
    let included = store.activity_for_key("included-txid").unwrap().unwrap();
    assert_eq!(included.status, "confirmed");
    assert_eq!(included.block_hash.as_deref(), Some("canonical-block"));
}

#[tokio::test(start_paused = true)]
async fn payment_confirmation_pass_resumes_after_a_stalled_lookup_exhausts_budget() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let mut claims = [
        confirmation_claim(&store, "first-txid"),
        confirmation_claim(&store, "second-txid"),
    ];
    claims.sort_by(|left, right| left.id.cmp(&right.id));
    let mut cursor = None;
    let changed = reconcile_unconfirmed_with(
        &store,
        &mut cursor,
        Instant::now() + Duration::from_millis(100),
        |_| std::future::pending::<anyhow::Result<Option<Value>>>(),
    )
    .await
    .unwrap();
    assert!(!changed);
    assert_eq!(cursor.as_deref(), Some(claims[0].id.as_str()));
    let calls = std::sync::Mutex::new(Vec::new());
    reconcile_unconfirmed_with(
        &store,
        &mut cursor,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        |txid| {
            calls.lock().unwrap().push(txid);
            std::future::ready(Ok(None))
        },
    )
    .await
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [claims[1].txid.clone(), claims[0].txid.clone()]
    );
}

#[tokio::test]
async fn payment_confirmation_cursor_survives_a_confirmed_record_leaving_the_set() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let mut claims = [
        confirmation_claim(&store, "alpha-txid"),
        confirmation_claim(&store, "beta-txid"),
        confirmation_claim(&store, "gamma-txid"),
    ];
    claims.sort_by(|left, right| left.id.cmp(&right.id));
    store
        .confirm(&claims[1].id, &claims[1].txid, "block")
        .unwrap();
    let mut cursor = Some(claims[1].id.clone());
    let calls = std::sync::Mutex::new(Vec::new());
    reconcile_unconfirmed_with(
        &store,
        &mut cursor,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        |txid| {
            calls.lock().unwrap().push(txid);
            std::future::ready(Ok(None))
        },
    )
    .await
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [claims[2].txid.clone(), claims[0].txid.clone()]
    );
}

#[tokio::test]
async fn payment_confirmation_does_not_confirm_a_mempool_transaction_or_a_replacement() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = confirmation_claim(&store, "original-txid");
    let mut cursor = None;
    reconcile_unconfirmed_with(
        &store,
        &mut cursor,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        |_| std::future::ready(Ok(Some(json!({"confirmations": 0})))),
    )
    .await
    .unwrap();
    assert_eq!(
        store
            .activity_for_key("original-txid")
            .unwrap()
            .unwrap()
            .status,
        "broadcast"
    );
    let changed = reconcile_unconfirmed_with(
        &store,
        &mut cursor,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        |_| {
            store.reset_for_retry(&claim.id, &claim.txid).unwrap();
            store
                .record_prepared(&claim.id, "replacement-txid", b"replacement", 180)
                .unwrap();
            std::future::ready(Ok(Some(
                json!({"confirmations": 1, "blockhash": "old-block"}),
            )))
        },
    )
    .await
    .unwrap();
    assert!(!changed);
    let replacement = store.activity_for_key("original-txid").unwrap().unwrap();
    assert_eq!(replacement.txid, "replacement-txid");
    assert_eq!(replacement.status, "prepared");
    assert!(replacement.block_hash.is_none());
}
