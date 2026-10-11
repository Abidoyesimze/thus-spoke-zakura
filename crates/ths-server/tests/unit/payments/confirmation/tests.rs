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
