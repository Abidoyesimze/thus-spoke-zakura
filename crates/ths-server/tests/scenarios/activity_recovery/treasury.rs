use super::{
    API_READ_TIMEOUT, Activity, ChainInfo, Client, Duration, HeightCheckpoint, Instant,
    LARGE_MINE_TIMEOUT, LiveScenario, RECOVERY_POLL_INTERVAL, RecoveryFailureReporter,
    RecoveryPhase, RegtestStack, Result, SEND_TIMEOUT, Status, json, request_json, rpc,
    rpc_with_timeout, run_live_scenario, support,
};

pub(super) async fn large_reward_history_keeps_treasury_faucet_responsive() -> Result<()> {
    run_live_scenario(LiveScenario::TreasurySync).await
}

pub(super) async fn exercise_treasury_sync(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
) -> Result<()> {
    let client = Client::new();
    let before: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;

    reporter.phase(RecoveryPhase::DirectMine);
    let _: Vec<String> = rpc_with_timeout(
        &client,
        fixture.node_url(),
        "generate",
        json!([10_000]),
        LARGE_MINE_TIMEOUT,
    )
    .await?;
    let bulk_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    anyhow::ensure!(
        bulk_tip.blocks == before.blocks + 10_000,
        "direct mining did not create the expected reward history"
    );
    reporter.record_height(HeightCheckpoint::Tip, bulk_tip.blocks);

    reporter.phase(RecoveryPhase::Recovery);
    wait_for_wallet_height(fixture, reporter, bulk_tip.blocks, LARGE_MINE_TIMEOUT).await?;

    // An additional block tests refresh cost after the large reward history exists.
    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let incremental_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    anyhow::ensure!(
        incremental_tip.blocks == bulk_tip.blocks + 1,
        "incremental mining did not advance the chain by one block"
    );
    reporter.record_height(HeightCheckpoint::Tip, incremental_tip.blocks);
    wait_for_wallet_height(fixture, reporter, incremental_tip.blocks, SEND_TIMEOUT).await?;

    reporter.phase(RecoveryPhase::Faucet);
    let accounts: Vec<serde_json::Value> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        accounts.len() == 5
            && accounts.iter().all(|account| {
                account["id"]
                    .as_u64()
                    .is_some_and(|id| (1..=5).contains(&id))
            }),
        "treasury appeared in public accounts"
    );
    let initial_balance = accounts
        .iter()
        .find(|account| account["id"] == 2)
        .and_then(|account| account["ironwood_zatoshi"].as_u64())
        .ok_or_else(|| anyhow::anyhow!("destination balance was missing"))?;
    for request in 0..10 {
        let payment: Activity = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/faucet",
            Some(&json!({
                "account_id": 2,
                "pool": "ironwood",
                "amount_zatoshi": 500_000_000u64,
                "idempotency_key": format!("treasury-history-faucet-{request}"),
            })),
            SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            payment.status == "confirmed" && payment.amount_zatoshi == 500_000_000,
            "faucet payment did not confirm after large reward history"
        );
    }
    let accounts: Vec<serde_json::Value> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    let final_balance = accounts
        .iter()
        .find(|account| account["id"] == 2)
        .and_then(|account| account["ironwood_zatoshi"].as_u64())
        .ok_or_else(|| anyhow::anyhow!("final destination balance was missing"))?;
    anyhow::ensure!(
        final_balance == initial_balance + 5_000_000_000,
        "faucet did not deliver ten maximum payments"
    );
    Ok(())
}

async fn wait_for_wallet_height(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
    height: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = match fixture
            .recovery_read::<Status>(deadline, "/api/v1/status")
            .await
        {
            Ok(status) => status,
            Err(error) if support::is_retryable_read_transport(&error) => {
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(scanned) = status.wallet_sync.fully_scanned_height {
            reporter.record_height(HeightCheckpoint::Scanned, scanned);
        }
        if status.wallet_sync.state == "ready"
            && status.wallet_sync.fully_scanned_height == Some(height)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet did not converge after direct mining"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

use super::{Connection, Context, OpenFlags, PathBuf, preserve_scenario_failure};

pub(super) async fn startup_funding_retry_after_auto_mine_failure_has_one_payout() -> Result<()> {
    let mut reporter = super::RecoveryFailureReporter::new();
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start_with_funding_fault(true).await?;
        reporter.phase(super::RecoveryPhase::Recovery);
        anyhow::ensure!(
            fixture.proxy().counts().rejected == 1,
            "startup fault did not execute"
        );
        let client = Client::new();
        let activity: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?idempotency_key=startup-account-1-ironwood-v1",
            None,
            Duration::from_secs(5),
        )
        .await?;
        anyhow::ensure!(
            activity.as_array().is_some_and(|a| a.len() == 1),
            "startup key does not identify exactly one payment"
        );
        let payment = &activity[0];
        let original = fixture.proxy().startup_funding_snapshot()?;
        anyhow::ensure!(
            payment["id"] == original.id && payment["txid"] == original.txid,
            "startup retry replaced the first signed transaction"
        );
        let accounts: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            Duration::from_secs(5),
        )
        .await?;
        let account = accounts
            .as_array()
            .context("accounts response is not an array")?
            .iter()
            .find(|account| account["id"] == 1)
            .context("Account 1 missing")?;
        anyhow::ensure!(
            account["ironwood_zatoshi"] == 500_000_000,
            "startup retry produced an incorrect SDK account balance"
        );
        anyhow::ensure!(
            payment["status"] == "confirmed"
                && payment["amount_zatoshi"] == 500_000_000
                && payment["to_account"] == 1,
            "startup funding did not confirm the original amount"
        );
        let db = Connection::open_with_flags(
            fixture.data_dir().join("ths.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let count: u64 = db.query_row(
            "SELECT COUNT(*) FROM activity WHERE kind='faucet' AND to_account=1",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(count == 1, "startup retry created another payout");
        let raw: Vec<u8> = db.query_row(
            "SELECT raw_transaction FROM prepared_payments WHERE activity_id=?1",
            [payment["id"].as_str().context("missing payment id")?],
            |r| r.get(0),
        )?;
        let wallet = Connection::open_with_flags(
            fixture.data_dir().join("wallet.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let (txid, journal): (String, Vec<u8>) = wallet.query_row(
            "SELECT txid,raw_transaction FROM ext_tsz_prepared_payments WHERE activity_id=?1",
            [payment["id"].as_str().unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        anyhow::ensure!(
            payment["txid"] == txid && raw == journal && raw == original.raw,
            "startup retry changed signed payment identity"
        );
        let expiry: u64 = db.query_row(
            "SELECT expiry_height FROM prepared_payments WHERE activity_id=?1",
            [&original.id],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            expiry == original.expiry_height,
            "startup retry changed the original expiry height"
        );
        let evidence: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            evidence["confirmations"].as_u64().is_some_and(|n| n > 0)
                && evidence["blockhash"] == payment["block_hash"],
            "startup payment lacks chain inclusion"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    if scenario.is_err() || cleanup.is_err() {
        reporter.emit(
            Some(&fixture),
            if scenario.is_ok() {
                super::FailureRoute::Cleanup
            } else {
                super::FailureRoute::Error
            },
        );
    }
    preserve_scenario_failure(scenario, cleanup)
}
