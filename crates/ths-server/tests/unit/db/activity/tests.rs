use super::*;
use std::sync::Barrier;
#[test]
fn records_real_transfer_without_mutating_balances() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let activity = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "send", None)
        .unwrap();
    let activity = store
        .record_prepared(&activity.id, "real-txid", b"raw transaction", 140)
        .unwrap();
    let activity = store.mark_broadcast(&activity.id, &activity.txid).unwrap();
    assert_eq!(activity.txid, "real-txid");
    assert_eq!(store.account(1).unwrap().transparent_zatoshi, 0);
}

#[test]
fn unconfirmed_activities_skip_unprepared_and_confirmed_rows() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(1, 2, "ironwood", "ironwood", 11_000, "preparing-key", None)
        .unwrap();
    let pending = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "pending-key", None)
        .unwrap();
    let pending = store
        .record_prepared(&pending.id, "txid-pending", b"pending", 140)
        .unwrap();
    let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();
    let mined = store
        .claim_transfer(1, 3, "ironwood", "ironwood", 13_000, "mined-key", None)
        .unwrap();
    let mined = store
        .record_prepared(&mined.id, "txid-mined", b"mined", 140)
        .unwrap();
    let mined = store.mark_broadcast(&mined.id, &mined.txid).unwrap();
    store
        .confirm(&mined.id, &mined.txid, &"c".repeat(64))
        .unwrap();

    let open = store.unconfirmed_activities().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, pending.id);
    assert_eq!(open[0].status, "broadcast");
    assert_eq!(open[0].txid, "txid-pending");
}

#[test]
fn confirm_rejects_an_empty_block_hash() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let pending = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "empty-hash", None)
        .unwrap();
    let pending = store
        .record_prepared(&pending.id, "txid-empty", b"raw", 140)
        .unwrap();
    let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();

    assert!(store.confirm(&pending.id, &pending.txid, "").is_err());
    let again = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "empty-hash", None)
        .unwrap();
    assert_eq!(again.id, pending.id);
    assert_eq!(again.status, "broadcast");
    assert_eq!(again.block_hash, None);
}

#[test]
fn rejects_reusing_a_key_for_a_different_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();

    let error = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 13_000, "same", None)
        .unwrap_err();

    assert!(error.to_string().contains("different payment"));
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());

    let error = store
        .claim_faucet(2, "ironwood", 12_000, "same")
        .unwrap_err();
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn reusing_a_key_conflicts_on_every_payment_field() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "send", None)
        .unwrap();
    store.claim_faucet(2, "ironwood", 12_000, "faucet").unwrap();

    for (from, to, source, destination, amount) in [
        (3, 2, "ironwood", "ironwood", 12_000),
        (1, 3, "ironwood", "ironwood", 12_000),
        (1, 2, "transparent", "ironwood", 12_000),
        (1, 2, "ironwood", "transparent", 12_000),
        (1, 2, "ironwood", "ironwood", 13_000),
    ] {
        let error = store
            .claim_transfer(from, to, source, destination, amount, "send", None)
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
    for (to, pool, amount) in [
        (3, "ironwood", 12_000),
        (2, "transparent", 12_000),
        (2, "ironwood", 13_000),
    ] {
        let error = store.claim_faucet(to, pool, amount, "faucet").unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
    let error = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "faucet", None)
        .unwrap_err();
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn send_and_faucet_validate_their_own_requests() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();

    for (from, to, source, destination, amount) in [
        (9, 2, "ironwood", "ironwood", 12_000),
        (1, 9, "ironwood", "ironwood", 12_000),
        (1, 2, "sapling", "ironwood", 12_000),
        (1, 2, "ironwood", "sapling", 12_000),
        (1, 2, "ironwood", "ironwood", 0),
    ] {
        assert!(
            store
                .claim_transfer(from, to, source, destination, amount, "send", None)
                .is_err()
        );
    }
    for (to, pool, amount) in [
        (9, "ironwood", 12_000),
        (2, "sapling", 12_000),
        (2, "ironwood", 0),
    ] {
        assert!(store.claim_faucet(to, pool, amount, "faucet").is_err());
    }
    assert!(store.activity_for_key("send").unwrap().is_none());
    assert!(store.activity_for_key("faucet").unwrap().is_none());

    let faucet = store
        .claim_faucet(2, "transparent", 12_000, "faucet")
        .unwrap();
    assert_eq!(faucet.kind, "faucet");
    assert_eq!(faucet.from_account, None);
    assert_eq!(faucet.source_pool, "ironwood");
    assert_eq!(faucet.destination_pool, "transparent");
    assert_eq!(faucet.status, "preparing");
    assert!(faucet.txid.is_empty());
}

#[test]
fn memo_is_part_of_the_claimed_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let first = store
        .claim_transfer(
            1,
            2,
            "ironwood",
            "ironwood",
            12_000,
            "memo-key",
            Some("rent"),
        )
        .unwrap();
    assert_eq!(
        store
            .claim_transfer(
                1,
                2,
                "ironwood",
                "ironwood",
                12_000,
                "memo-key",
                Some("rent")
            )
            .unwrap()
            .id,
        first.id
    );
    for memo in [None, Some(""), Some("gift")] {
        let error = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "memo-key", memo)
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
}

#[test]
fn concurrent_claims_create_one_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let claims = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(claims[0].id, claims[1].id);
    assert_eq!(claims[0].status, "preparing");
    assert!(store.activities(10).unwrap().is_empty());
}

#[test]
fn discarding_a_failed_prepare_removes_its_claim() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let failed = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();

    store.discard_preparing(&failed.id).unwrap();

    assert!(store.activity_for_key("same").unwrap().is_none());
    let retry = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    assert_ne!(retry.id, failed.id);
}

#[test]
fn prepared_payment_can_be_reset_after_expiry() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    store
        .record_prepared(&claim.id, "expired-txid", b"signed transaction", 140)
        .unwrap();

    let retry = store.reset_for_retry(&claim.id, "expired-txid").unwrap();

    assert_eq!(retry.status, "preparing");
    assert!(retry.txid.is_empty());
    assert!(store.prepared_transaction(&claim.id).is_err());
    assert!(store.activities(10).unwrap().is_empty());
}

#[test]
fn recording_the_same_prepared_transaction_converges() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    let first = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();

    let second = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();

    assert_eq!(second.id, first.id);
    assert_eq!(second.txid, first.txid);
    assert_eq!(second.status, "prepared");
}

#[test]
fn recording_prepared_bytes_backfills_a_legacy_broadcast() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    let prepared = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();
    let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM prepared_payments", [])
        .unwrap();

    store
        .record_prepared(
            &broadcast.id,
            &broadcast.txid,
            b"recovered transaction",
            140,
        )
        .unwrap();

    assert_eq!(
        store
            .prepared_transaction(&broadcast.id)
            .unwrap()
            .raw_transaction,
        b"recovered transaction"
    );
}

#[test]
fn stale_retry_does_not_reset_a_replacement_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    store
        .record_prepared(&claim.id, "old-txid", b"old transaction", 140)
        .unwrap();
    store.reset_for_retry(&claim.id, "old-txid").unwrap();
    store
        .record_prepared(&claim.id, "new-txid", b"new transaction", 180)
        .unwrap();

    let current = store.reset_for_retry(&claim.id, "old-txid").unwrap();

    assert_eq!(current.txid, "new-txid");
    assert_eq!(current.status, "prepared");
}

#[test]
fn broadcast_reconciliation_preserves_a_concurrent_confirmation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    store
        .record_prepared(&claim.id, "real-txid", b"transaction", 140)
        .unwrap();
    store
        .confirm(&claim.id, "real-txid", "mined-block")
        .unwrap();

    let current = store.mark_broadcast(&claim.id, "real-txid").unwrap();

    assert_eq!(current.status, "confirmed");
    assert_eq!(current.block_hash.as_deref(), Some("mined-block"));
}

#[test]
fn prepared_transaction_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();
    let id = claim.id.clone();
    store
        .record_prepared(&id, "real-txid", b"raw transaction", 140)
        .unwrap();
    drop(store);

    let reopened = Store::open(path).unwrap();
    reopened.initialize().unwrap();
    let recovered = reopened
        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
        .unwrap();

    assert_eq!(recovered.txid, "real-txid");
    assert_eq!(recovered.status, "prepared");
    let prepared = reopened.prepared_transaction(&id).unwrap();
    assert_eq!(prepared.raw_transaction, b"raw transaction");
    assert_eq!(prepared.expiry_height, 140);
}
