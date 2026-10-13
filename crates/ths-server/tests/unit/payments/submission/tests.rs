use super::*;
use crate::db::Recipient;
use crate::payments::progression::PAYMENT_DISPATCH_BUDGET;
use crate::payments::progression::repair_activity_journal;
use crate::payments::progression::repair_address_journal;
use crate::rpc::NodeRpc;
use crate::{
    db::{Activity, Store},
    wallet::PreparedPayment,
};
use crate::{
    payments::confirmation::tests::lifecycle_claim, state::tests::state_with_local_wallet,
};
use axum::{Json, Router, routing::post};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::time::Instant;
#[derive(Default)]
struct RecordingPaymentSubmitter {
    broadcasts: Mutex<Vec<Vec<u8>>>,
    lookups: Mutex<VecDeque<Result<bool, &'static str>>>,
    recovered: Mutex<Option<PreparedPayment>>,
    height: AtomicU64,
    fail_next: AtomicBool,
    queued_next: AtomicBool,
}

#[tokio::test]
async fn address_faucet_reconciles_the_original_txid_after_a_lost_broadcast_response() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let payment = store
        .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
        .unwrap();
    store
        .record_address_prepared(&payment.id, "original-txid", b"original signed bytes", 140)
        .unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let payment = store
        .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
        .unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([
            Ok(false),
            Ok(false),
            Err("node unavailable"),
            Ok(true),
        ])),
        height: AtomicU64::new(139),
        fail_next: AtomicBool::new(true),
        ..Default::default()
    };

    assert!(
        submit_address_prepared(&store, &runtime, &payment)
            .await
            .is_err()
    );
    let same = store
        .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
        .unwrap();
    let resumed = submit_address_prepared(&store, &runtime, &same)
        .await
        .unwrap();
    let AddressSubmission::Broadcast(resumed) = resumed else {
        panic!("original payment must resume")
    };
    assert_eq!(resumed.txid, "original-txid");
    assert_eq!(resumed.status, "broadcast");
    assert_eq!(
        runtime.broadcasts.into_inner().unwrap(),
        [b"original signed bytes"]
    );
}

#[async_trait::async_trait]
impl PaymentSubmitter for RecordingPaymentSubmitter {
    async fn lookup_original(&self, txid: &str) -> anyhow::Result<Option<Value>> {
        Ok(self
            .transaction_known(txid)
            .await?
            .then(|| json!({"txid":txid,"confirmations":0})))
    }
    async fn inclusion_height(&self, _hash: &str) -> anyhow::Result<u64> {
        anyhow::bail!("unexpected inclusion lookup")
    }
    async fn canonical_hash(&self, _height: u32) -> anyhow::Result<String> {
        anyhow::bail!("unexpected canonical lookup")
    }

    async fn transaction_known(&self, _txid: &str) -> anyhow::Result<bool> {
        self.lookups
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(false))
            .map_err(anyhow::Error::msg)
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.height.load(Ordering::SeqCst))
    }

    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
        self.broadcasts
            .lock()
            .unwrap()
            .push(raw_transaction.to_vec());
        if self.fail_next.swap(false, Ordering::SeqCst) {
            anyhow::bail!("response lost");
        }
        if self.queued_next.swap(false, Ordering::SeqCst) {
            crate::wallet::check_broadcast_response(
                zcash_client_backend::proto::service::SendResponse {
                    error_code: -1,
                    error_message: "transaction dropped because it is already queued for download"
                        .into(),
                },
            )?;
        }
        Ok(())
    }

    async fn recover_prepared(&self, _txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
        Ok(self.recovered.lock().unwrap().take())
    }
}

#[tokio::test]
async fn retry_submits_the_same_prepared_transaction_after_a_lost_response() {
    for (to, source, destination) in [
        (2, "ironwood", "ironwood"),
        (1, "ironwood", "transparent"),
        (1, "transparent", "ironwood"),
    ] {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(
                1,
                Recipient::Account(to),
                source,
                destination,
                12_000,
                "same",
                None,
            )
            .unwrap();
        store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();
        let prepared = store.activity_for_key("same").unwrap().unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([
                Ok(false),
                Ok(false),
                Err("node unavailable"),
                Ok(false),
                Ok(false),
            ])),
            height: AtomicU64::new(139),
            fail_next: AtomicBool::new(true),
            ..Default::default()
        };

        assert!(submit_prepared(&store, &runtime, &prepared).await.is_err());
        assert_eq!(
            store.activity_for_key("same").unwrap().unwrap().status,
            "prepared"
        );

        let broadcast = submit_prepared(&store, &runtime, &prepared).await.unwrap();
        assert!(matches!(broadcast, PreparedSubmission::Broadcast(_)));
        assert_eq!(
            runtime.broadcasts.into_inner().unwrap(),
            [b"signed transaction", b"signed transaction"]
        );
    }
}

#[tokio::test]
async fn a_forgotten_broadcast_is_resubmitted_from_the_saved_bytes() {
    let (store, prepared) = prepared_payment(140);
    let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false)])),
        height: AtomicU64::new(139),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &broadcast).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Broadcast(_)));
    assert_eq!(
        runtime.broadcasts.into_inner().unwrap(),
        [b"signed transaction"]
    );
    assert_eq!(
        store.activity_for_key("same").unwrap().unwrap().status,
        "broadcast"
    );
}

#[tokio::test]
async fn a_lookup_failure_does_not_broadcast_or_change_status() {
    let (store, prepared) = prepared_payment(140);
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Err("node unavailable")])),
        height: AtomicU64::new(139),
        ..Default::default()
    };

    assert!(submit_prepared(&store, &runtime, &prepared).await.is_err());
    assert!(runtime.broadcasts.into_inner().unwrap().is_empty());
    assert_eq!(
        store.activity_for_key("same").unwrap().unwrap().status,
        "prepared"
    );
}

#[tokio::test]
async fn queued_retry_keeps_the_original_payment_pending_until_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same-queued",
            None,
        )
        .unwrap();
    let prepared = store
        .record_prepared(&claim.id, "original-txid", b"original signed bytes", 140)
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false), Ok(true)])),
        height: AtomicU64::new(139),
        queued_next: AtomicBool::new(true),
        ..Default::default()
    };
    let PreparedSubmission::Broadcast(pending) =
        submit_prepared(&store, &runtime, &prepared).await.unwrap()
    else {
        panic!("queued bytes must not be replaced");
    };
    assert_eq!(pending.id, prepared.id);
    assert_eq!(pending.txid, "original-txid");
    assert_eq!(pending.status, "broadcast");
    assert!(pending.block_hash.is_none());
    assert_eq!(
        store
            .prepared_transaction(&pending.id)
            .unwrap()
            .raw_transaction,
        b"original signed bytes"
    );
    // A later lookup observes the same transaction without another broadcast.
    assert!(matches!(
        submit_prepared(&store, &runtime, &pending).await.unwrap(),
        PreparedSubmission::Broadcast(_)
    ));
    assert_eq!(
        *runtime.broadcasts.lock().unwrap(),
        [b"original signed bytes"]
    );
    let same = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same-queued",
            None,
        )
        .unwrap();
    assert_eq!(same.id, pending.id);
    assert_eq!(same.txid, pending.txid);
}

#[tokio::test]
async fn queued_external_faucet_retry_preserves_its_payment_and_bytes() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_address_faucet("external-regtest-address", 12_000, "queued-address")
        .unwrap();
    let prepared = store
        .record_address_prepared(&claim.id, "address-txid", b"address signed bytes", 140)
        .unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false)])),
        height: AtomicU64::new(139),
        queued_next: AtomicBool::new(true),
        ..Default::default()
    };
    let AddressSubmission::Broadcast(pending) =
        submit_address_prepared(&store, &runtime, &prepared)
            .await
            .unwrap()
    else {
        panic!("queued payment must remain pending");
    };
    assert_eq!(pending.id, prepared.id);
    assert_eq!(pending.txid, prepared.txid);
    assert_eq!(pending.status, "broadcast");
    assert!(pending.block_hash.is_none());
    assert_eq!(
        *runtime.broadcasts.lock().unwrap(),
        [b"address signed bytes"]
    );
}

#[tokio::test]
async fn a_broadcast_error_is_success_when_the_transaction_is_known() {
    let (store, prepared) = prepared_payment(140);
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false), Ok(true)])),
        height: AtomicU64::new(139),
        fail_next: AtomicBool::new(true),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Broadcast(_)));
    assert_eq!(
        store.activity_for_key("same").unwrap().unwrap().status,
        "broadcast"
    );
}

#[tokio::test]
async fn a_known_transaction_does_not_require_saved_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    let prepared = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute("DELETE FROM prepared_payments", [])
        .unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(true)])),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Broadcast(_)));
    assert_eq!(
        store.activity_for_key("same").unwrap().unwrap().status,
        "broadcast"
    );
}

#[tokio::test]
async fn a_legacy_broadcast_recovers_bytes_from_the_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    let prepared = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();
    let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute("DELETE FROM prepared_payments", [])
        .unwrap();
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false)])),
        recovered: Mutex::new(Some(PreparedPayment {
            txid: broadcast.txid.clone(),
            raw_transaction: b"wallet transaction".to_vec(),
            expiry_height: 140,
        })),
        height: AtomicU64::new(139),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &broadcast).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Broadcast(_)));
    assert_eq!(
        runtime.broadcasts.into_inner().unwrap(),
        [b"wallet transaction"]
    );
    assert_eq!(
        store
            .prepared_transaction(&broadcast.id)
            .unwrap()
            .raw_transaction,
        b"wallet transaction"
    );
}

#[tokio::test]
async fn an_expired_missing_transaction_is_reset_for_preparation() {
    let (store, prepared) = prepared_payment(140);
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false)])),
        height: AtomicU64::new(140),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Expired(_)));
    assert!(runtime.broadcasts.into_inner().unwrap().is_empty());
    let retry = store.activity_for_key("same").unwrap().unwrap();
    assert_eq!(retry.status, "preparing");
    assert!(retry.txid.is_empty());
}

#[tokio::test]
async fn a_transaction_seen_at_expiry_is_not_replaced() {
    let (store, prepared) = prepared_payment(140);
    let runtime = RecordingPaymentSubmitter {
        lookups: Mutex::new(VecDeque::from([Ok(false), Ok(true)])),
        height: AtomicU64::new(140),
        ..Default::default()
    };

    let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

    assert!(matches!(result, PreparedSubmission::Broadcast(_)));
    let current = store.activity_for_key("same").unwrap().unwrap();
    assert_eq!(current.txid, "real-txid");
    assert_eq!(current.status, "broadcast");
}

fn prepared_payment(expiry_height: u64) -> (Store, Activity) {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", expiry_height)
        .unwrap();
    let prepared = store.activity_for_key("same").unwrap().unwrap();
    (store, prepared)
}

#[tokio::test]
async fn payment_presence_rejects_a_mismatched_transaction_identity() {
    let (mut state, _dir) = state_with_local_wallet();
    let app = Router::new().route(
        "/",
        post(|| async {
            Json(json!({"result":{"txid":"another-transaction","confirmations":0},"error":null}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Arc::get_mut(&mut state.0).unwrap().rpc =
        NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let attempt = PaymentAttempt {
        state: &state,
        dispatch_deadline: Some(Instant::now() + Duration::from_secs(1)),
    };
    assert!(
        PaymentSubmitter::transaction_known(&attempt, "expected-transaction")
            .await
            .is_err()
    );
    assert!(
        PaymentSubmitter::transaction_known(&state, "expected-transaction")
            .await
            .is_err()
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn original_presence_accepts_zakura_mempool_responses_without_confirmations() {
    let runtime = RecordingPaymentSubmitter::default();
    let mempool = json!({"txid": "original-txid", "hex": "010203", "vin": [], "vout": []});
    for confirmations in [None, Some(Value::Null), Some(json!(0))] {
        for blockhash in [None, Some(Value::Null)] {
            let mut transaction = mempool.clone();
            if let Some(confirmations) = &confirmations {
                transaction["confirmations"] = confirmations.clone();
            }
            if let Some(blockhash) = blockhash {
                transaction["blockhash"] = blockhash;
            }
            assert_eq!(
                original_presence_from_lookup(&runtime, "original-txid", Some(transaction))
                    .await
                    .unwrap(),
                OriginalTransactionState::Present,
            );
        }
    }
    for confirmations in [json!("0"), json!(false), json!(0.5), json!([]), json!(-1)] {
        let mut transaction = mempool.clone();
        transaction["confirmations"] = confirmations;
        assert!(
            original_presence_from_lookup(&runtime, "original-txid", Some(transaction))
                .await
                .is_err()
        );
    }
    for confirmations in [None, Some(Value::Null), Some(json!(0))] {
        let mut transaction = mempool.clone();
        transaction["blockhash"] = json!("an-inclusion-block");
        if let Some(confirmations) = confirmations {
            transaction["confirmations"] = confirmations;
        }
        assert!(
            original_presence_from_lookup(&runtime, "original-txid", Some(transaction))
                .await
                .is_err()
        );
    }
    let mut wrong_identity = mempool.clone();
    wrong_identity["txid"] = json!("another-txid");
    assert!(
        original_presence_from_lookup(&runtime, "original-txid", Some(wrong_identity))
            .await
            .is_err()
    );
    let mut unverified_inclusion = mempool;
    unverified_inclusion["confirmations"] = json!(1);
    unverified_inclusion["blockhash"] = json!("an-inclusion-block");
    assert!(
        original_presence_from_lookup(&runtime, "original-txid", Some(unverified_inclusion))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn saved_original_inclusion_wins_over_expiry_only_on_the_selected_chain() {
    for (mode, expected_status) in [
        ("included", "confirmed"),
        ("boundary", "confirmed"),
        ("orphan", "prepared"),
        ("malformed", "prepared"),
        ("unavailable", "prepared"),
    ] {
        let (mut state, dir) = state_with_local_wallet();
        let claim = lifecycle_claim(&state.0.store, mode);
        rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap().execute("INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'original-txid',?2,140)", rusqlite::params![claim.id, b"signed original"]).unwrap();
        repair_activity_journal(&state, &claim)
            .await
            .unwrap()
            .unwrap();
        let prepared = state.0.store.activity_for_key(mode).unwrap().unwrap();
        let lookups = Arc::new(AtomicUsize::new(0));
        let rpc_lookups = lookups.clone();
        let app = Router::new().route("/", post(move |Json(request): Json<Value>| {
                let lookups = Arc::clone(&rpc_lookups);
                async move {
                    let result = match request["method"].as_str().unwrap() {
                        "getrawtransaction" => {
                            let index = lookups.fetch_add(1, Ordering::SeqCst);
                            if (mode == "boundary" || mode == "unavailable") && index == 0 {
                                return Json(json!({"result":null,"error":{"code":-5,"message":"missing"}}));
                            }
                            if mode == "unavailable" {
                                return Json(json!({"result":null,"error":{"code":-28,"message":"unavailable"}}));
                            }
                            if mode == "malformed" { json!({"txid":"original-txid","confirmations":1}) }
                            else { json!({"txid":"original-txid","confirmations":1,"blockhash":"original-block"}) }
                        }
                        "getblockchaininfo" => json!({"chain":"regtest","blocks":140}),
                        "getblock" => json!({"height":140}),
                        "getblockhash" => json!(if mode == "orphan" {"different-block"} else {"original-block"}),
                        method => panic!("unexpected RPC {method}"),
                    };
                    Json(json!({"result":result,"error":null}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Arc::get_mut(&mut state.0).unwrap().rpc =
            NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let attempt = PaymentAttempt {
            state: &state,
            dispatch_deadline: Some(Instant::now() + PAYMENT_DISPATCH_BUDGET),
        };
        let result = submit_prepared(&state.0.store, &attempt, &prepared).await;
        if expected_status == "confirmed" {
            assert!(result.is_ok(), "{mode}");
        } else {
            assert!(result.is_err(), "{mode} must remain pending");
        }
        let saved = state.0.store.activity_for_key(mode).unwrap().unwrap();
        assert_eq!(saved.status, expected_status, "{mode}");
        assert_eq!(saved.id, claim.id);
        assert_eq!(saved.txid, "original-txid");
        assert_eq!(
            state
                .0
                .store
                .prepared_transaction(&claim.id)
                .unwrap()
                .raw_transaction,
            b"signed original"
        );
        let external = state
            .0
            .store
            .claim_address_faucet("external-address", 12_000, &format!("external-{mode}"))
            .unwrap();
        rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap().execute("INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'original-txid',?2,140)", rusqlite::params![external.id, b"signed original"]).unwrap();
        repair_address_journal(&state, &external)
            .await
            .unwrap()
            .unwrap();
        let external_prepared = state
            .0
            .store
            .address_faucet_for_key(&format!("external-{mode}"))
            .unwrap()
            .unwrap();
        lookups.store(0, Ordering::SeqCst);
        let result = submit_address_prepared(&state.0.store, &attempt, &external_prepared).await;
        assert_eq!(
            result.is_ok(),
            expected_status == "confirmed",
            "external {mode}"
        );
        let saved_external = state
            .0
            .store
            .address_faucet_for_key(&format!("external-{mode}"))
            .unwrap()
            .unwrap();
        assert_eq!(saved_external.status, expected_status);
        assert_eq!(saved_external.id, external.id);
        assert_eq!(saved_external.txid, "original-txid");
        for id in [&claim.id, &external.id] {
            let journal = state.0.wallet.prepared(id).await.unwrap().unwrap();
            assert_eq!(journal.txid, "original-txid");
            assert_eq!(journal.raw_transaction, b"signed original");
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn zero_expiry_and_mempool_presence_retain_the_original_payment() {
    for (expiry, present) in [(0, false), (140, true)] {
        let (store, prepared) = prepared_payment(expiry);
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(present)])),
            height: AtomicU64::new(500),
            ..Default::default()
        };
        let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();
        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        let saved = store.activity_for_key("same").unwrap().unwrap();
        assert_eq!(saved.txid, "real-txid");
        assert_eq!(saved.status, "broadcast");
        assert_eq!(
            *runtime.broadcasts.lock().unwrap(),
            if present {
                vec![]
            } else {
                vec![b"signed transaction".to_vec()]
            }
        );
    }
}
