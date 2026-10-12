use super::{
    confirmation::confirm_address_after_mining,
    execution::{execute_payment, record_prepared_payment},
    submission::{AddressSubmission, submit_address_prepared},
};
use crate::rpc::ChainCheckpoint;
use crate::state::AppState;
use crate::{
    db::{Account, Activity, AddressFaucet, Store, TREASURY_ACCOUNT_ID, ZATOSHIS_PER_ZEC},
    sync::mine_and_sync,
    wallet::{PaymentError, PreparedPayment, WALLET_BIRTHDAY_HEIGHT},
};
use crate::{network::regtest_network, payments::confirmation::confirmed_block_hash};
use anyhow::Context;
use serde_json::Value;
use zcash_keys::encoding::AddressCodec;
use zcash_protocol::consensus::COINBASE_MATURITY_BLOCKS;
pub(crate) async fn fund_address_from_treasury(
    state: &AppState,
    address: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<AddressFaucet> {
    let _payment = state.0.payments.lock().await;
    let mut pending =
        state
            .0
            .store
            .claim_address_faucet(address, amount_zatoshi, idempotency_key)?;
    loop {
        match pending.status.as_str() {
            "confirmed" => return Ok(pending),
            "prepared" | "broadcast" => {
                match submit_address_prepared(&state.0.store, state, &pending).await? {
                    AddressSubmission::Broadcast(payment) => {
                        pending = payment;
                        break;
                    }
                    AddressSubmission::Expired(payment) => pending = payment,
                }
            }
            "preparing" => {
                let prepared = async {
                    state.synchronize_latest().await?;
                    let seed = state.0.store.seed()?;
                    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
                    let _replenishment = state.0.treasury_replenishment.lock().await;
                    prepare_with_replenishment(
                        state,
                        Some(&pending.id),
                        &seed,
                        &treasury,
                        address,
                        amount_zatoshi,
                        record_prepared_address_payment(&state.0.store, &pending.id),
                    )
                    .await
                }
                .await;
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        if !state.0.wallet.has_prepared(&pending.id).await? {
                            state.0.store.discard_address_preparing(&pending.id)?;
                        }
                        return Err(error);
                    }
                };
                pending = state.0.store.record_address_prepared(
                    &pending.id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
            }
            status => anyhow::bail!("address faucet has unsupported status {status}"),
        }
    }
    confirm_address_after_mining(state, pending).await
}

pub(crate) async fn fund_from_treasury(
    state: &AppState,
    account_id: u8,
    pool: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<Activity> {
    let _payment = state.0.payments.lock().await;
    let pending = state
        .0
        .store
        .claim_faucet(account_id, pool, amount_zatoshi, idempotency_key)?;
    execute_payment(state, pending, |activity_id| async move {
        let destination = state.0.store.account(account_id)?;
        let address = match pool {
            "transparent" => destination.transparent_address,
            "ironwood" => destination.unified_address,
            _ => unreachable!("claim_faucet validates pool"),
        };
        state.synchronize_latest().await?;
        let seed = state.0.store.seed()?;
        // SDK proposals check spendability and the actual fee before construction. Total
        // balances include pending change and cannot decide whether this request is fundable.
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
        let _replenishment = state.0.treasury_replenishment.lock().await;
        prepare_with_replenishment(
            state,
            Some(&activity_id),
            &seed,
            &treasury,
            &address,
            amount_zatoshi,
            record_prepared_payment(&state.0.store, &activity_id),
        )
        .await
    })
    .await
}

#[async_trait::async_trait]
trait FaucetRuntime: Sync {
    async fn prepare_payment<F>(
        &self,
        activity_id: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
        record: F,
    ) -> anyhow::Result<PreparedPayment>
    where
        F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Send + 'static;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn block_hash(&self, height: u32) -> anyhow::Result<String>;
    async fn synchronize_latest(&self) -> anyhow::Result<()>;
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>>;
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()>;
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()>;
    async fn mine_to_height(&self, height: u64) -> anyhow::Result<()> {
        let current = self.chain_height().await?;
        if height > current {
            self.mine_and_sync(u32::try_from(height - current)?).await?;
        }
        anyhow::ensure!(
            self.chain_height().await? >= height,
            "maturity mining did not reach height {height}"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl FaucetRuntime for AppState {
    async fn prepare_payment<F>(
        &self,
        activity_id: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
        record: F,
    ) -> anyhow::Result<PreparedPayment>
    where
        F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Send + 'static,
    {
        self.0
            .wallet
            .prepare(
                activity_id,
                seed,
                TREASURY_ACCOUNT_ID,
                "ironwood",
                destination,
                amount_zatoshi,
                None,
                record,
            )
            .await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.0.rpc.chain_info().await?.blocks)
    }

    async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
        self.0.rpc.block_hash(height).await
    }
    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        AppState::synchronize_latest(self).await
    }
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
        self.0.wallet.treasury_cursor().await
    }
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        self.0
            .wallet
            .initialize_treasury_cursor(receiver, height, hash)
            .await
    }
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        let _sync = self.0.wallet_sync.lock().await;
        discover_reward(self, cursor, treasury).await
    }
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        self.0
            .wallet
            .shield_coinbase(
                seed,
                TREASURY_ACCOUNT_ID,
                &treasury.transparent_address,
                &treasury.unified_address,
                minimum_net,
            )
            .await?;
        Ok(())
    }

    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        mine_and_sync(self, blocks).await?;
        Ok(())
    }
}

async fn discover_reward(
    state: &AppState,
    cursor: &crate::db::TreasuryCursor,
    treasury: &Account,
) -> anyhow::Result<crate::db::TreasuryCursor> {
    let next_height = cursor
        .height
        .checked_add(1)
        .context("treasury discovery height overflow")?;
    let block_hash = state.0.rpc.block_hash(next_height).await?;
    let block = state.0.rpc.block(&block_hash).await?;
    anyhow::ensure!(
        block.get("height").and_then(Value::as_u64) == Some(u64::from(next_height)),
        "Zakura returned the wrong treasury discovery height"
    );
    let candidate = coinbase_candidate(&block, &treasury.transparent_address)?;
    let mut required_spenders = Vec::new();
    if let Some(candidate) = &candidate {
        for index in &candidate.output_indexes {
            if state
                .0
                .rpc
                .unspent_output(&candidate.txid, *index)
                .await?
                .is_none()
            {
                let spenders = state
                    .0
                    .wallet
                    .known_spending_transactions(&candidate.txid, *index)
                    .await?;
                let mut accepted = Vec::new();
                for txid in spenders {
                    let Some(transaction) = state.0.rpc.lookup_transaction(&txid).await? else {
                        continue;
                    };
                    if let Some(hash) = confirmed_block_hash(&transaction) {
                        let height = state
                            .0
                            .rpc
                            .block(hash)
                            .await?
                            .get("height")
                            .and_then(Value::as_u64)
                            .context("spender block omitted its height")?;
                        anyhow::ensure!(
                            state.0.rpc.block_hash(u32::try_from(height)?).await? == hash,
                            "spender block is not canonical"
                        );
                    } else if !state.0.rpc.mempool().await?.contains(&txid) {
                        continue;
                    }
                    accepted.push((*index, txid));
                }
                anyhow::ensure!(
                    !accepted.is_empty(),
                    "treasury output has no known spender; discovery remains pending"
                );
                required_spenders.extend(accepted);
            }
        }
    }
    let checkpoint = ChainCheckpoint {
        height: u64::from(next_height),
        hash: block_hash,
    };
    anyhow::ensure!(
        state.0.rpc.block_hash(next_height).await? == checkpoint.hash,
        "canonical block changed during treasury discovery"
    );
    let next = state
        .0
        .wallet
        .commit_treasury_discovery(
            cursor,
            &checkpoint,
            candidate.as_ref().map(|value| value.raw.as_str()),
            &required_spenders,
        )
        .await?;
    anyhow::ensure!(
        state.0.rpc.block_hash(next_height).await? == checkpoint.hash,
        "canonical block changed after treasury discovery"
    );
    Ok(next)
}

async fn prepare_with_replenishment<R, F>(
    runtime: &R,
    activity_id: Option<&str>,
    seed: &str,
    treasury: &Account,
    destination: &str,
    amount_zatoshi: u64,
    record: F,
) -> anyhow::Result<PreparedPayment>
where
    R: FaucetRuntime,
    F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static,
{
    let mut reconciled_after_scan_required = false;
    loop {
        match runtime
            .prepare_payment(
                activity_id,
                seed,
                destination,
                amount_zatoshi,
                record.clone(),
            )
            .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(error) => {
                if matches!(error.downcast_ref(), Some(PaymentError::ScanRequired)) {
                    if !reconciled_after_scan_required {
                        runtime.synchronize_latest().await?;
                        reconciled_after_scan_required = true;
                        continue;
                    }
                    if runtime.chain_height().await? < u64::from(WALLET_BIRTHDAY_HEIGHT) {
                        runtime
                            .mine_to_height(u64::from(WALLET_BIRTHDAY_HEIGHT))
                            .await?;
                        runtime.synchronize_latest().await?;
                        continue;
                    }
                    return Err(error);
                }
                let Some(PaymentError::InsufficientFunds {
                    available,
                    required,
                }) = error.downcast_ref()
                else {
                    return Err(error);
                };
                let minimum_net = required
                    .checked_sub(*available)
                    .filter(|value| *value > 0)
                    .context("SDK insufficient-funds result has no positive deficit")?;
                replenish_treasury(runtime, seed, treasury, minimum_net).await?;
                reconciled_after_scan_required = false;
            }
        }
    }
}

async fn replenish_treasury<R: FaucetRuntime>(
    runtime: &R,
    seed: &str,
    treasury: &Account,
    minimum_net: u64,
) -> anyhow::Result<()> {
    let boundary = WALLET_BIRTHDAY_HEIGHT - 1;
    runtime.mine_to_height(u64::from(boundary)).await?;
    let mut cursor = match runtime.treasury_cursor().await? {
        Some(cursor) => {
            anyhow::ensure!(
                cursor.receiver == treasury.transparent_address,
                "treasury cursor belongs to a different receiver"
            );
            cursor
        }
        None => {
            let hash = runtime.block_hash(boundary).await?;
            runtime
                .initialize_treasury_cursor(&treasury.transparent_address, boundary, &hash)
                .await?
        }
    };
    if runtime.chain_height().await? < u64::from(cursor.height)
        || runtime.block_hash(cursor.height).await? != cursor.block_hash
    {
        runtime.synchronize_latest().await?;
        cursor = runtime
            .treasury_cursor()
            .await?
            .context("treasury cursor disappeared during rewind")?;
        anyhow::ensure!(
            runtime.chain_height().await? >= u64::from(cursor.height)
                && runtime.block_hash(cursor.height).await? == cursor.block_hash,
            "treasury cursor is not canonical after wallet rewind"
        );
    }

    loop {
        match runtime.shield_coinbase(seed, treasury, minimum_net).await {
            Ok(()) => return runtime.mine_and_sync(1).await,
            Err(error) if matches!(error.downcast_ref(), Some(PaymentError::TreasuryExhausted)) => {
            }
            Err(error) => return Err(error),
        }
        let next_height = cursor
            .height
            .checked_add(1)
            .context("treasury discovery height overflow")?;
        // The SDK constructs a spend at tip + 1; 100 confirmations make this reward eligible there.
        let required_height = u64::from(next_height) + u64::from(COINBASE_MATURITY_BLOCKS) - 1;
        runtime.mine_to_height(required_height).await?;
        cursor = runtime.discover_reward(&cursor, treasury).await?;
    }
}

struct CoinbaseCandidate {
    txid: String,
    raw: String,
    output_indexes: Vec<u32>,
}

fn coinbase_candidate(block: &Value, receiver: &str) -> anyhow::Result<Option<CoinbaseCandidate>> {
    let height = block
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|height| u32::try_from(height).ok())
        .context("Zakura block omitted its height")?;
    let transaction = block
        .get("tx")
        .and_then(Value::as_array)
        .and_then(|transactions| transactions.first())
        .context("Zakura block omitted its coinbase transaction")?;
    let raw = transaction
        .get("hex")
        .and_then(Value::as_str)
        .context("Zakura coinbase omitted its raw transaction")?;
    let bytes = hex::decode(raw).context("invalid treasury transaction hex")?;
    let decoded = zcash_primitives::transaction::Transaction::read(
        &bytes[..],
        zcash_protocol::consensus::BranchId::for_height(&regtest_network(), height.into()),
    )?;
    let txid = decoded.txid().to_string();
    anyhow::ensure!(
        transaction.get("txid").and_then(Value::as_str) == Some(txid.as_str()),
        "Zakura coinbase transaction ID does not match its raw transaction"
    );
    let bundle = decoded
        .transparent_bundle()
        .filter(|bundle| bundle.is_coinbase())
        .context("Zakura block's first transaction is not coinbase")?;
    let output_indexes = bundle
        .vout
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            output
                .recipient_address()
                .filter(|address| address.encode(&regtest_network()) == receiver)
                .map(|_| u32::try_from(index).context("treasury output index exceeds u32"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if output_indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(CoinbaseCandidate {
        txid,
        raw: raw.to_owned(),
        output_indexes,
    }))
}

pub(crate) async fn provision_initial_balance(state: &AppState) -> anyhow::Result<()> {
    const INITIAL_FUNDING_KEY: &str = "startup-account-1-ironwood-v1";
    let existing = state.0.store.activity_for_key(INITIAL_FUNDING_KEY)?;
    if existing
        .as_ref()
        .is_some_and(|activity| activity.status == "confirmed")
    {
        return Ok(());
    }
    if existing.is_none() {
        // A fresh wallet needs scanned blocks before a proposal can determine its target height.
        let seed = state.0.store.seed()?;
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
        replenish_treasury(state, &seed, &treasury, 1).await?;
    }
    fund_from_treasury(
        state,
        1,
        "ironwood",
        5 * ZATOSHIS_PER_ZEC,
        INITIAL_FUNDING_KEY,
    )
    .await?;
    let account = state
        .accounts()
        .await
        .into_iter()
        .find(|account| account.id == 1)
        .context("Account 1 disappeared during startup provisioning")?;
    anyhow::ensure!(
        account.ironwood_zatoshi == 5 * ZATOSHIS_PER_ZEC,
        "Account 1 startup Ironwood balance is {}, expected {} zatoshi",
        account.ironwood_zatoshi,
        5 * ZATOSHIS_PER_ZEC
    );
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/payments/treasury/tests.rs"]
pub(crate) mod tests;

fn record_prepared_address_payment(
    store: &Store,
    id: &str,
) -> impl FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static {
    let store = store.clone();
    let id = id.to_owned();
    move |prepared| {
        store.record_address_prepared(
            &id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )?;
        Ok(prepared)
    }
}
