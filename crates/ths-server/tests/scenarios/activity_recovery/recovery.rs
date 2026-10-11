use super::{
    API_READ_TIMEOUT, AccountBalance, Activity, Address, BlockHeight, ChainInfo, Client,
    Connection, Context, Duration, GenerateCounts, HeightCheckpoint, Instant, LiveScenario,
    LocalNetwork, OpenFlags, PREPARE_RESTART_IDEMPOTENCY_KEY, PREPARE_ROLLBACK_IDEMPOTENCY_KEY,
    PathBuf, PreparedPaymentRecord, QueuedBroadcastProxy, RECOVERY_IDEMPOTENCY_KEY,
    RECOVERY_POLL_INTERVAL, RecoveryFailureReporter, RecoveryPhase, RegtestStack, Result,
    SEND_TIMEOUT, Status, TransactionEvidence, TransparentAddress, assert_persisted_send_response,
    assert_read_only_persistence, assert_requested_payment_fields, constructed_transactions, json,
    lose_payment_response, one_matching_activity, params, pending_payment_snapshot,
    prepared_payment, prepared_payment_journal, preserve_scenario_failure, request_json, rpc,
    run_live_scenario, support,
};

pub(super) async fn prepare_rolls_back_constructed_transaction_when_journal_write_fails()
-> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        let transactions_before = constructed_transactions(fixture.data_dir())?;
        let journal_before = prepared_payment_journal(fixture.data_dir())?;
        let wallet_database = Connection::open(fixture.data_dir().join("wallet.db"))?;
        wallet_database.execute_batch(
            "CREATE TRIGGER fail_prepared_payment_journal
             BEFORE INSERT ON ext_tsz_prepared_payments
             BEGIN SELECT RAISE(ABORT, 'injected prepared-payment journal failure'); END;",
        )?;

        let response = Client::new()
            .post(format!("{}/api/v1/send", fixture.api_url()))
            .timeout(SEND_TIMEOUT)
            .json(&json!({
                "from_account": 1,
                "to_account": 2,
                "source_pool": "ironwood",
                "destination_pool": "ironwood",
                "amount_zatoshi": 1_000_000,
                "idempotency_key": PREPARE_ROLLBACK_IDEMPOTENCY_KEY,
            }))
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "journal failure did not fail payment preparation"
        );
        let error: serde_json::Value = response.json().await?;
        anyhow::ensure!(
            error["error"]["message"].as_str().is_some_and(
                |message| message.contains("injected prepared-payment journal failure")
            ),
            "payment preparation failed before reaching the injected journal write"
        );

        wallet_database.execute_batch("DROP TRIGGER fail_prepared_payment_journal")?;
        drop(wallet_database);
        fixture.restart_server().await?;
        anyhow::ensure!(
            constructed_transactions(fixture.data_dir())? == transactions_before,
            "constructed transaction survived the failed journal write"
        );
        anyhow::ensure!(
            prepared_payment_journal(fixture.data_dir())? == journal_before,
            "prepared-payment journal changed despite transaction rollback"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn retry_recovers_prepare_interrupted_before_store_recording() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture
            .start()
            .await
            .context("starting prepare-restart fixture")?;
        let client = Client::new();
        let accounts_before: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let balance_before = accounts_before
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .context("destination account is missing")?;
        let request = json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 1_000_000,
            "idempotency_key": PREPARE_RESTART_IDEMPOTENCY_KEY,
        });
        // seed only the pre-prepare claim. the request still performs all wallet
        // construction, serialization, and extension-journal persistence.
        let activity_id =
            seed_preparing_payment(fixture.data_dir(), PREPARE_RESTART_IDEMPOTENCY_KEY)?;
        let application_lock = Connection::open(fixture.data_dir().join("ths.db"))?;
        // the server's sqlite connection waits five seconds on this writer lock,
        // leaving time to observe the journal commit and interrupt the process.
        application_lock.execute_batch("BEGIN IMMEDIATE")?;
        let interrupted_request = tokio::spawn({
            let client = client.clone();
            let api_url = fixture.api_url().to_owned();
            let request = request.clone();
            async move {
                client
                    .post(format!("{api_url}/api/v1/send"))
                    .timeout(SEND_TIMEOUT)
                    .json(&request)
                    .send()
                    .await
            }
        });

        let original = wait_for_prepared_payment(fixture.data_dir(), &activity_id)
            .await
            .context("waiting for wallet preparation to commit")?;
        anyhow::ensure!(
            !original.raw_transaction.is_empty() && original.expiry_height > 0,
            "wallet journal did not retain complete prepared transaction data"
        );
        anyhow::ensure!(
            !interrupted_request.is_finished(),
            "send completed before interruption at the preparation boundary"
        );
        fixture
            .interrupt_server()
            .await
            .context("interrupting server after wallet preparation")?;
        application_lock.execute_batch("ROLLBACK")?;
        drop(application_lock);
        let interrupted = interrupted_request
            .await
            .context("interrupted send task did not finish")?;
        anyhow::ensure!(
            interrupted.is_err(),
            "send returned a response instead of being interrupted at the preparation boundary"
        );

        let application_database = Connection::open(fixture.data_dir().join("ths.db"))?;
        let (txid, status): (String, String) = application_database.query_row(
            "SELECT txid,status FROM activity WHERE id=?1",
            [&activity_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            txid.is_empty() && status == "preparing",
            "application database recorded preparation before interruption"
        );
        let application_prepared: u32 = application_database.query_row(
            "SELECT COUNT(*) FROM prepared_payments WHERE activity_id=?1",
            [&activity_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            application_prepared == 0,
            "application database retained prepared bytes before retry"
        );
        drop(application_database);

        fixture
            .restart_server()
            .await
            .context("restarting server after interrupted preparation")?;
        let recovered: Activity = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/send",
            Some(&request),
            SEND_TIMEOUT,
        )
        .await
        .context("retrying interrupted payment")?;
        anyhow::ensure!(
            recovered.id == activity_id
                && recovered.txid == original.txid
                && recovered.status == "confirmed",
            "retry did not confirm the original activity and transaction"
        );
        let application_database = Connection::open(fixture.data_dir().join("ths.db"))?;
        let recorded: (Vec<u8>, u64) = application_database.query_row(
            "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
            [&activity_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            recorded == (original.raw_transaction.clone(), original.expiry_height),
            "retry did not recover the original serialized bytes and expiry height"
        );

        let accounts_after: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let balance_after = accounts_after
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .context("destination account is missing after retry")?;
        anyhow::ensure!(
            balance_after.checked_sub(balance_before) == Some(1_000_000),
            "retry produced more or less than one payout"
        );
        let transaction: TransactionEvidence = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([original.txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            transaction
                .confirmations
                .is_some_and(|confirmations| confirmations > 0),
            "recovered transaction was not confirmed"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn broadcast_recovers_after_auto_mine_failure() -> Result<()> {
    run_live_scenario(LiveScenario::BroadcastRecovery).await
}

pub(super) async fn queued_retry_survives_restart(external: bool) -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let mut broadcast_proxy = None;
    let scenario = async {
        fixture.start().await?;
        broadcast_proxy = Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
        let proxy = broadcast_proxy.as_ref().unwrap();
        fixture.restart_with_lightwalletd(proxy.port()).await?;
        let client = Client::new();
        let key = "queued-lost-response-restart";
        let one = Some(BlockHeight::from_u32(1));
        let network = LocalNetwork { overwinter: one, sapling: one, blossom: one, heartwood: one, canopy: one, nu5: one, nu6: one, nu6_1: one, nu6_2: one, nu6_3: one, nu7: None };
        let address = Address::Transparent(TransparentAddress::PublicKeyHash([43; 20])).encode(&network);
        let (path, body) = if external {
            ("/api/v1/faucet/address", json!({"address": address, "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        } else {
            ("/api/v1/send", json!({"from_account": 1, "to_account": 2, "source_pool": "ironwood", "destination_pool": "ironwood", "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        };
        fixture.proxy().fail_next_generate()?;
        let lost = lose_payment_response(&client, fixture.api_url(), path, &body).await?;
        anyhow::ensure!(lost["status"] == "broadcast" && lost["block_hash"].is_null(), "first broadcast did not remain pending");
        let original = pending_payment_snapshot(&fixture, external, key)?;
        anyhow::ensure!(lost["txid"].as_str() == Some(original.txid.as_str()), "lost response and journals disagree");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.contains(&original.txid), "original broadcast did not reach the real node");

        fixture.proxy().hide_transaction(Some(original.txid.clone()))?;
        fixture.proxy().fail_next_generate()?;
        proxy.queue_next_broadcast();
        let retry: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(retry == lost, "queued retry changed the original pending response");
        proxy.assert_identical_retry()?;
        anyhow::ensure!(fixture.proxy().hidden_lookups()? > 0, "queued retry did not exercise missing transaction lookup");
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "queued retry changed journals or reserved inputs");

        fixture.restart_server().await?;
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "reopening stores changed the pending original payment");
        fixture.proxy().hide_transaction(None)?;
        let _: serde_json::Value = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
        let confirmed: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(confirmed["status"] == "confirmed" && confirmed["txid"].as_str() == Some(original.txid.as_str()), "retry did not confirm the original transaction");
        let evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 1])).await?;
        anyhow::ensure!(evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && evidence["blockhash"].is_string() && confirmed["block_hash"] == evidence["blockhash"], "confirmation lacks canonical transaction evidence");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblockheader", json!([evidence["blockhash"]])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(evidence["blockhash"].as_str() == Some(canonical.as_str()), "confirmed transaction is not on the canonical chain");
        let before: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let replay: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(replay == confirmed, "confirmed replay changed the payment");
        proxy.assert_identical_retry()?;
        let after: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(before.blocks == after.blocks && before.bestblockhash == after.bestblockhash, "confirmed replay mined another payment");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.is_empty(), "confirmed replay broadcast another transaction");
        if external {
            let balance: serde_json::Value = rpc(&client, fixture.node_url(), "getaddressbalance", json!([{"addresses": [address]}])).await?;
            anyhow::ensure!(balance["balance"] == 1_000_000, "external destination did not receive exactly one payout");
        } else {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let accounts: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
                if accounts.iter().any(|a| a.id == 2 && a.ironwood_zatoshi == 1_000_000) { break; }
                anyhow::ensure!(Instant::now() < deadline, "Send destination did not receive exactly one payout");
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
            }
        }
        let db = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let count: u64 = db.query_row(if external { "SELECT COUNT(*) FROM address_faucets WHERE key=?1" } else { "SELECT COUNT(*) FROM idempotency WHERE key=?1" }, [key], |row| row.get(0))?;
        anyhow::ensure!(count == 1, "retry created duplicate operations");
        fixture.assert_running().await?;
        Ok(())
    }.await;
    let cleanup = fixture.shutdown().await;
    let proxy_cleanup = if let Some(proxy) = broadcast_proxy.as_mut() {
        proxy.shutdown().await
    } else {
        Ok(())
    };
    preserve_scenario_failure(scenario, preserve_scenario_failure(cleanup, proxy_cleanup))
}

fn seed_preparing_payment(path: &std::path::Path, idempotency_key: &str) -> Result<String> {
    let activity_id = uuid::Uuid::new_v4().to_string();
    let mut database = Connection::open(path.join("ths.db"))?;
    let transaction = database.transaction()?;
    transaction.execute(
        "INSERT INTO activity(
           id,kind,from_account,to_account,source_pool,destination_pool,
           amount_zatoshi,txid,status
         ) VALUES(?1,'send',1,2,'ironwood','ironwood',1000000,'','preparing')",
        [&activity_id],
    )?;
    transaction.execute(
        "INSERT INTO idempotency(key,activity_id,memo) VALUES(?1,?2,NULL)",
        params![idempotency_key, activity_id],
    )?;
    transaction.commit()?;
    Ok(activity_id)
}

async fn wait_for_prepared_payment(
    path: &std::path::Path,
    activity_id: &str,
) -> Result<PreparedPaymentRecord> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    loop {
        if let Some(prepared) = prepared_payment(path, activity_id)? {
            return Ok(prepared);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet preparation did not commit before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) async fn exercise_recovery(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
) -> Result<()> {
    let client = Client::new();
    reporter.phase(RecoveryPhase::Broadcast);

    let initial_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    let before_auto_mine: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::BeforeAutoMine, before_auto_mine.blocks);
    let proxy_before = fixture.proxy().counts();

    reporter.phase(RecoveryPhase::AutoMine);
    fixture.proxy().fail_next_generate()?;
    let sent: Activity = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/send",
        Some(&json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 1000000,
            "idempotency_key": RECOVERY_IDEMPOTENCY_KEY,
        })),
        SEND_TIMEOUT,
    )
    .await?;
    assert_requested_payment_fields(&sent)?;
    anyhow::ensure!(
        sent.status == "broadcast",
        "send returned an activity that was not broadcast"
    );
    anyhow::ensure!(
        sent.block_hash.is_none(),
        "send returned an activity with an unexpected block hash"
    );
    reporter.record_activity(&sent.id, &sent.status, &sent.txid);

    let proxy_after_auto_mine = fixture.proxy().counts();
    anyhow::ensure!(
        proxy_after_auto_mine
            == GenerateCounts {
                rejected: proxy_before.rejected + 1,
                forwarded: proxy_before.forwarded,
            },
        "the injected auto-mine fault did not reject exactly one generate request"
    );
    let after_auto_mine: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::AfterAutoMine, after_auto_mine.blocks);
    anyhow::ensure!(
        after_auto_mine.blocks == before_auto_mine.blocks,
        "auto-mine failure changed the node height"
    );
    let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
    anyhow::ensure!(
        mempool.iter().any(|txid| txid == &sent.txid),
        "broadcast transaction was not present in the node mempool"
    );

    let persisted_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        persisted_activities.len() == initial_activities.len() + 1,
        "broadcast did not add exactly one activity row"
    );
    let persisted_broadcast = one_matching_activity(&persisted_activities, &sent)?;
    assert_persisted_send_response(&sent, &persisted_broadcast)?;

    reporter.phase(RecoveryPhase::DirectMine);
    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let inclusion_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::Inclusion, inclusion_tip.blocks);
    let inclusion: TransactionEvidence = rpc(
        &client,
        fixture.node_url(),
        "getrawtransaction",
        json!([sent.txid, 1]),
    )
    .await?;
    anyhow::ensure!(
        inclusion
            .confirmations
            .is_some_and(|confirmations| confirmations >= 1),
        "directly mined transaction does not have a confirmation"
    );
    let expected_block_hash = inclusion
        .blockhash
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| anyhow::anyhow!("included transaction did not report a block hash"))?;

    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let later_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::Tip, later_tip.blocks);
    anyhow::ensure!(
        later_tip.bestblockhash != expected_block_hash,
        "the later chain tip did not differ from the transaction inclusion block"
    );

    reporter.phase(RecoveryPhase::Recovery);
    let recovery_deadline = RegtestStack::recovery_deadline();
    let confirmed = loop {
        let status = match fixture
            .recovery_read::<Status>(recovery_deadline, "/api/v1/status")
            .await
        {
            Ok(status) => status,
            Err(error) if support::is_retryable_read_transport(&error) => {
                wait_for_recovery_poll(recovery_deadline).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let activities = match fixture
            .recovery_read::<Vec<Activity>>(recovery_deadline, "/api/v1/activity?limit=100")
            .await
        {
            Ok(activities) => activities,
            Err(error) if support::is_retryable_read_transport(&error) => {
                wait_for_recovery_poll(recovery_deadline).await?;
                continue;
            }
            Err(error) => return Err(error),
        };

        if let Some(height) = status.wallet_sync.fully_scanned_height {
            reporter.record_height(HeightCheckpoint::Scanned, height);
        }
        let activity = one_matching_activity(&activities, &persisted_broadcast)?;
        if status.wallet_sync.state == "ready"
            && status
                .wallet_sync
                .fully_scanned_height
                .is_some_and(|height| height >= later_tip.blocks)
            && activity.status == "confirmed"
        {
            break activity;
        }
        wait_for_recovery_poll(recovery_deadline).await?;
    };

    reporter.record_activity(&confirmed.id, &confirmed.status, &confirmed.txid);
    let mut expected_confirmed = persisted_broadcast.clone();
    expected_confirmed.status = "confirmed".to_owned();
    expected_confirmed.block_hash = Some(expected_block_hash.clone());
    anyhow::ensure!(
        confirmed == expected_confirmed,
        "activity changed beyond its confirmation status and inclusion block hash"
    );
    assert_requested_payment_fields(&confirmed)?;
    anyhow::ensure!(
        confirmed.status == "confirmed",
        "recovered activity was not confirmed"
    );
    anyhow::ensure!(
        fixture.proxy().counts() == proxy_after_auto_mine,
        "recovery polling changed proxy mining counters"
    );
    let final_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        final_activities.len() == initial_activities.len() + 1,
        "recovery changed the activity count"
    );
    let final_activity = one_matching_activity(&final_activities, &persisted_broadcast)?;
    anyhow::ensure!(
        final_activity == confirmed,
        "final activity read did not preserve the recovered row"
    );

    assert_read_only_persistence(
        fixture.data_dir().join("ths.db"),
        &persisted_broadcast,
        &expected_block_hash,
    )?;
    Ok(())
}

async fn wait_for_recovery_poll(deadline: Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "background activity recovery exceeded its 120-second deadline"
    );
    tokio::time::sleep(remaining.min(RECOVERY_POLL_INTERVAL)).await;
    Ok(())
}
