use super::*;

fn subtree_end_heights(path: &Path) -> (Option<u32>, Option<u32>, Option<u32>) {
    let db = rusqlite::Connection::open(path.join("wallet.db")).unwrap();
    let height = |table: &str| {
        db.query_row(
            &format!("SELECT subtree_end_height FROM {table} WHERE shard_index = 0"),
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    };
    (
        height("sapling_tree_shards"),
        height("orchard_tree_shards"),
        height("ironwood_tree_shards"),
    )
}

#[tokio::test]
async fn subtree_roots_reject_malformed_data_and_route_by_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = RealWallet::open(dir.path(), &hex::encode([7_u8; 64])).unwrap();

    for protocol in [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ] {
        wallet
            .put_subtree_roots(
                protocol,
                vec![
                    SubtreeRootData {
                        completing_height: 100,
                        root_hash: vec![0; 32],
                    },
                    SubtreeRootData {
                        completing_height: 101,
                        root_hash: vec![0; 31],
                    },
                ],
            )
            .await
            .unwrap_err();
    }
    assert_eq!(subtree_end_heights(dir.path()), (None, None, None));

    for (protocol, completing_height) in [
        (ShieldedProtocol::Sapling, 100),
        (ShieldedProtocol::Orchard, 101),
        (ShieldedProtocol::Ironwood, 102),
    ] {
        wallet
            .put_subtree_roots(
                protocol,
                vec![SubtreeRootData {
                    completing_height,
                    root_hash: vec![0; 32],
                }],
            )
            .await
            .unwrap();
    }
    assert_eq!(
        subtree_end_heights(dir.path()),
        (Some(100), Some(101), Some(102))
    );
}

struct TestWallet {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    wallet: RealWallet,
    seed: String,
    destination: String,
}

fn test_wallet() -> TestWallet {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::db::Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let seed = store.seed().unwrap();
    TestWallet {
        path: dir.path().to_owned(),
        wallet: RealWallet::open(dir.path(), &seed).unwrap(),
        destination: store.account(2).unwrap().transparent_address,
        seed,
        _dir: dir,
    }
}

fn delay_wallet_queries(path: &std::path::Path, hold: Duration) -> std::thread::JoinHandle<()> {
    let blocker = rusqlite::Connection::open(path.join("wallet.db")).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(hold);
        blocker.execute_batch("COMMIT").unwrap();
    })
}

async fn assert_runs_off_the_executor<T, F>(call: impl FnOnce(TestWallet) -> F)
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T>> + Send + 'static,
{
    let test = test_wallet();
    let release = delay_wallet_queries(&test.path, Duration::from_millis(500));
    let work = tokio::spawn(call(test));
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let elapsed = started.elapsed();
    let still_waiting = !work.is_finished();
    release.join().unwrap();
    let _ = work.await.unwrap();
    assert!(
        elapsed < Duration::from_millis(250),
        "wallet database work blocked the async executor for {elapsed:?}"
    );
    assert!(still_waiting, "the delayed wallet query finished early");
}

#[tokio::test]
async fn operation_cancelled_while_waiting_for_the_wallet_never_runs() {
    let test = test_wallet();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let wallet = test.wallet.clone();
        tokio::spawn(async move {
            wallet
                .with_db(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        })
    };
    entered_rx.await.unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiting = {
        let wallet = test.wallet.clone();
        let ran = Arc::clone(&ran);
        tokio::spawn(async move {
            wallet
                .with_db(move |_| {
                    ran.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
    test.wallet.with_db(|_| Ok(())).await.unwrap();
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "a caller cancelled before it held the wallet still ran its operation"
    );
}

#[tokio::test]
async fn started_operation_finishes_after_its_caller_is_cancelled() {
    let test = test_wallet();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let started = {
        let wallet = test.wallet.clone();
        tokio::spawn(async move {
            wallet
                .with_db(move |db| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                        ext.execute(
                            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('activity-1','txid-1',x'00',0)",
                            [],
                        )?;
                        Ok(())
                    })
                })
                .await
        })
    };
    entered_rx.await.unwrap();
    started.abort();
    assert!(started.await.unwrap_err().is_cancelled());

    let check = {
        let wallet = test.wallet.clone();
        tokio::spawn(async move { wallet.has_prepared("activity-1").await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !check.is_finished(),
        "another caller used the wallet while a cancelled operation still held it"
    );
    release_tx.send(()).unwrap();
    assert!(check.await.unwrap().unwrap());

    let retried = test
        .wallet
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
    assert_eq!(retried.txid, "txid-1");
}

#[tokio::test]
async fn prepared_payment_is_recorded_after_its_caller_is_dropped() {
    let test = test_wallet();
    rusqlite::Connection::open(test.path.join("wallet.db"))
        .unwrap()
        .execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('activity-1','txid-1',x'00',0)",
            [],
        )
        .unwrap();
    let recorded = Arc::new(std::sync::Mutex::new(None));
    let release = delay_wallet_queries(&test.path, Duration::from_millis(300));
    let caller = {
        let wallet = test.wallet.clone();
        let recorded = Arc::clone(&recorded);
        tokio::spawn(async move {
            wallet
                .prepare(
                    Some("activity-1"),
                    "invalid seed",
                    0,
                    "invalid pool",
                    "invalid address",
                    0,
                    None,
                    move |prepared| {
                        *recorded.lock().unwrap() = Some(prepared.txid);
                        Ok(())
                    },
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.join().unwrap();
    test.wallet.with_db(|_| Ok(())).await.unwrap();
    assert_eq!(
        recorded.lock().unwrap().as_deref(),
        Some("txid-1"),
        "a journaled payment was not recorded after its caller was dropped"
    );
}

#[tokio::test]
async fn payment_errors_stay_typed_through_the_wallet_worker() {
    let test = test_wallet();
    let memo_to_transparent = test
        .wallet
        .prepare(
            None,
            &test.seed,
            1,
            "ironwood",
            &test.destination,
            10_000,
            Some(MemoBytes::empty()),
            Ok,
        )
        .await
        .err()
        .expect("a memo to a transparent address must be rejected");
    assert!(matches!(
        memo_to_transparent.downcast_ref(),
        Some(PaymentError::TransparentMemo)
    ));
    let unscanned = test
        .wallet
        .prepare(
            None,
            &test.seed,
            1,
            "ironwood",
            &test.destination,
            10_000,
            None,
            Ok,
        )
        .await
        .err()
        .expect("an unscanned wallet must not build a payment");
    assert!(matches!(
        unscanned.downcast_ref(),
        Some(PaymentError::ScanRequired)
    ));
}

#[tokio::test]
async fn delayed_prepare_does_not_block_the_async_executor() {
    assert_runs_off_the_executor(|test| async move {
        test.wallet
            .prepare(
                Some("activity-1"),
                &test.seed,
                1,
                "ironwood",
                &test.destination,
                10_000,
                None,
                Ok,
            )
            .await
            .map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn delayed_send_quote_does_not_block_the_async_executor() {
    assert_runs_off_the_executor(|test| async move {
        test.wallet
            .send_quote(1, "ironwood", &test.destination)
            .await
            .map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn delayed_has_prepared_does_not_block_the_async_executor() {
    assert_runs_off_the_executor(|test| async move {
        test.wallet.has_prepared("activity-1").await.map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn delayed_recover_prepared_does_not_block_the_async_executor() {
    assert_runs_off_the_executor(|test| async move {
        test.wallet
            .recover_prepared(&"00".repeat(32))
            .await
            .map(|_| ())
    })
    .await;
}
