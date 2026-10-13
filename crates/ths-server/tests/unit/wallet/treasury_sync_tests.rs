use super::*;
use crate::db::{Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT};

#[tokio::test]
async fn wallet_migration_installs_only_the_treasury_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let seed = SecretVec::new(hex::decode(store.seed().unwrap()).unwrap());
    let mut db = WalletDb::for_path(
        dir.path().join("wallet.db"),
        regtest_network(),
        SystemClock,
        UnwrapErr(SysRng),
    )
    .unwrap();
    zcash_client_sqlite::wallet::init::init_wallet_db(
        &mut db,
        Some(SecretVec::new(seed.expose_secret().clone())),
    )
    .unwrap();
    let birthday =
        AccountBirthday::from_parts(ChainState::empty(1.into(), BlockHash([0; 32])), None);
    for id in 1..=USER_ACCOUNT_COUNT {
        db.create_account(&format!("Account {id}"), &seed, &birthday, None)
            .unwrap();
    }
    drop(db);

    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet
        .with_db(|db| {
            assert_eq!(db.get_account_ids()?.len(), 6);
            let (cursor, prepared): (u32, u32) =
                db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                    Ok((
                        ext.query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_treasury_cursor'",
                            [],
                            |row| row.get(0),
                        )?,
                        ext.query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_prepared_transactions'",
                            [],
                            |row| row.get(0),
                        )?,
                    ))
                })?;
            assert_eq!((cursor, prepared), (1, 0));
            Ok(())
        })
        .await
        .unwrap();
    drop(wallet);
    let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    assert!(reopened.treasury_cursor().await.unwrap().is_none());
}

#[tokio::test]
async fn routine_transparent_queries_exclude_the_treasury() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();

    let queries = wallet.public_transparent_queries().await.unwrap();
    assert_eq!(queries.len(), usize::from(USER_ACCOUNT_COUNT));
    for id in 1..=USER_ACCOUNT_COUNT {
        assert!(queries.iter().any(|query| {
            query
                .addresses
                .contains(&store.account(id).unwrap().transparent_address)
        }));
    }
    assert!(!queries.iter().any(|query| {
        query.addresses.contains(
            &store
                .account(TREASURY_ACCOUNT_ID)
                .unwrap()
                .transparent_address,
        )
    }));
}

#[tokio::test]
async fn height_wait_deadline_includes_stalled_lightwalletd_calls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet.lightwalletd = format!("http://{}", listener.local_addr().unwrap());

    let result = tokio::time::timeout(
        Duration::from_millis(250),
        wallet.wait_for_height(1, Duration::from_millis(50)),
    )
    .await;

    assert!(result.is_ok(), "height wait ignored its deadline");
    let error = result.unwrap().unwrap_err();
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    );
}

#[tokio::test]
async fn database_work_does_not_block_the_async_executor() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    let work = tokio::spawn(async move {
        wallet
            .with_db(move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    work.await.unwrap().unwrap();
    assert!(
        elapsed < Duration::from_millis(750),
        "database work blocked the async executor for {elapsed:?}"
    );
}
