use super::{
    execution::{execute_payment, parse_payment_memo, record_prepared_payment},
    treasury::{
        prepare_with_replenishment, record_prepared_address_payment, resume_address_payment,
    },
};
use crate::{
    db::{Activity, AddressFaucet, Store, TREASURY_ACCOUNT_ID},
    rpc::ChainInfo,
    state::AppState,
    wallet::{PaymentError, PreparedPayment},
};
use anyhow::Context;
use std::time::Duration;
use tokio::time::Instant;

pub(super) const PAYMENT_DISPATCH_BUDGET: Duration = Duration::from_secs(10);
pub(super) const PAYMENT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn bounded_payment_chain_info(state: &AppState) -> anyhow::Result<ChainInfo> {
    bounded_payment_chain_info_until(state, Instant::now() + PAYMENT_LOOKUP_TIMEOUT).await
}

async fn bounded_payment_chain_info_until(
    state: &AppState,
    deadline: Instant,
) -> anyhow::Result<ChainInfo> {
    let lookup_deadline = deadline.min(Instant::now() + PAYMENT_LOOKUP_TIMEOUT);
    tokio::time::timeout_at(lookup_deadline, state.0.rpc.chain_info())
        .await
        .context("payment progression chain lookup timed out")?
}

pub(super) async fn bounded_payment_transaction_known(
    state: &AppState,
    txid: &str,
) -> anyhow::Result<bool> {
    tokio::time::timeout(PAYMENT_LOOKUP_TIMEOUT, state.0.rpc.transaction_known(txid))
        .await
        .context("payment progression transaction lookup timed out")?
}

pub(super) async fn progress_payment_intents(
    state: &AppState,
    deadline: Instant,
    activity_cursor: &mut Option<String>,
    address_cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    let mut activities = state.0.store.unresolved_activities()?;
    activities.sort_by(|left, right| left.activity.id.cmp(&right.activity.id));
    let mut address_faucets: Vec<crate::db::AddressFaucetIntent> =
        state.0.store.unresolved_address_faucets()?;
    address_faucets.sort_by(|left, right| left.payment.id.cmp(&right.payment.id));
    if activities.is_empty() {
        *activity_cursor = None;
    }
    if address_faucets.is_empty() {
        *address_cursor = None;
    }
    let activity_start = activity_cursor.as_ref().map_or(0, |id| {
        activities.partition_point(|intent| intent.activity.id.as_str() <= id.as_str())
            % activities.len().max(1)
    });
    let address_start = address_cursor.as_ref().map_or(0, |id| {
        address_faucets.partition_point(|intent| intent.payment.id.as_str() <= id.as_str())
            % address_faucets.len().max(1)
    });

    // Alternate between the internal and external queues. A slow internal
    // lookup must not consume the whole dispatch budget before external
    // faucets get a turn.
    for offset in 0..activities.len().max(address_faucets.len()) {
        if Instant::now() >= deadline {
            break;
        }
        if offset < activities.len()
            && let Some(intent) = activities.get((activity_start + offset) % activities.len())
        {
            let _payment = state.0.payments.lock().await;
            if Instant::now() >= deadline {
                break;
            }
            *activity_cursor = Some(intent.activity.id.clone());
            if let Some(activity) = state.0.store.activity_for_key(&intent.key)?
                && !matches!(activity.status.as_str(), "confirmed" | "failed")
                && let Err(error) =
                    progress_activity_until(state, intent, activity.clone(), deadline).await
            {
                // Progression may have persisted one or more state changes
                // before failing. Reload before applying the guarded error
                // update so it is associated with the current state and txid.
                record_current_activity_error(&state.0.store, &intent.key, &error.to_string())?;
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    activity = %activity.id,
                    "accepted payment remains pending"
                );
            }
        }

        if Instant::now() >= deadline {
            break;
        }
        if offset < address_faucets.len()
            && let Some(intent) =
                address_faucets.get((address_start + offset) % address_faucets.len())
        {
            let _payment = state.0.payments.lock().await;
            if Instant::now() >= deadline {
                break;
            }
            *address_cursor = Some(intent.payment.id.clone());
            if let Some(payment) = state.0.store.address_faucet_for_key(&intent.key)?
                && !matches!(payment.status.as_str(), "confirmed" | "failed")
                && let Err(error) = progress_address_faucet(state, payment.clone(), deadline).await
            {
                record_current_address_error(&state.0.store, &intent.key, &error.to_string())?;
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    payment = %payment.id,
                    "accepted address faucet remains pending"
                );
            }
        }
    }
    Ok(())
}

fn record_current_activity_error(store: &Store, key: &str, error: &str) -> anyhow::Result<()> {
    if let Some(current) = store.activity_for_key(key)? {
        store.record_error(&current.id, &current.status, &current.txid, error)?;
    }
    Ok(())
}

fn record_current_address_error(store: &Store, key: &str, error: &str) -> anyhow::Result<()> {
    if let Some(current) = store.address_faucet_for_key(key)? {
        store.record_address_error(&current.id, &current.status, &current.txid, error)?;
    }
    Ok(())
}

pub(super) fn is_insufficient_funds(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<PaymentError>(),
        Some(PaymentError::InsufficientFunds { .. })
    )
}

pub(super) async fn can_fail_insufficient_send(state: &AppState, activity_id: &str) -> bool {
    // The caller holds the payment mutex. Reading the wallet journal also
    // waits behind any preparation worker that already owns the wallet DB.
    let wallet_prepared = match state.0.wallet.prepared(activity_id).await {
        Ok(prepared) => prepared,
        Err(_) => return false,
    };
    let store_expiry = match state.0.store.prepared_transaction_expiry(activity_id) {
        Ok(expiry) => expiry,
        Err(_) => return false,
    };
    match wallet_prepared {
        None => store_expiry.is_none(),
        Some(prepared) if prepared.expiry_height != 0 => {
            let current_height = match bounded_payment_chain_info(state).await {
                Ok(info) => info.blocks,
                Err(_) => return false,
            };
            if current_height < prepared.expiry_height
                || store_expiry.is_some_and(|expiry| expiry == 0 || current_height < expiry)
            {
                return false;
            }
            matches!(
                bounded_payment_transaction_known(state, &prepared.txid).await,
                Ok(false)
            )
        }
        Some(_) => false,
    }
}

async fn repair_activity_journal(
    state: &AppState,
    activity: &Activity,
    deadline: Instant,
) -> anyhow::Result<Option<PreparedPayment>> {
    let Some(prepared) = state.0.wallet.prepared(&activity.id).await? else {
        return Ok(None);
    };
    if prepared.expiry_height == 0
        || bounded_payment_chain_info_until(state, deadline)
            .await?
            .blocks
            < prepared.expiry_height
    {
        return Ok(Some(prepared));
    }
    Ok(None)
}

async fn prepare_activity_intent(
    state: &AppState,
    intent: &crate::db::ActivityIntent,
    deadline: Instant,
) -> anyhow::Result<PreparedPayment> {
    let activity = &intent.activity;
    let sync_deadline = (Instant::now() + PAYMENT_LOOKUP_TIMEOUT).min(deadline);
    state.synchronize_wallet_until(None, sync_deadline).await?;
    let seed = state.0.store.seed()?;
    let record = record_prepared_payment(&state.0.store, &activity.id);
    match activity.kind.as_str() {
        "send" => {
            let account_id = activity
                .from_account
                .context("send omitted source account")?;
            let destination = if let Some(address) = &activity.to_address {
                address.clone()
            } else {
                let account = state
                    .0
                    .store
                    .account(activity.to_account.context("send omitted destination")?)?;
                match activity.destination_pool.as_str() {
                    "transparent" => account.transparent_address,
                    "ironwood" => account.unified_address,
                    pool => anyhow::bail!("send has unsupported destination pool {pool}"),
                }
            };
            let memo = parse_payment_memo(intent.memo.as_deref(), &activity.destination_pool)
                .map_err(anyhow::Error::msg)?;
            state
                .0
                .wallet
                .prepare(
                    Some(&activity.id),
                    &seed,
                    account_id,
                    &activity.source_pool,
                    &destination,
                    activity.amount_zatoshi,
                    memo,
                    record,
                )
                .await
        }
        "faucet" => {
            let account = state.0.store.account(TREASURY_ACCOUNT_ID)?;
            let destination = state.0.store.account(
                activity
                    .to_account
                    .context("faucet omitted destination account")?,
            )?;
            let address = match activity.destination_pool.as_str() {
                "transparent" => destination.transparent_address,
                "ironwood" => destination.unified_address,
                pool => anyhow::bail!("faucet has unsupported destination pool {pool}"),
            };
            let _replenishment = state.0.treasury_replenishment.lock().await;
            prepare_with_replenishment(
                state,
                Some(&activity.id),
                &seed,
                &account,
                &address,
                activity.amount_zatoshi,
                record,
            )
            .await
        }
        kind => anyhow::bail!("unsupported accepted payment kind {kind}"),
    }
}

async fn repair_address_journal(
    state: &AppState,
    payment: &AddressFaucet,
    deadline: Instant,
) -> anyhow::Result<Option<PreparedPayment>> {
    let Some(prepared) = state.0.wallet.prepared(&payment.id).await? else {
        return Ok(None);
    };
    if prepared.expiry_height == 0
        || bounded_payment_chain_info_until(state, deadline)
            .await?
            .blocks
            < prepared.expiry_height
    {
        return Ok(Some(prepared));
    }
    Ok(None)
}

#[cfg(test)]
async fn progress_activity(
    state: &AppState,
    intent: &crate::db::ActivityIntent,
    activity: Activity,
) -> anyhow::Result<()> {
    progress_activity_until(
        state,
        intent,
        activity,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
    )
    .await
}

async fn progress_activity_until(
    state: &AppState,
    intent: &crate::db::ActivityIntent,
    activity: Activity,
    deadline: Instant,
) -> anyhow::Result<()> {
    let result = execute_payment(state, activity.clone(), |_| async {
        if let Some(prepared) = repair_activity_journal(state, &activity, deadline).await? {
            return Ok(prepared);
        }
        prepare_activity_intent(state, intent, deadline).await
    })
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            if state
                .0
                .store
                .activity_for_key(&intent.key)?
                .is_some_and(|current| current.status == "failed")
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

async fn progress_address_faucet(
    state: &AppState,
    payment: AddressFaucet,
    deadline: Instant,
) -> anyhow::Result<()> {
    resume_address_payment(state, payment.clone(), |activity_id| {
        let payment = &payment;
        async move {
            if let Some(prepared) = repair_address_journal(state, payment, deadline).await? {
                return Ok(prepared);
            }
            let sync_deadline = (Instant::now() + PAYMENT_LOOKUP_TIMEOUT).min(deadline);
            state.synchronize_wallet_until(None, sync_deadline).await?;
            let seed = state.0.store.seed()?;
            let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
            let _replenishment = state.0.treasury_replenishment.lock().await;
            prepare_with_replenishment(
                state,
                Some(&activity_id),
                &seed,
                &treasury,
                &payment.address,
                payment.amount_zatoshi,
                record_prepared_address_payment(&state.0.store, &activity_id),
            )
            .await
        }
    })
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/payments/progression/tests.rs"]
mod tests;
