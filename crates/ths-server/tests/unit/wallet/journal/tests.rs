use super::*;

#[tokio::test]
async fn prepared_payment_journal_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let seed = hex::encode([7_u8; 64]);
    let wallet = RealWallet::open(dir.path(), &seed).unwrap();
    drop(wallet);

    let db = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    db.execute(
        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
        rusqlite::params!["activity-1", "txid-1", b"signed transaction", 140_u64],
    )
    .unwrap();
    drop(db);

    let wallet = RealWallet::open(dir.path(), &seed).unwrap();
    assert!(wallet.has_prepared("activity-1").await.unwrap());
    assert!(!wallet.has_prepared("missing").await.unwrap());
    let journaled = wallet.prepared("activity-1").await.unwrap().unwrap();
    assert_eq!(journaled.txid, "txid-1");
    assert_eq!(journaled.raw_transaction, b"signed transaction");
    assert_eq!(journaled.expiry_height, 140);
    assert!(wallet.prepared("missing").await.unwrap().is_none());
    let recovered = wallet
        .prepare(
            Some("activity-1"),
            "invalid seed",
            0,
            "invalid pool",
            "invalid address",
            0,
            None,
            Ok,
        )
        .await
        .unwrap();
    assert_eq!(recovered.txid, "txid-1");
    assert_eq!(recovered.raw_transaction, b"signed transaction");
    assert_eq!(recovered.expiry_height, 140);
}
