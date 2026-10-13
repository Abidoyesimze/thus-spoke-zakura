use super::*;
use crate::wallet::TREASURY_SHIELDING_JOURNAL_ID;
use crate::wallet::tests::test_wallet;

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
            None,
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

#[tokio::test]
async fn shielding_retirement_cannot_remove_a_newer_journal_or_user_payment() {
    let test = test_wallet();
    test.wallet
            .with_db(|db| {
                db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                    ext.execute(
                        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'shielding-newer',x'040506',180)",
                        [TREASURY_SHIELDING_JOURNAL_ID],
                    )?;
                    ext.execute(
                        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('user-payment','shielding-newer',x'070809',180)",
                        [],
                    )?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    let error = test
        .wallet
        .retire_shielding("shielding-expired")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("journal changed before retirement"),
        "{error:#}"
    );
    let retained = test.wallet.prepared_shielding().await.unwrap().unwrap();
    assert_eq!(retained.txid, "shielding-newer");
    assert_eq!(retained.raw_transaction, [4, 5, 6]);
    assert_eq!(retained.expiry_height, 180);
    test.wallet
        .retire_shielding("shielding-newer")
        .await
        .unwrap();
    assert!(test.wallet.prepared_shielding().await.unwrap().is_none());
    let user = test.wallet.prepared("user-payment").await.unwrap().unwrap();
    assert_eq!(user.txid, "shielding-newer");
    assert_eq!(user.raw_transaction, [7, 8, 9]);
}
