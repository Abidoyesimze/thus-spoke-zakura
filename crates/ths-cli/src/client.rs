use crate::{
    intents::FaucetJournal,
    runtime::{InstanceName, Runtime, container_running, prefix},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MineResult {
    pub(crate) blocks: usize,
    pub(crate) hashes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    #[serde(default = "confirmed_status")]
    status: String,
}

fn confirmed_status() -> String {
    "confirmed".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    to_account: u8,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

impl Runtime {
    fn instance_client(&self, name: &InstanceName) -> Result<(String, reqwest::blocking::Client)> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container).unwrap_or(false) {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok((dashboard, client))
    }

    pub fn mine(&self, name: &InstanceName, blocks: u32, json: bool) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let result: MineResult = client
            .post(format!("{dashboard}/api/v1/mine"))
            .json(&serde_json::json!({"blocks": blocks}))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Mined {} blocks on {name}.", result.blocks);
            if let Some(tip) = result.hashes.last() {
                println!("New tip: {tip}");
            }
        }
        Ok(())
    }

    pub fn faucet(
        &self,
        name: &InstanceName,
        address: &str,
        amount_zatoshi: u64,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let intent = format!("address:{address}:{amount_zatoshi}");
        let idempotency_key = journal.key_for(&intent)?;
        let result: FaucetResult = client
            .post(format!("{dashboard}/api/v1/faucet/address"))
            .json(&serde_json::json!({
                "address": address,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
            }))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to fund {address}"))?;
        if result.status == "confirmed" && result.block_hash.is_none() {
            bail!("faucet reported confirmation without a block hash; retry the same command");
        }
        if result.status == "confirmed" {
            journal.clear(&[&intent])?;
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Transaction: {}", result.txid);
            if let Some(block_hash) = &result.block_hash {
                println!(
                    "Sent {} ZEC to {} on {name}.",
                    format_zec(result.amount_zatoshi),
                    result.address
                );
                println!("Confirmed in: {block_hash}");
            } else {
                println!(
                    "Payment to {} is pending. Run the same command to check it again.",
                    result.address
                );
            }
        }
        if result.status == "confirmed" {
            Ok(())
        } else {
            bail!("faucet payment is pending; run the same command to check it again")
        }
    }

    pub fn wallet_faucet(
        &self,
        name: &InstanceName,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_faucet_with(
            name,
            &dashboard,
            &client,
            accounts,
            amount_zatoshi,
            pool,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn wallet_faucet_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let batch = journal.wallet_batch(pool, amount_zatoshi, accounts)?;
        let intents: Vec<_> = accounts.iter().map(|id| format!("{batch}:{id}")).collect();
        let mut funded = Vec::new();
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        for (&account_id, intent) in accounts.iter().zip(&intents) {
            let idempotency_key = journal.key_for(intent)?;
            let outcome = client
                .post(format!("{dashboard}/api/v1/faucet"))
                .json(&serde_json::json!({
                    "account_id": account_id,
                    "pool": pool,
                    "amount_zatoshi": amount_zatoshi,
                    "idempotency_key": idempotency_key,
                }))
                .send()
                .with_context(|| format!("asking environment {name} to fund account {account_id}"));
            match outcome.and_then(decode_response::<Activity>) {
                Ok(activity) if activity.status == "confirmed" => funded.push(activity),
                Ok(activity) => pending.push(activity),
                Err(error) => failures.push(format!("account {account_id}: {error:#}")),
            }
        }
        if failures.is_empty() && pending.is_empty() {
            journal.clear(&intents.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "pending": pending, "failed": failures})
                )?
            );
        } else {
            for activity in &funded {
                println!(
                    "Funded account {} with {} ZEC ({} pool) on {name}.",
                    activity.to_account,
                    format_zec(activity.amount_zatoshi),
                    activity.destination_pool
                );
                println!("  Transaction: {}", activity.txid);
            }
            for activity in &pending {
                eprintln!(
                    "Payment to account {} is pending ({}). Run the same command to check it again.",
                    activity.to_account, activity.txid
                );
            }
            for failure in &failures {
                eprintln!("Failed to fund {failure}");
            }
        }
        if failures.is_empty() && pending.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} of {} faucet requests are pending or failed",
                failures.len() + pending.len(),
                accounts.len()
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wallet_send(
        &self,
        name: &InstanceName,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount_zatoshi: u64,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let idempotency_key = format!("ths-wallet-send-{}", uuid::Uuid::new_v4());
        let activity: Activity = client
            .post(format!("{dashboard}/api/v1/send"))
            .json(&serde_json::json!({
                "from_account": from,
                "to_account": to,
                "source_pool": source_pool,
                "destination_pool": destination_pool,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
                "memo": memo,
            }))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| {
                format!("asking environment {name} to send from account {from} to account {to}")
            })?;
        if json {
            println!("{}", serde_json::to_string_pretty(&activity)?);
        } else {
            println!(
                "Sent {} ZEC from account {} ({} pool) to account {} ({} pool) on {name}.",
                format_zec(activity.amount_zatoshi),
                from,
                activity.source_pool,
                activity.to_account,
                activity.destination_pool
            );
            if let Some(memo) = memo {
                println!("Memo: {memo}");
            }
            println!("Transaction: {}", activity.txid);
            if let Some(block_hash) = &activity.block_hash {
                println!("Confirmed in: {block_hash}");
            }
        }
        Ok(())
    }
}

pub(crate) fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let detail = response
            .text()
            .unwrap_or_else(|_| "response body was unreadable".to_owned());
        bail!("rejected ({status}): {detail}");
    }
    response.json().context("decoding response")
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / 100_000_000;
    let fraction = zatoshi % 100_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:08}")
            .trim_end_matches('0')
            .to_owned()
    }
}
