use super::*;
use crate::{
    db::{Activity, Store},
    wallet::PreparedPayment,
};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
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
            .claim_transfer(1, to, source, destination, 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();
        let prepared = store.activity_for_key("same").unwrap().unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([
                Ok(false),
                Err("node unavailable"),
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
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same-queued", None)
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
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same-queued", None)
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
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
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
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
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
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", expiry_height)
        .unwrap();
    let prepared = store.activity_for_key("same").unwrap().unwrap();
    (store, prepared)
}
