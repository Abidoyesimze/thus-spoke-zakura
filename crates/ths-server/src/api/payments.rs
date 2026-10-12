use super::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::{
    db::{Activity, Store, USER_ACCOUNT_COUNT, ZATOSHIS_PER_ZEC},
    mining::validate_idempotency_key,
    network::regtest_network,
    payments::{
        execution::{execute_payment, record_prepared_payment},
        treasury::{fund_address_from_treasury, fund_from_treasury},
    },
    wallet::{PaymentError, SendQuote},
};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use zcash_keys::address::Address;
use zcash_protocol::{memo::MemoBytes, value::MAX_MONEY};
#[derive(Deserialize)]
pub(crate) struct SendRequest {
    from_account: u8,
    to_account: u8,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
    #[serde(default)]
    memo: Option<String>,
}
pub(crate) async fn send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> ApiResult<Json<Activity>> {
    let memo = validate_send(&req)?;
    let _payment = state.0.payments.lock().await;
    let pending = state.0.store.claim_transfer(
        req.from_account,
        req.to_account,
        &req.source_pool,
        &req.destination_pool,
        req.amount_zatoshi,
        &req.idempotency_key,
        req.memo.as_deref(),
    )?;
    Ok(Json(
        execute_payment(&state, pending, |activity_id| {
            let state = &state;
            let memo = memo.clone();
            let req = &req;
            async move {
                state.synchronize_latest().await?;
                let destination = state.0.store.account(req.to_account)?;
                let address = match req.destination_pool.as_str() {
                    "transparent" => destination.transparent_address,
                    "ironwood" => destination.unified_address,
                    _ => unreachable!("claim_transfer validates destination_pool"),
                };
                state
                    .0
                    .wallet
                    .prepare(
                        Some(&activity_id),
                        &state.0.store.seed()?,
                        req.from_account,
                        &req.source_pool,
                        &address,
                        req.amount_zatoshi,
                        memo,
                        record_prepared_payment(&state.0.store, &activity_id),
                    )
                    .await
            }
        })
        .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct SendQuoteRequest {
    from_account: u8,
    source_pool: String,
    destination_pool: String,
}
pub(crate) async fn send_quote(
    State(state): State<AppState>,
    Json(req): Json<SendQuoteRequest>,
) -> ApiResult<Json<SendQuote>> {
    require_user_account(req.from_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    // The fee depends only on the destination's receiver kinds, which every
    // account shares, so the source account's own address quotes the same fee.
    let destination = state.0.store.account(req.from_account)?;
    let address = if req.destination_pool == "transparent" {
        destination.transparent_address
    } else {
        destination.unified_address
    };
    Ok(Json(
        state
            .0
            .wallet
            .send_quote(req.from_account, &req.source_pool, &address)
            .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct FaucetRequest {
    account_id: u8,
    pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
}
pub(crate) async fn faucet(
    State(state): State<AppState>,
    Json(req): Json<FaucetRequest>,
) -> ApiResult<Json<Activity>> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.account_id)?;
    if req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "a faucet request is limited to 5 ZEC",
        ));
    }
    if req.amount_zatoshi == 0 {
        return Err(ApiError::bad_request("amount must be greater than zero"));
    }
    require_pool(&req.pool, "pool")?;
    Ok(Json(
        fund_from_treasury(
            &state,
            req.account_id,
            &req.pool,
            req.amount_zatoshi,
            &req.idempotency_key,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct FaucetAddressRequest {
    address: String,
    amount_zatoshi: u64,
    #[serde(default)]
    idempotency_key: String,
}

#[derive(Serialize)]
pub(crate) struct FaucetAddressResponse {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

fn internal_faucet_destination(
    store: &Store,
    address: &str,
) -> anyhow::Result<Option<(u8, &'static str)>> {
    for account in store.accounts()? {
        if !(1..=USER_ACCOUNT_COUNT).contains(&account.id) {
            continue;
        }
        if account.transparent_address == address {
            return Ok(Some((account.id, "transparent")));
        }
        if account.unified_address == address {
            return Ok(Some((account.id, "ironwood")));
        }
    }
    Ok(None)
}

fn faucet_address_response(
    req: FaucetAddressRequest,
    activity: Activity,
) -> anyhow::Result<FaucetAddressResponse> {
    let block_hash = activity.block_hash.filter(|hash| !hash.is_empty());
    if activity.status == "confirmed" {
        anyhow::ensure!(
            block_hash.is_some(),
            "faucet transaction was not included in a block"
        );
    }
    Ok(FaucetAddressResponse {
        address: req.address,
        amount_zatoshi: req.amount_zatoshi,
        txid: activity.txid,
        block_hash,
        status: activity.status,
    })
}

#[async_trait::async_trait]
trait AddressFaucetRuntime: Sync {
    async fn fund_internal(
        &self,
        account_id: u8,
        pool: &str,
        amount_zatoshi: u64,
        key: &str,
    ) -> anyhow::Result<Activity>;
    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse>;
}

#[async_trait::async_trait]
impl AddressFaucetRuntime for AppState {
    async fn fund_internal(
        &self,
        account_id: u8,
        pool: &str,
        amount_zatoshi: u64,
        key: &str,
    ) -> anyhow::Result<Activity> {
        fund_from_treasury(self, account_id, pool, amount_zatoshi, key).await
    }

    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse> {
        let payment = fund_address_from_treasury(
            self,
            &req.address,
            req.amount_zatoshi,
            &req.idempotency_key,
        )
        .await?;
        Ok(FaucetAddressResponse {
            address: payment.address,
            amount_zatoshi: payment.amount_zatoshi,
            txid: payment.txid,
            block_hash: payment.block_hash,
            status: payment.status,
        })
    }
}

async fn execute_address_faucet<R: AddressFaucetRuntime>(
    store: &Store,
    runtime: &R,
    req: FaucetAddressRequest,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_key(&req.idempotency_key)?;
    require_faucet_address(&req.address)?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "amount must be greater than zero and no more than 5 ZEC",
        ));
    }
    if store
        .address_faucet_for_key(&req.idempotency_key)?
        .is_some()
    {
        return Ok(Json(runtime.fund_external(req).await?));
    }
    if let Some((account_id, pool)) = internal_faucet_destination(store, &req.address)? {
        let activity = runtime
            .fund_internal(account_id, pool, req.amount_zatoshi, &req.idempotency_key)
            .await?;
        Ok(Json(faucet_address_response(req, activity)?))
    } else {
        Ok(Json(runtime.fund_external(req).await?))
    }
}

pub(crate) async fn faucet_address(
    State(state): State<AppState>,
    Json(req): Json<FaucetAddressRequest>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    execute_address_faucet(&state.0.store, &state, req).await
}

pub(crate) fn require_key(key: &str) -> ApiResult<()> {
    validate_idempotency_key(key).map_err(ApiError::bad_request)
}

fn require_pool(pool: &str, field: &str) -> ApiResult<()> {
    match pool {
        "transparent" | "ironwood" => Ok(()),
        // NU6.3 is active from block 1, so the Orchard pool can neither receive
        // nor hold funds on this chain.
        "orchard" => Err(ApiError::bad_request(format!(
            "{field} orchard is unavailable: NU6.3 is active and Orchard no longer accepts deposits; use ironwood"
        ))),
        _ => Err(ApiError::bad_request(format!(
            "{field} must be transparent or ironwood"
        ))),
    }
}

/// Checks the whole send request without touching the store, wallet or node,
/// and returns the encoded memo it carries.
fn validate_send(req: &SendRequest) -> ApiResult<Option<MemoBytes>> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.from_account)?;
    require_user_account(req.to_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    if req.from_account == req.to_account && req.source_pool == req.destination_pool {
        return Err(ApiError::bad_request(
            "choose a different account or a different destination pool",
        ));
    }
    if req.amount_zatoshi == 0 || req.amount_zatoshi > MAX_MONEY {
        return Err(ApiError::bad_request(format!(
            "amount_zatoshi must be between 1 and {MAX_MONEY}"
        )));
    }
    parse_memo(req.memo.as_deref(), &req.destination_pool)
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
fn parse_memo(memo: Option<&str>, destination_pool: &str) -> ApiResult<Option<MemoBytes>> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "ironwood" {
        return Err(anyhow::Error::new(PaymentError::TransparentMemo).into());
    }
    // Text memos are zero-padded to 512 bytes, so a trailing NUL could not be
    // told apart from padding and would be silently lost on decode.
    if text.ends_with('\0') {
        return Err(ApiError::bad_request(
            "memo must not end with a NUL (U+0000) character",
        ));
    }
    MemoBytes::from_bytes(text.as_bytes())
        .map(Some)
        .map_err(|error| ApiError::bad_request(format!("invalid memo: {error}")))
}

fn require_user_account(id: u8) -> ApiResult<()> {
    if (1..=USER_ACCOUNT_COUNT).contains(&id) {
        Ok(())
    } else {
        Err(ApiError::bad_request("account must be between 1 and 5"))
    }
}

fn require_faucet_address(value: &str) -> ApiResult<()> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Unified(_) | Address::Transparent(_)) => Ok(()),
        Some(_) => Err(ApiError::bad_request(
            "destination must be a unified or transparent Regtest address",
        )),
        None => Err(ApiError::bad_request(
            "destination is not a valid Regtest address",
        )),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/payments/tests.rs"]
pub(crate) mod tests;
