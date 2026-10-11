use super::*;
#[test]
fn creates_user_accounts_and_hidden_treasury() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    assert_eq!(store.accounts().unwrap().len(), 6);
    assert_eq!(store.user_accounts().unwrap().len(), 5);
    assert_eq!(
        store.account(TREASURY_ACCOUNT_ID).unwrap().name,
        "Account 6"
    );
    assert!(
        store
            .account(1)
            .unwrap()
            .transparent_address
            .starts_with("tm")
    );
    let first = store
        .claim_faucet(2, "ironwood", ZATOSHIS_PER_ZEC, "same")
        .unwrap();
    let second = store
        .claim_faucet(2, "ironwood", ZATOSHIS_PER_ZEC, "same")
        .unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(store.account(2).unwrap().ironwood_zatoshi, 0);
}
#[test]
fn fresh_stores_derive_the_same_development_accounts() {
    let first = Store::open(":memory:").unwrap();
    first.initialize().unwrap();
    let second = Store::open(":memory:").unwrap();
    second.initialize().unwrap();
    assert_eq!(first.accounts().unwrap(), second.accounts().unwrap());
    assert_eq!(
        first.mnemonic().unwrap(),
        format!("{}art", "abandon ".repeat(23))
    );
    assert_eq!(
        first.account(1).unwrap().transparent_address,
        "tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd"
    );
}
#[test]
fn existing_store_exports_user_viewing_keys_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let seed = [7u8; 32];
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE accounts (
                 id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                 unified_address TEXT NOT NULL, transparent_address TEXT NOT NULL,
                 transparent_zatoshi INTEGER NOT NULL DEFAULT 0,
                 ironwood_zatoshi INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        db.execute(
            "INSERT INTO metadata(key,value) VALUES('seed',?1)",
            [hex::encode(seed)],
        )
        .unwrap();
        for id in 1..=TREASURY_ACCOUNT_ID {
            let (ua, taddr) = derived_addresses(&seed, id).unwrap();
            db.execute(
                "INSERT INTO accounts
                 (id,name,unified_address,transparent_address,transparent_zatoshi,ironwood_zatoshi)
                 VALUES(?1,?2,?3,?4,123,456)",
                params![id, format!("Account {id}"), ua, taddr],
            )
            .unwrap();
        }
    }

    let store = Store::open(&path).unwrap();
    let users = store.user_accounts().unwrap();
    assert_eq!(
        users.iter().map(|a| a.id).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    let network = regtest_network();
    let mut distinct = std::collections::HashSet::new();
    for account in &users {
        let json = serde_json::to_value(account).unwrap();
        let encoded = json["unified_full_viewing_key"]
            .as_str()
            .expect("user projection must contain a viewing key");
        let viewing = zcash_keys::keys::UnifiedFullViewingKey::decode(&network, encoded)
            .unwrap_or_else(|_| panic!("user viewing key must decode for regtest"));
        let (ua, _) = viewing
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .unwrap();
        assert!(ua.encode(&network) == account.unified_address);
        let transparent = ua.transparent().cloned().expect("transparent receiver");
        assert!(Address::Transparent(transparent).encode(&network) == account.transparent_address);
        let index = zip32::AccountId::try_from(u32::from(account.id - 1)).unwrap();
        let expected = UnifiedSpendingKey::from_seed(&network, &seed, index)
            .unwrap()
            .to_unified_full_viewing_key()
            .encode(&network);
        assert!(encoded == expected);
        assert!(distinct.insert(encoded.to_owned()));
        assert_eq!(account.transparent_zatoshi, 123);
        assert_eq!(account.ironwood_zatoshi, 456);
    }

    let all = store.accounts().unwrap();
    assert_eq!(all.len(), 6);
    let treasury = all.iter().find(|a| a.id == TREASURY_ACCOUNT_ID).unwrap();
    assert!(
        serde_json::to_value(treasury)
            .unwrap()
            .get("unified_full_viewing_key")
            .is_none()
    );
    assert!(
        serde_json::to_value(store.account(TREASURY_ACCOUNT_ID).unwrap())
            .unwrap()
            .get("unified_full_viewing_key")
            .is_none()
    );
    store.initialize().unwrap();
    assert!(users == store.user_accounts().unwrap());
    {
        let db = store.0.lock().unwrap();
        let columns = db
            .prepare("PRAGMA table_info(accounts)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            columns,
            [
                "id",
                "name",
                "unified_address",
                "transparent_address",
                "transparent_zatoshi",
                "ironwood_zatoshi",
            ]
        );
        let metadata_keys = db
            .prepare("SELECT key FROM metadata ORDER BY key")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(metadata_keys, ["seed"]);
    }
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert!(users == reopened.user_accounts().unwrap());
}
