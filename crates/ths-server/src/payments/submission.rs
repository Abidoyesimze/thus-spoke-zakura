use super::attempt::PaymentAttempt;
use super::confirmation::confirmed_block_hash;
use super::progression::PAYMENT_LOOKUP_TIMEOUT;
use crate::db::Activity;
use crate::db::AddressFaucet;
use crate::db::PreparedTransaction;
use crate::db::Store;
#[cfg(test)]
use crate::state::AppState;
use crate::wallet::PreparedPayment;
use anyhow::Context;
use serde_json::Value;
use std::time::Duration;

#[async_trait::async_trait]
pub(crate) trait PaymentSubmitter: Sync {
    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool>;
    async fn lookup_original(&self, txid: &str) -> anyhow::Result<Option<Value>>;
    async fn inclusion_height(&self, hash: &str) -> anyhow::Result<u64>;
    async fn canonical_hash(&self, height: u32) -> anyhow::Result<String>;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()>;
    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>>;
}

#[cfg(test)]
#[async_trait::async_trait]
impl PaymentSubmitter for AppState {
    async fn lookup_original(&self, txid: &str) -> anyhow::Result<Option<Value>> {
        PaymentSubmitter::lookup_original(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            txid,
        )
        .await
    }
    async fn inclusion_height(&self, hash: &str) -> anyhow::Result<u64> {
        PaymentSubmitter::inclusion_height(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            hash,
        )
        .await
    }
    async fn canonical_hash(&self, height: u32) -> anyhow::Result<String> {
        PaymentSubmitter::canonical_hash(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            height,
        )
        .await
    }

    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool> {
        PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        }
        .network(
            PAYMENT_LOOKUP_TIMEOUT,
            "looking up payment transaction",
            self.0.rpc.transaction_known(txid),
        )
        .await
    }
    async fn chain_height(&self) -> anyhow::Result<u64> {
        PaymentSubmitter::chain_height(&PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        })
        .await
    }
    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
        PaymentSubmitter::broadcast(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            raw_transaction,
        )
        .await
    }
    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
        self.0.wallet.recover_prepared(txid).await
    }
}

#[async_trait::async_trait]
impl PaymentSubmitter for PaymentAttempt<'_> {
    async fn lookup_original(&self, txid: &str) -> anyhow::Result<Option<Value>> {
        self.network(
            PAYMENT_LOOKUP_TIMEOUT,
            "looking up original payment",
            self.state.0.rpc.lookup_transaction(txid),
        )
        .await
    }
    async fn inclusion_height(&self, hash: &str) -> anyhow::Result<u64> {
        self.network(
            PAYMENT_LOOKUP_TIMEOUT,
            "reading payment inclusion block",
            self.state.0.rpc.block(hash),
        )
        .await?
        .get("height")
        .and_then(Value::as_u64)
        .context("payment inclusion block omitted its height")
    }
    async fn canonical_hash(&self, height: u32) -> anyhow::Result<String> {
        let hash = self
            .network(
                PAYMENT_LOOKUP_TIMEOUT,
                "validating payment inclusion block",
                self.state.0.rpc.block_hash(height),
            )
            .await?;
        anyhow::ensure!(!hash.is_empty(), "canonical payment block hash is empty");
        Ok(hash)
    }

    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool> {
        Ok(self
            .network(
                PAYMENT_LOOKUP_TIMEOUT,
                "looking up payment transaction",
                self.state.0.rpc.lookup_transaction(txid),
            )
            .await?
            .is_some())
    }
    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self
            .network(
                PAYMENT_LOOKUP_TIMEOUT,
                "reading payment chain height",
                self.state.0.rpc.chain_info(),
            )
            .await?
            .blocks)
    }
    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
        self.check_can_start()?;
        if self.dispatch_deadline.is_none() {
            return self.state.0.wallet.broadcast(raw_transaction).await;
        }
        self.state
            .0
            .wallet
            .broadcast_until(
                raw_transaction,
                self.network_deadline(Duration::from_secs(30)),
            )
            .await
    }
    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
        self.check_can_start()?;
        self.state.0.wallet.recover_prepared(txid).await
    }
}

pub(crate) async fn submit_prepared<R: PaymentSubmitter>(
    store: &Store,
    runtime: &R,
    activity: &Activity,
) -> anyhow::Result<PreparedSubmission> {
    match submit_saved_prepared(store, runtime, &activity.id, &activity.txid, |recovered| {
        store.record_prepared(
            &activity.id,
            &activity.txid,
            &recovered.raw_transaction,
            recovered.expiry_height,
        )?;
        Ok(())
    })
    .await?
    {
        SavedSubmission::Broadcast => Ok(PreparedSubmission::Broadcast(
            store.mark_broadcast(&activity.id, &activity.txid)?,
        )),
        SavedSubmission::Included(hash) => Ok(PreparedSubmission::Broadcast(store.confirm(
            &activity.id,
            &activity.txid,
            &hash,
        )?)),
        SavedSubmission::Expired => Ok(PreparedSubmission::Expired(
            store.reset_for_retry(&activity.id, &activity.txid)?,
        )),
    }
}

pub(crate) enum AddressSubmission {
    Broadcast(AddressFaucet),
    Expired(AddressFaucet),
}

pub(crate) async fn submit_address_prepared<R: PaymentSubmitter>(
    store: &Store,
    runtime: &R,
    payment: &AddressFaucet,
) -> anyhow::Result<AddressSubmission> {
    match submit_saved_prepared(store, runtime, &payment.id, &payment.txid, |recovered| {
        store.record_address_prepared(
            &payment.id,
            &payment.txid,
            &recovered.raw_transaction,
            recovered.expiry_height,
        )?;
        Ok(())
    })
    .await?
    {
        SavedSubmission::Broadcast => Ok(AddressSubmission::Broadcast(
            store.mark_address_broadcast(&payment.id, &payment.txid)?,
        )),
        SavedSubmission::Included(hash) => Ok(AddressSubmission::Broadcast(
            store.confirm_address(&payment.id, &payment.txid, &hash)?,
        )),
        SavedSubmission::Expired => Ok(AddressSubmission::Expired(
            store.reset_address_for_retry(&payment.id, &payment.txid)?,
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OriginalTransactionState {
    Missing,
    Present,
    Included(String),
    Expired,
}

pub(crate) async fn original_presence<R: PaymentSubmitter>(
    runtime: &R,
    txid: &str,
) -> anyhow::Result<OriginalTransactionState> {
    let transaction = runtime.lookup_original(txid).await?;
    original_presence_from_lookup(runtime, txid, transaction).await
}

pub(crate) async fn original_presence_from_lookup<R: PaymentSubmitter>(
    runtime: &R,
    txid: &str,
    transaction: Option<Value>,
) -> anyhow::Result<OriginalTransactionState> {
    let Some(transaction) = transaction else {
        return Ok(OriginalTransactionState::Missing);
    };
    anyhow::ensure!(
        transaction.get("txid").and_then(Value::as_str) == Some(txid),
        "original payment lookup returned a different transaction"
    );
    // Zakura omits confirmations for mempool transactions.
    let confirmations = match transaction.get("confirmations") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_i64()
            .context("original payment lookup has invalid confirmations")?,
    };
    anyhow::ensure!(
        confirmations >= 0,
        "original payment is not on the selected chain"
    );
    if confirmations == 0 {
        anyhow::ensure!(
            transaction.get("blockhash").is_none_or(Value::is_null),
            "unconfirmed original payment has an inclusion block"
        );
        return Ok(OriginalTransactionState::Present);
    }
    let hash = confirmed_block_hash(&transaction)
        .context("original payment inclusion omitted its block hash")?;
    let height = runtime.inclusion_height(hash).await?;
    anyhow::ensure!(
        runtime.canonical_hash(u32::try_from(height)?).await? == hash,
        "original payment inclusion is not canonical"
    );
    Ok(OriginalTransactionState::Included(hash.to_owned()))
}

pub(crate) async fn original_after_absence<R: PaymentSubmitter>(
    runtime: &R,
    txid: &str,
    expiry_height: u64,
) -> anyhow::Result<OriginalTransactionState> {
    if expiry_height == 0 {
        return Ok(OriginalTransactionState::Missing);
    }
    let height = runtime.chain_height().await?;
    let presence = original_presence(runtime, txid).await?;
    if presence != OriginalTransactionState::Missing {
        return Ok(presence);
    }
    // Block N may include expiry N. At tip N, absence means the next possible
    // inclusion is N+1, so the original can safely be replaced.
    Ok(if height >= expiry_height {
        OriginalTransactionState::Expired
    } else {
        OriginalTransactionState::Missing
    })
}

pub(crate) async fn original_transaction_state<R: PaymentSubmitter>(
    runtime: &R,
    txid: &str,
    expiry_height: u64,
) -> anyhow::Result<OriginalTransactionState> {
    match original_presence(runtime, txid).await? {
        OriginalTransactionState::Missing => {
            original_after_absence(runtime, txid, expiry_height).await
        }
        presence => Ok(presence),
    }
}

pub(crate) enum SavedSubmission {
    Broadcast,
    Included(String),
    Expired,
}

pub(crate) async fn submit_saved_prepared<
    R: PaymentSubmitter,
    F: FnOnce(&PreparedPayment) -> anyhow::Result<()>,
>(
    store: &Store,
    runtime: &R,
    id: &str,
    txid: &str,
    record_recovered: F,
) -> anyhow::Result<SavedSubmission> {
    match original_presence(runtime, txid)
        .await
        .context("reconciling original payment before broadcast")?
    {
        OriginalTransactionState::Present => return Ok(SavedSubmission::Broadcast),
        OriginalTransactionState::Included(hash) => return Ok(SavedSubmission::Included(hash)),
        OriginalTransactionState::Missing => {}
        OriginalTransactionState::Expired => unreachable!(),
    }
    let prepared = match store.optional_prepared_transaction(id)? {
        Some(prepared) => prepared,
        None => {
            let recovered = runtime
                .recover_prepared(txid)
                .await?
                .with_context(|| format!("prepared transaction {id} is missing"))?;
            if recovered.txid != txid {
                anyhow::bail!("wallet returned a different prepared transaction");
            }
            record_recovered(&recovered)?;
            PreparedTransaction {
                raw_transaction: recovered.raw_transaction,
                expiry_height: recovered.expiry_height,
            }
        }
    };
    match original_after_absence(runtime, txid, prepared.expiry_height)
        .await
        .context("reconciling payment expiry before broadcast")?
    {
        OriginalTransactionState::Present => return Ok(SavedSubmission::Broadcast),
        OriginalTransactionState::Included(hash) => return Ok(SavedSubmission::Included(hash)),
        OriginalTransactionState::Expired => return Ok(SavedSubmission::Expired),
        OriginalTransactionState::Missing => {}
    }
    if let Err(broadcast_error) = runtime.broadcast(&prepared.raw_transaction).await {
        match runtime.transaction_known(txid).await {
            Ok(true) => {}
            Ok(false) => return Err(broadcast_error.context("broadcasting prepared payment")),
            Err(lookup_error) => {
                return Err(lookup_error).with_context(|| {
                    format!(
                        "broadcast failed and transaction {txid} could not be checked: {broadcast_error:#}"
                    )
                });
            }
        }
    }
    Ok(SavedSubmission::Broadcast)
}

pub(crate) enum PreparedSubmission {
    Broadcast(Activity),
    Expired(Activity),
}

#[cfg(test)]
#[path = "../../tests/unit/payments/submission/tests.rs"]
pub(crate) mod tests;
