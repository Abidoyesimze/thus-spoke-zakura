use super::{
    API_READ_TIMEOUT, AccountBalance, Activity, CONCURRENT_IDEMPOTENCY_KEY, ChainInfo, Client,
    PathBuf, RegtestStack, Result, SEND_TIMEOUT, assert_persisted_send_response, json,
    one_matching_activity, preserve_scenario_failure, queued_retry_survives_restart, request_json,
    rpc,
};

pub(super) async fn queued_send_retry_survives_lost_response_and_restart() -> Result<()> {
    queued_retry_survives_restart(false).await
}

pub(super) async fn same_account_cross_pool_round_trip_is_replay_safe() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let initial: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let initial = initial
            .iter()
            .find(|a| a.id == 1)
            .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
        anyhow::ensure!(
            initial.transparent_zatoshi == 0,
            "initial transparent balance is nonzero"
        );
        let mut expected_ironwood = initial.ironwood_zatoshi;
        let initial_activity: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let mut prior_txid = None::<String>;
        for (index, source, destination, amount, key) in [
            (
                1,
                "ironwood",
                "transparent",
                1_000_000u64,
                "self-unshield-round-trip",
            ),
            (
                2,
                "transparent",
                "ironwood",
                500_000u64,
                "self-shield-round-trip",
            ),
        ] {
            let body = json!({
                "from_account": 1, "to_account": 1, "source_pool": source,
                "destination_pool": destination, "amount_zatoshi": amount,
                "idempotency_key": key,
            });
            let sent: Activity = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(sent.status == "confirmed", "send did not confirm");
            anyhow::ensure!(sent.from_account == Some(1), "source account changed");
            anyhow::ensure!(sent.to_account == 1, "destination account changed");
            anyhow::ensure!(sent.source_pool == source, "source pool changed");
            anyhow::ensure!(
                sent.destination_pool == destination,
                "destination pool changed"
            );
            anyhow::ensure!(sent.amount_zatoshi == amount, "send amount changed");
            let tx: serde_json::Value = rpc(
                &client,
                fixture.node_url(),
                "getrawtransaction",
                json!([sent.txid, 1]),
            )
            .await?;
            anyhow::ensure!(
                tx["confirmations"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                    >= 1,
                "transaction has no confirmation"
            );
            let hash = tx["blockhash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                sent.block_hash.as_deref() == Some(hash),
                "activity block hash differs from the transaction"
            );
            let block: serde_json::Value =
                rpc(&client, fixture.node_url(), "getblock", json!([hash, 1])).await?;
            anyhow::ensure!(
                block["tx"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                    .iter()
                    .any(|id| id == &json!(sent.txid)),
                "transaction is absent from its confirming block"
            );
            let expected_transparent;
            if source == "ironwood" {
                anyhow::ensure!(
                    tx["vin"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                        .is_empty(),
                    "unshield transaction has transparent inputs"
                );
                anyhow::ensure!(
                    tx["vout"][0]["valueZat"] == 1_000_000,
                    "unshield transparent output differs from 1,000,000 zatoshis"
                );
                anyhow::ensure!(
                    tx["ironwood"]["valueBalanceZat"] == 1_015_000,
                    "unshield Ironwood balance differs from 1,015,000 zatoshis"
                );
                expected_ironwood -= 1_015_000;
                expected_transparent = 1_000_000;
                prior_txid = Some(sent.txid.clone());
            } else {
                anyhow::ensure!(
                    tx["vin"][0]["txid"].as_str() == prior_txid.as_deref(),
                    "shield does not consume the preceding unshield transaction"
                );
                anyhow::ensure!(
                    tx["vin"][0]["vout"] == 0,
                    "shield does not consume transparent output zero"
                );
                anyhow::ensure!(
                    tx["vout"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                        .is_empty(),
                    "shield left transparent change"
                );
                anyhow::ensure!(
                    tx["ironwood"]["valueBalanceZat"] == -985_000,
                    "shield Ironwood balance differs from -985,000 zatoshis"
                );
                expected_ironwood += 985_000;
                expected_transparent = 0;
            }
            let accounts: Vec<AccountBalance> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let account = accounts
                .iter()
                .find(|a| a.id == 1)
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                account.ironwood_zatoshi == expected_ironwood,
                "Ironwood balance differs from the expected change"
            );
            anyhow::ensure!(
                account.transparent_zatoshi == expected_transparent,
                "transparent balance differs from the expected output"
            );
            let activities: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?limit=100",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                activities.len() == initial_activity.len() + index,
                "send did not add exactly one activity"
            );
            anyhow::ensure!(
                activities.iter().filter(|a| a.id == sent.id).count() == 1,
                "activity ID was not recorded exactly once"
            );
            anyhow::ensure!(
                activities.iter().filter(|a| a.txid == sent.txid).count() == 1,
                "transaction ID was not recorded exactly once"
            );
            let persisted = one_matching_activity(&activities, &sent)?;
            assert_persisted_send_response(&sent, &persisted)?;
            let height_before: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            let replay: Activity = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                replay == persisted,
                "replay differs from persisted activity"
            );
            let height_after: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            anyhow::ensure!(
                height_after.blocks == height_before.blocks,
                "replay advanced the chain height"
            );
            anyhow::ensure!(
                height_after.bestblockhash == height_before.bestblockhash,
                "replay changed the chain tip"
            );
            let after: Vec<AccountBalance> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let after = after
                .iter()
                .find(|a| a.id == 1)
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                after.ironwood_zatoshi == account.ironwood_zatoshi,
                "replay changed the Ironwood balance"
            );
            anyhow::ensure!(
                after.transparent_zatoshi == account.transparent_zatoshi,
                "replay changed the transparent balance"
            );
            let after_activity: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?limit=100",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                after_activity == activities,
                "replay changed activity history"
            );
        }
        anyhow::ensure!(
            initial.ironwood_zatoshi - expected_ironwood == 30_000,
            "round trip did not spend exactly 30,000 zatoshis in fees"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn concurrent_identical_sends_have_one_chain_effect() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let before_accounts: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let before_balance = before_accounts
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .ok_or_else(|| anyhow::anyhow!("destination account is missing"))?;
        let before_activities: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let request = json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 10_000_000,
            "idempotency_key": CONCURRENT_IDEMPOTENCY_KEY,
        });

        let (first, second) = tokio::join!(
            request_json::<Activity>(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&request),
                SEND_TIMEOUT,
            ),
            request_json::<Activity>(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&request),
                SEND_TIMEOUT,
            ),
        );
        let first = first?;
        let second = second?;
        anyhow::ensure!(
            first.id == second.id,
            "requests returned different activities"
        );
        anyhow::ensure!(
            first.txid == second.txid,
            "requests returned different transactions"
        );
        anyhow::ensure!(
            first.status == "confirmed" && second.status == "confirmed",
            "requests did not converge on a confirmed payment"
        );

        let after_accounts: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let after_balance = after_accounts
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .ok_or_else(|| anyhow::anyhow!("destination account is missing"))?;
        anyhow::ensure!(
            after_balance.checked_sub(before_balance) == Some(10_000_000),
            "recipient balance changed by more than one payment"
        );
        let after_activities: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            after_activities.len() == before_activities.len() + 1,
            "concurrent requests did not create exactly one activity"
        );
        anyhow::ensure!(
            after_activities
                .iter()
                .filter(|activity| activity.id == first.id && activity.txid == first.txid)
                .count()
                == 1,
            "activity and transaction were not recorded exactly once"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}
