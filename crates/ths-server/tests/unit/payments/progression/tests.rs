use super::*;
use crate::payments::confirmation::tests::lifecycle_claim;
use crate::state::tests::started_state_with_local_wallet as state_with_local_wallet;
use crate::{
    db::{PreparedTransaction, Recipient, Store},
    rpc::NodeRpc,
    wallet::RealWallet,
};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

async fn journal_height_rpc(slow: Arc<AtomicBool>) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let slow = slow.clone();
            async move {
                let result = match request["method"].as_str().unwrap() {
                    "getblockchaininfo" => {
                        if slow.load(Ordering::SeqCst) {
                            tokio::time::sleep(PAYMENT_DISPATCH_BUDGET + Duration::from_secs(1))
                                .await;
                        }
                        json!({"chain":"regtest","blocks":100,"bestblockhash":"ab".repeat(32),"verificationprogress":1.0})
                    }
                    "getrawtransaction" => {
                        let txid = request["params"][0].as_str().unwrap();
                        assert!(matches!(txid, "journal-txid" | "fast-external-txid"));
                        json!({"txid":txid,"confirmations":1,"blockhash":"confirmed-block"})
                    }
                    method => panic!("unexpected journal recovery RPC {method}"),
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result,"error":null}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, server)
}

#[tokio::test]
async fn slow_journal_height_lookup_preserves_recovery_and_external_dispatch_across_passes() {
    let slow = Arc::new(AtomicBool::new(true));
    let (endpoint, server) = journal_height_rpc(slow.clone()).await;
    let (mut state, dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
    let internal = lifecycle_claim(&state.0.store, "slow-journal");
    let wallet_db = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    wallet_db
        .execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
            rusqlite::params![internal.id, "journal-txid", b"original signed bytes", 140_u64],
        )
        .unwrap();

    let mut activity_cursor = None;
    let mut address_cursor = None;
    for key in ["fast-external-first-pass", "fast-external-second-pass"] {
        let external = state
            .0
            .store
            .claim_address_faucet("tm-external", 12_000, key)
            .unwrap();
        state
            .0
            .store
            .record_address_prepared(&external.id, "fast-external-txid", b"external bytes", 0)
            .unwrap();
        progress_payment_intents(
            &state,
            Instant::now() + PAYMENT_DISPATCH_BUDGET,
            &mut activity_cursor,
            &mut address_cursor,
        )
        .await
        .unwrap();

        let progressed = state.0.store.address_faucet_for_key(key).unwrap().unwrap();
        assert_eq!(progressed.status, "confirmed");
        assert_eq!(progressed.txid, "fast-external-txid");
        assert_eq!(progressed.block_hash.as_deref(), Some("confirmed-block"));
        let pending = state
            .0
            .store
            .activity_for_key("slow-journal")
            .unwrap()
            .unwrap();
        assert_eq!(pending.status, "preparing");
        assert!(pending.txid.is_empty());
        assert!(pending.last_error.is_some());
        assert!(
            state
                .0
                .store
                .prepared_transaction_expiry(&internal.id)
                .unwrap()
                .is_none()
        );
        let journal = state
            .0
            .wallet
            .prepared(&internal.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.txid, "journal-txid");
        assert_eq!(journal.raw_transaction, b"original signed bytes");
        assert_eq!(journal.expiry_height, 140);
    }

    slow.store(false, Ordering::SeqCst);
    progress_payment_intents(
        &state,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        &mut activity_cursor,
        &mut address_cursor,
    )
    .await
    .unwrap();
    let recovered = state
        .0
        .store
        .activity_for_key("slow-journal")
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, internal.id);
    assert_eq!(recovered.status, "confirmed");
    assert_eq!(recovered.txid, "journal-txid");
    let saved = state.0.store.prepared_transaction(&internal.id).unwrap();
    assert_eq!(saved.raw_transaction, b"original signed bytes");
    assert_eq!(saved.expiry_height, 140);

    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn journal_height_lookup_caps_both_repair_paths_without_extending_the_pass_deadline() {
    let (endpoint, server) = journal_height_rpc(Arc::new(AtomicBool::new(true))).await;
    let (mut state, dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
    let internal = lifecycle_claim(&state.0.store, "bounded-internal-journal");
    let external = state
        .0
        .store
        .claim_address_faucet("tm-external", 12_000, "bounded-external-journal")
        .unwrap();
    let wallet_db = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    for id in [&internal.id, &external.id] {
        wallet_db
            .execute(
                "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
                rusqlite::params![id, "journal-txid", b"original signed bytes", 140_u64],
            )
            .unwrap();
    }

    for budget in [PAYMENT_DISPATCH_BUDGET, Duration::from_millis(100)] {
        for internal_path in [true, false] {
            let started = Instant::now();
            let deadline = started + budget;
            let (id, result) = if internal_path {
                (
                    &internal.id,
                    repair_activity_journal(&state, &internal, deadline).await,
                )
            } else {
                (
                    &external.id,
                    repair_address_journal(&state, &external, deadline).await,
                )
            };
            let error = result.err().unwrap();
            assert!(
                error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
            );
            assert!(
                started.elapsed() < budget.min(PAYMENT_LOOKUP_TIMEOUT) + Duration::from_secs(1)
            );
            let journal = state.0.wallet.prepared(id).await.unwrap().unwrap();
            assert_eq!(journal.txid, "journal-txid");
            assert_eq!(journal.raw_transaction, b"original signed bytes");
            assert_eq!(journal.expiry_height, 140);
        }
    }

    wallet_db
        .execute("UPDATE ext_tsz_prepared_payments SET expiry_height=0", [])
        .unwrap();
    let internal_journal = repair_activity_journal(&state, &internal, Instant::now())
        .await
        .unwrap()
        .unwrap();
    let external_journal = repair_address_journal(&state, &external, Instant::now())
        .await
        .unwrap()
        .unwrap();
    for journal in [internal_journal, external_journal] {
        assert_eq!(journal.txid, "journal-txid");
        assert_eq!(journal.raw_transaction, b"original signed bytes");
        assert_eq!(journal.expiry_height, 0);
    }

    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn slow_internal_lookup_does_not_starve_external_faucet_dispatch() {
    let external_lookups = Arc::new(AtomicUsize::new(0));
    let lookup_count = external_lookups.clone();
    let app = Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let lookup_count = lookup_count.clone();
            async move {
                let txid = request["params"][0].as_str().unwrap();
                if txid == "slow-internal-txid" {
                    tokio::time::sleep(PAYMENT_LOOKUP_TIMEOUT + Duration::from_secs(1)).await;
                    return Json(json!({
                        "jsonrpc":"2.0",
                        "id":request["id"],
                        "result":null,
                        "error":{"code":-5,"message":"transaction not found"}
                    }));
                }
                lookup_count.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "jsonrpc":"2.0",
                    "id":request["id"],
                    "result":{"txid":txid,"confirmations":1,"blockhash":"external-block"},
                    "error":null
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);

    let internal = lifecycle_claim(&state.0.store, "slow-internal");
    state
        .0
        .store
        .record_prepared(&internal.id, "slow-internal-txid", b"internal bytes", 0)
        .unwrap();
    let external = state
        .0
        .store
        .claim_address_faucet("tm-external", 12_000, "fast-external")
        .unwrap();
    state
        .0
        .store
        .record_address_prepared(&external.id, "fast-external-txid", b"external bytes", 0)
        .unwrap();

    let mut activity_cursor = None;
    let mut address_cursor = None;
    progress_payment_intents(
        &state,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        &mut activity_cursor,
        &mut address_cursor,
    )
    .await
    .unwrap();

    let progressed = state
        .0
        .store
        .address_faucet_for_key("fast-external")
        .unwrap()
        .unwrap();
    assert_eq!(progressed.status, "confirmed");
    assert!(external_lookups.load(Ordering::SeqCst) >= 1);
    let pending_internal = state
        .0
        .store
        .activity_for_key("slow-internal")
        .unwrap()
        .unwrap();
    assert_eq!(pending_internal.status, "prepared");
    assert!(pending_internal.last_error.is_some());

    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn insufficient_send_is_terminal_for_same_key_but_a_new_key_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "insufficient-send");
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(
        store.clone(),
        wallet,
        "http://127.0.0.1:1".into(),
        "test".into(),
    );
    state.mark_started();

    let failure = || {
        std::future::ready(Err(anyhow::Error::new(PaymentError::InsufficientFunds {
            available: 0,
            required: 12_000,
        })))
    };
    assert!(
        execute_payment(&state, claim.clone(), |_| failure())
            .await
            .is_err()
    );
    let failed = store
        .activity_for_key("insufficient-send")
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert!(
        failed
            .last_error
            .as_deref()
            .unwrap()
            .contains("insufficient")
    );

    drop(state);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    reopened.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &reopened.seed().unwrap()).unwrap();
    let state = AppState::new(
        reopened.clone(),
        wallet,
        "http://127.0.0.1:1".into(),
        "test".into(),
    );
    state.mark_started();
    let replay = lifecycle_claim(&reopened, "insufficient-send");
    assert_eq!(replay.id, failed.id);
    assert_eq!(replay.status, "failed");
    let prepare_calls = AtomicUsize::new(0);
    let replayed = execute_payment(&state, replay, |_| {
        prepare_calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(PreparedPayment {
            txid: "later-funded-txid".into(),
            raw_transaction: "later-funded-raw".into(),
            expiry_height: 100,
        }))
    })
    .await
    .unwrap();
    assert_eq!(replayed.status, "failed");
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);

    let new_key = lifecycle_claim(&reopened, "insufficient-send-retry");
    assert_ne!(new_key.id, failed.id);
    assert_eq!(new_key.status, "preparing");
}

#[tokio::test]
async fn insufficient_send_policy_waits_for_wallet_journal_and_checks_server_transaction() {
    let (state, dir) = state_with_local_wallet();
    let claim = state
        .0
        .store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "policy-store-transaction",
            None,
        )
        .unwrap();
    assert!(can_fail_insufficient_send(&state, &claim.id).await);

    state
        .0
        .store
        .record_prepared(&claim.id, "server-txid", b"signed", 140)
        .unwrap();
    assert!(!can_fail_insufficient_send(&state, &claim.id).await);

    let wallet_claim = state
        .0
        .store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "policy-wallet-transaction",
            None,
        )
        .unwrap();
    rusqlite::Connection::open(dir.path().join("wallet.db"))
        .unwrap()
        .execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
            rusqlite::params![wallet_claim.id, "wallet-txid", b"signed", 140_u64],
        )
        .unwrap();
    assert!(!can_fail_insufficient_send(&state, &wallet_claim.id).await);
}

#[test]
fn progression_errors_are_recorded_against_post_transition_state() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();

    let internal = lifecycle_claim(&store, "transition-error-internal");
    let original_internal = store
        .unresolved_activities()
        .unwrap()
        .into_iter()
        .find(|intent| intent.key == "transition-error-internal")
        .unwrap();
    store
        .record_prepared(&internal.id, "current-internal-txid", b"signed", 140)
        .unwrap();
    record_current_activity_error(&store, &original_internal.key, "broadcast unavailable").unwrap();
    let current_internal = store
        .activity_for_key("transition-error-internal")
        .unwrap()
        .unwrap();
    assert_eq!(current_internal.status, "prepared");
    assert_eq!(current_internal.txid, "current-internal-txid");
    assert_eq!(
        current_internal.last_error.as_deref(),
        Some("broadcast unavailable")
    );

    let external = store
        .claim_address_faucet("tm-external", 12_000, "transition-error-external")
        .unwrap();
    let intent = store
        .unresolved_address_faucets()
        .unwrap()
        .into_iter()
        .find(|intent| intent.key == "transition-error-external")
        .unwrap();
    store
        .record_address_prepared(&external.id, "current-external-txid", b"signed", 140)
        .unwrap();
    record_current_address_error(&store, &intent.key, "broadcast unavailable").unwrap();
    let current_external = store
        .address_faucet_for_key("transition-error-external")
        .unwrap()
        .unwrap();
    assert_eq!(current_external.status, "prepared");
    assert_eq!(current_external.txid, "current-external-txid");
    assert_eq!(
        current_external.last_error.as_deref(),
        Some("broadcast unavailable")
    );
}

#[tokio::test]
async fn server_store_repairs_preparation_from_the_wallet_journal() {
    let (mut state, dir) = state_with_local_wallet();
    let claim = state
        .0
        .store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "journal-repair",
            None,
        )
        .unwrap();
    rusqlite::Connection::open(dir.path().join("wallet.db"))
        .unwrap()
        .execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
            rusqlite::params![claim.id, "journal-txid", b"journaled signed bytes", 150_u64],
        )
        .unwrap();

    let rpc = Router::new().route(
        "/",
        post(|Json(request): Json<Value>| async move {
            let result = match request["method"].as_str().unwrap() {
                "getblockchaininfo" => json!({"chain":"regtest","blocks":100}),
                "getrawtransaction" => json!({
                    "txid":"journal-txid",
                    "confirmations":1,
                    "blockhash":"journal-block"
                }),
                method => panic!("unexpected RPC method {method}"),
            };
            Json(json!({
                "jsonrpc":"2.0",
                "id":request["id"],
                "result":result,
                "error":null
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);

    let intent = state
        .0
        .store
        .unresolved_activities()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    progress_activity(&state, &intent, claim.clone())
        .await
        .unwrap();
    let repaired = state
        .0
        .store
        .activity_for_key("journal-repair")
        .unwrap()
        .unwrap();

    assert_eq!(
        (repaired.status.as_str(), repaired.txid.as_str()),
        ("confirmed", "journal-txid")
    );
    assert_eq!(repaired.block_hash.as_deref(), Some("journal-block"));
    assert_eq!(
        state.0.store.prepared_transaction(&claim.id).unwrap(),
        PreparedTransaction {
            raw_transaction: b"journaled signed bytes".to_vec(),
            expiry_height: 150
        }
    );
}
