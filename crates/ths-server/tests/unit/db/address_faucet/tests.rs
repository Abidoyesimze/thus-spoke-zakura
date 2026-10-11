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
