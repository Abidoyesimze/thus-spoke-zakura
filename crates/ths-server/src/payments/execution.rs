use super::progression::{can_fail_insufficient_send, is_insufficient_funds};
use super::{
    confirmation::confirm_after_mining,
    submission::{PreparedSubmission, submit_prepared},
};
use crate::{
    db::{Activity, Store},
    state::AppState,
    wallet::{PaymentError, PreparedPayment},
};
use std::future::Future;
use zcash_protocol::memo::MemoBytes;
#[async_trait::async_trait]
pub(crate) trait PaymentLifecycle: Sync {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission>;
    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()>;
    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity>;
    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity>;
    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
    ) -> anyhow::Result<bool> {
        let _ = (activity, error);
        Ok(false)
    }
}

#[async_trait::async_trait]
impl PaymentLifecycle for AppState {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        submit_prepared(&self.0.store, self, activity).await
    }

    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        self.0.store.record_error(id, "preparing", "", error)?;
        Ok(())
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

    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
    ) -> anyhow::Result<bool> {
        if activity.status != "preparing"
            || !activity.txid.is_empty()
            || !can_fail_insufficient_send(self, &activity.id).await
        {
            return Ok(false);
        }
        self.0.store.fail(&activity.id, error)?;
        Ok(true)
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
            "confirmed" | "failed" => return Ok(pending),
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
                        if pending.kind == "send"
                            && is_insufficient_funds(&error)
                            && runtime
                                .fail_insufficient_send(&pending, &error.to_string())
                                .await?
                        {
                            // Preserve the current POST error response while
                            // retaining a terminal, replayable operation.
                            return Err(error);
                        }
                        runtime.record_preparation_error(&pending.id, &error.to_string())?;
                        return Err(error);
                    }
                };
                pending = runtime.record_prepared(&pending.id, &prepared)?;
            }
            status => anyhow::bail!("payment has unsupported status {status}"),
        }
    }
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

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
pub(crate) fn parse_payment_memo(
    memo: Option<&str>,
    destination_pool: &str,
) -> Result<Option<MemoBytes>, String> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "ironwood" {
        return Err(PaymentError::TransparentMemo.to_string());
    }
    // Text memos are zero-padded to 512 bytes, so a trailing NUL could not be
    // told apart from padding and would be silently lost on decode.
    if text.ends_with('\0') {
        return Err("memo must not end with a NUL (U+0000) character".into());
    }
    MemoBytes::from_bytes(text.as_bytes())
        .map(Some)
        .map_err(|error| format!("invalid memo: {error}"))
}
