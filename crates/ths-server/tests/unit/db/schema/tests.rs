use super::*;
#[test]
fn restores_the_hidden_treasury_for_existing_stores() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM accounts WHERE id=?1", [TREASURY_ACCOUNT_ID])
        .unwrap();
    store.initialize().unwrap();
    assert_eq!(store.accounts().unwrap().len(), 6);
    assert_eq!(store.user_accounts().unwrap().len(), 5);
}
#[test]
fn existing_idempotency_tables_gain_a_memo_column() {
    let store = Store::open(":memory:").unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute_batch("CREATE TABLE idempotency (key TEXT PRIMARY KEY, activity_id TEXT NOT NULL)")
        .unwrap();
    store.initialize().unwrap();
    store
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
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT memo FROM idempotency WHERE key='memo-key'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "rent"
    );
}
