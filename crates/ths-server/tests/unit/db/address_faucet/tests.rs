use super::*;
use std::sync::Barrier;
#[test]
fn address_faucet_replay_survives_restart_and_rejects_conflicting_intents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let address = store.account(1).unwrap().unified_address;
    let first = store
        .claim_address_faucet(&address, 100_000_000, "address-key")
        .unwrap();
    let first = store
        .record_address_prepared(&first.id, "txid-one", b"signed bytes", 140)
        .unwrap();
    assert_eq!(first.status, "prepared");
    drop(store);

    let reopened = Store::open(&path).unwrap();
    reopened.initialize().unwrap();
    let replay = reopened
        .claim_address_faucet(&address, 100_000_000, "address-key")
        .unwrap();
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.txid, "txid-one");
    assert_eq!(
        reopened
            .prepared_transaction(&replay.id)
            .unwrap()
            .raw_transaction,
        b"signed bytes"
    );
    assert!(
        reopened
            .claim_address_faucet(&address, 2, "address-key")
            .is_err()
    );
    assert!(
        reopened
            .claim_address_faucet(
                &reopened.account(2).unwrap().unified_address,
                100_000_000,
                "address-key"
            )
            .is_err()
    );
    assert!(
        reopened
            .claim_faucet(1, "ironwood", 100_000_000, "address-key")
            .is_err()
    );
    assert!(
        reopened
            .claim_address_faucet(&address, 100_000_000, "fresh-key")
            .is_ok()
    );
    reopened
        .claim_faucet(1, "ironwood", 100_000_000, "account-key")
        .unwrap();
    assert!(
        reopened
            .claim_address_faucet(&address, 100_000_000, "account-key")
            .is_err()
    );
}
#[test]
fn concurrent_address_faucet_claims_share_one_operation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let claims: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.claim_address_faucet("same-address", 1, "same-operation")
            })
        })
        .collect();
    barrier.wait();
    let ids: Vec<_> = claims
        .into_iter()
        .map(|claim| claim.join().unwrap().unwrap().id)
        .collect();
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn address_faucet_errors_and_failures_never_overwrite_newer_state() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_address_faucet("tm-external", 100, "address-guarded")
        .unwrap();
    store
        .record_address_error(&claim.id, "preparing", "", "treasury is syncing")
        .unwrap();
    let prepared = store
        .record_address_prepared(&claim.id, "txid-1", b"raw", 140)
        .unwrap();
    assert_eq!(prepared.last_error, None);
    let stale = store
        .record_address_error(&claim.id, "preparing", "", "stale")
        .unwrap();
    assert_eq!(stale.last_error, None);
    assert!(store.fail_address(&claim.id, "stale").is_err());
    store
        .record_address_error(&claim.id, "prepared", "txid-1", "broadcast unavailable")
        .unwrap();
    assert_eq!(
        store
            .mark_address_broadcast(&claim.id, "txid-1")
            .unwrap()
            .last_error,
        None
    );
    store
        .record_address_error(&claim.id, "broadcast", "txid-1", "auto-mine failed")
        .unwrap();
    let confirmed = store.confirm_address(&claim.id, "txid-1", "block").unwrap();
    assert_eq!(confirmed.last_error, None);
    assert_eq!(
        store
            .record_address_error(&claim.id, "confirmed", "txid-1", "late")
            .unwrap()
            .last_error,
        None
    );
    assert!(store.fail_address(&claim.id, "late").is_err());

    let doomed = store
        .claim_address_faucet("tm-external", 100, "address-doomed")
        .unwrap();
    let failed = store
        .fail_address(&doomed.id, "treasury cannot pay")
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(
        store
            .fail_address(&doomed.id, "another reason")
            .unwrap()
            .last_error
            .as_deref(),
        Some("treasury cannot pay")
    );
    store
        .record_address_error(&doomed.id, "failed", "", "overwrite")
        .unwrap();
    assert!(
        store
            .record_address_prepared(&doomed.id, "late-txid", b"late", 140)
            .is_err()
    );
    store.discard_address_preparing(&doomed.id).unwrap();
    let replay = store
        .claim_address_faucet("tm-external", 100, "address-doomed")
        .unwrap();
    assert_eq!(replay, failed);
}
