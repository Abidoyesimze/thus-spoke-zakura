use crate::state::{AppState, notify};
use crate::{
    db::{Activity, AddressFaucet, Store},
    sync::mine_and_sync,
};
use serde_json::Value;
use std::{future::Future, time::Duration};
use tokio::time::Instant;
pub(crate) async fn confirm_address_after_mining(
    state: &AppState,
    pending: AddressFaucet,
) -> anyhow::Result<AddressFaucet> {
    let check = |payment: AddressFaucet, tx: Value| -> anyhow::Result<AddressFaucet> {
        match confirmed_block_hash(&tx) {
            Some(hash) => state
                .0
                .store
                .confirm_address(&payment.id, &payment.txid, hash),
            None => Ok(payment),
        }
    };
    let pending = match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => check(pending, tx)?,
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not check address faucet confirmation"
            );
            pending
        }
    };
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    if let Err(error) = mine_and_sync(state, 1).await {
        tracing::warn!(
            error = %format_args!("{error:#}"),
            payment = %pending.id,
            "address faucet recorded but auto-mine failed"
        );
        return Ok(pending);
    }
    match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => check(pending, tx),
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not check address faucet confirmation"
            );
            Ok(pending)
        }
    }
}

pub(crate) fn confirmed_block_hash(tx: &Value) -> Option<&str> {
    let confirmations = tx.get("confirmations").and_then(Value::as_u64).unwrap_or(0);
    if confirmations == 0 {
        return None;
    }
    tx.get("blockhash")
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
}

fn apply_confirmation(store: &Store, pending: &Activity, tx: &Value) -> anyhow::Result<Activity> {
    match confirmed_block_hash(tx) {
        Some(hash) => store.confirm(&pending.id, &pending.txid, hash),
        None => Ok(pending.clone()),
    }
}

async fn confirm_from_chain(state: &AppState, pending: Activity) -> anyhow::Result<Activity> {
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => {
            let updated = apply_confirmation(&state.0.store, &pending, &tx)?;
            if updated.status == "confirmed" {
                notify(state, "wallet");
            }
            Ok(updated)
        }
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not fetch transaction for confirmation"
            );
            Ok(pending)
        }
    }
}

async fn reconcile_unconfirmed_with<F, Fut>(
    store: &Store,
    cursor: &mut Option<String>,
    deadline: Instant,
    mut lookup: F,
) -> anyhow::Result<bool>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<Value>>>,
{
    let mut activities = store.unconfirmed_activities()?;
    if activities.is_empty() {
        *cursor = None;
        return Ok(false);
    }
    // Stable ID ordering retains the resume point even if the last attempted
    // record was confirmed and has disappeared from the unresolved set.
    activities.sort_by(|left, right| left.id.cmp(&right.id));
    let start = cursor.as_ref().map_or(0, |id| {
        activities.partition_point(|activity| activity.id.as_str() <= id.as_str())
            % activities.len()
    });
    let mut changed = false;
    for offset in 0..activities.len() {
        if Instant::now() >= deadline {
            break;
        }
        let activity = &activities[(start + offset) % activities.len()];
        // Advance even after an uncertain lookup, so an outage for one record
        // cannot monopolize every subsequent bounded pass.
        *cursor = Some(activity.id.clone());
        let attempt_deadline = deadline.min(Instant::now() + PAYMENT_LOOKUP_TIMEOUT);
        let result: anyhow::Result<bool> = async {
            let transaction = crate::reconcile::within_deadline(
                attempt_deadline,
                "checking payment confirmation",
                lookup(activity.txid.clone()),
            )
            .await?;
            if let Some(transaction) = transaction {
                let updated = apply_confirmation(store, activity, &transaction)?;
                Ok(updated.status == "confirmed")
            } else {
                Ok(false)
            }
        }
        .await;
        match result {
            Ok(confirmed) => changed |= confirmed,
            Err(error) => tracing::warn!(
                error = %format_args!("{error:#}"),
                activity = %activity.id,
                txid = %activity.txid,
                "payment confirmation remains pending"
            ),
        }
    }
    Ok(changed)
}

pub(crate) async fn confirm_after_mining(
    state: &AppState,
    pending: Activity,
) -> anyhow::Result<Activity> {
    let pending = confirm_from_chain(state, pending).await?;
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    match mine_and_sync(state, 1).await {
        Ok(_) => confirm_from_chain(state, pending).await,
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                activity = %pending.id,
                "transaction recorded but auto-mine failed"
            );
            Ok(pending)
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/payments/confirmation/tests.rs"]
pub(crate) mod tests;

const PAYMENT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const PAYMENT_DISPATCH_BUDGET: Duration = Duration::from_secs(10);
const PAYMENT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) async fn payment_confirmation_loop(state: AppState) {
    let mut cursor = None;
    let mut interval = tokio::time::interval(PAYMENT_RETRY_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let deadline = Instant::now() + PAYMENT_DISPATCH_BUDGET;
        let rpc = &state.0.rpc;
        match reconcile_unconfirmed_with(&state.0.store, &mut cursor, deadline, |txid| async move {
            rpc.lookup_transaction(&txid).await
        })
        .await
        {
            Ok(true) => notify(&state, "wallet"),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                error = %format_args!("{error:#}"),
                "background payment confirmation failed"
            ),
        }
    }
}
