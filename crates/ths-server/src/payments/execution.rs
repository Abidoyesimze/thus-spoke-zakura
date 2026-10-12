use super::{
    confirmation::confirm_after_mining,
    submission::{PreparedSubmission, submit_prepared},
};
use crate::{
    db::{Activity, Store},
    state::AppState,
    wallet::PreparedPayment,
};
use std::future::Future;
#[async_trait::async_trait]
pub(crate) trait PaymentLifecycle: Sync {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission>;
    async fn discard_unprepared_claim(&self, id: &str) -> anyhow::Result<()>;
    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity>;
    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity>;
}

#[async_trait::async_trait]
impl PaymentLifecycle for AppState {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        submit_prepared(&self.0.store, self, activity).await
    }

    async fn discard_unprepared_claim(&self, id: &str) -> anyhow::Result<()> {
        discard_unprepared_claim(self, id).await
    }

    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity> {
        self.0.store.record_prepared(
            id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )
    }

    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity> {
        confirm_after_mining(self, activity).await
    }
}

pub(crate) async fn execute_payment<R, Prepare, PrepareFuture>(
    runtime: &R,
    mut pending: Activity,
    mut prepare: Prepare,
) -> anyhow::Result<Activity>
where
    R: PaymentLifecycle,
    Prepare: FnMut(String) -> PrepareFuture,
    PrepareFuture: Future<Output = anyhow::Result<PreparedPayment>>,
{
    loop {
        match pending.status.as_str() {
            "confirmed" => return Ok(pending),
            "prepared" | "broadcast" => match runtime.submit_prepared(&pending).await? {
                PreparedSubmission::Broadcast(activity) => {
                    return runtime.confirm_after_mining(activity).await;
                }
                PreparedSubmission::Expired(activity) => pending = activity,
            },
            "preparing" => {
                let prepared = match prepare(pending.id.clone()).await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        runtime.discard_unprepared_claim(&pending.id).await?;
                        return Err(error);
                    }
                };
                pending = runtime.record_prepared(&pending.id, &prepared)?;
            }
            status => anyhow::bail!("payment has unsupported status {status}"),
        }
    }
}

async fn discard_unprepared_claim(state: &AppState, id: &str) -> anyhow::Result<()> {
    if !state.0.wallet.has_prepared(id).await? {
        state.0.store.discard_preparing(id)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/payments/execution/tests.rs"]
pub(crate) mod tests;

pub(crate) fn record_prepared_payment(
    store: &Store,
    id: &str,
) -> impl FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static {
    let store = store.clone();
    let id = id.to_owned();
    move |prepared| {
        store.record_prepared(
            &id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )?;
        Ok(prepared)
    }
}
