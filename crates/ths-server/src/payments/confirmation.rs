use crate::state::{AppState, notify};
use crate::{
    db::{Activity, AddressFaucet, Store},
    sync::mine_and_sync,
};
use serde_json::Value;
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

pub(crate) async fn reconcile_unconfirmed(state: &AppState) -> anyhow::Result<()> {
    for activity in state.0.store.unconfirmed_activities()? {
        confirm_from_chain(state, activity).await?;
    }
    Ok(())
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
