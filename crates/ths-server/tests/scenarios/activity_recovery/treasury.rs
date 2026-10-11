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
