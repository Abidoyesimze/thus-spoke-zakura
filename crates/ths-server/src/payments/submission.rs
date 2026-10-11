use crate::{
    db::{Activity, AddressFaucet, PreparedTransaction, Store},
    state::AppState,
    wallet::PreparedPayment,
};
use anyhow::Context;
#[async_trait::async_trait]
pub(crate) trait PaymentSubmitter: Sync {
    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool>;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()>;
    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>>;
}

#[async_trait::async_trait]
impl PaymentSubmitter for AppState {
    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool> {
        self.0.rpc.transaction_known(txid).await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.0.rpc.chain_info().await?.blocks)
    }

    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
        self.0.wallet.broadcast(raw_transaction).await
    }

    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
        self.0.wallet.recover_prepared(txid).await
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
        SavedSubmission::Expired => Ok(AddressSubmission::Expired(
            store.reset_address_for_retry(&payment.id, &payment.txid)?,
        )),
    }
}

enum SavedSubmission {
    Broadcast,
    Expired,
}

async fn submit_saved_prepared<
    R: PaymentSubmitter,
    F: FnOnce(&PreparedPayment) -> anyhow::Result<()>,
>(
    store: &Store,
    runtime: &R,
    id: &str,
    txid: &str,
    record_recovered: F,
) -> anyhow::Result<SavedSubmission> {
    if runtime.transaction_known(txid).await? {
        return Ok(SavedSubmission::Broadcast);
    }
    let prepared = match store.prepared_transaction(id) {
        Ok(prepared) => prepared,
        Err(missing) => {
            let recovered = runtime.recover_prepared(txid).await?.ok_or(missing)?;
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
    if prepared.expiry_height != 0 && runtime.chain_height().await? >= prepared.expiry_height {
        if runtime.transaction_known(txid).await? {
            return Ok(SavedSubmission::Broadcast);
        }
        return Ok(SavedSubmission::Expired);
    }
    if let Err(broadcast_error) = runtime.broadcast(&prepared.raw_transaction).await {
        match runtime.transaction_known(txid).await {
            Ok(true) => {}
            Ok(false) => return Err(broadcast_error),
            Err(lookup_error) => {
                return Err(lookup_error).with_context(|| {
                    format!(
                        "broadcast failed and transaction {txid} could not be checked: {broadcast_error}"
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
