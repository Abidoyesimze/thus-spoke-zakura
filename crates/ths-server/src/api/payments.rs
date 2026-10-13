use super::error::require_started;
use super::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::{
    db::{Activity, AddressFaucet, Recipient, Store, USER_ACCOUNT_COUNT, ZATOSHIS_PER_ZEC},
    mining::validate_idempotency_key,
    network::regtest_network,
    payments::{
        execution::{execute_payment, parse_payment_memo, record_prepared_payment},
        treasury::{fund_address_from_treasury, fund_from_treasury},
    },
    wallet::SendQuote,
};
use anyhow::Context;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use zcash_keys::address::Address;
use zcash_protocol::{
    memo::{Memo, MemoBytes},
    value::{MAX_MONEY, Zatoshis},
};
use zip321::{Payment, TransactionRequest};
#[derive(Deserialize)]
/// Exactly one of `to_account` or `to_address` names the destination.
pub(crate) struct SendRequest {
    from_account: u8,
    to_account: Option<u8>,
    to_address: Option<String>,
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
    require_started(&state)?;
    let (recipient, memo) = validate_send(&req)?;
    let address = match recipient {
        Recipient::Account(id) => {
            let destination = state.0.store.account(id)?;
            match req.destination_pool.as_str() {
                "transparent" => destination.transparent_address,
                "ironwood" => destination.unified_address,
                _ => unreachable!("validate_send checks destination_pool"),
            }
        }
        Recipient::Address(address) => address.to_owned(),
    };
    // A pasted address that belongs to a development account is recorded as
    // that account, before the claim, so a replay resolves to the same payment.
    let recipient = match recipient {
        Recipient::Address(address) => {
            development_account_for(&state.0.store, address)?.map_or(recipient, Recipient::Account)
        }
        recipient => recipient,
    };
    if recipient == Recipient::Account(req.from_account) && req.source_pool == req.destination_pool
    {
        return Err(ApiError::bad_request(
            "choose a different account or a different destination pool",
        ));
    }
    let _payment = state.0.payments.lock().await;
    let pending = state.0.store.claim_transfer(
        req.from_account,
        recipient,
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
            let address = &address;
            async move {
                state.synchronize_latest().await?;
                state
                    .0
                    .wallet
                    .prepare(
                        Some(&activity_id),
                        &state.0.store.seed()?,
                        req.from_account,
                        &req.source_pool,
                        address,
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
pub(crate) struct PaymentUriRequest {
    uri: String,
}

/// A single-payment ZIP-321 request, resolved for the Send form.
#[derive(Debug, Serialize)]
pub(crate) struct PaymentUri {
    address: String,
    destination_pool: &'static str,
    /// Set when the address belongs to a development account.
    to_account: Option<u8>,
    amount_zatoshi: Option<u64>,
    memo: Option<String>,
}

pub(crate) async fn parse_payment_uri(
    State(state): State<AppState>,
    Json(req): Json<PaymentUriRequest>,
) -> ApiResult<Json<PaymentUri>> {
    Ok(Json(payment_uri(&state.0.store, &req.uri)?))
}

fn payment_uri(store: &Store, uri: &str) -> ApiResult<PaymentUri> {
    let request = TransactionRequest::from_uri(uri.trim()).map_err(|error| {
        tracing::debug!(%error, "rejected ZIP-321 URI");
        ApiError::bad_request("Invalid payment URI")
    })?;
    let mut payments = request.payments().values();
    let (Some(payment), None) = (payments.next(), payments.next()) else {
        return Err(ApiError::bad_request(
            "only single-payment URIs are supported",
        ));
    };
    let address = payment.recipient_address().encode();
    let destination_pool = require_send_address(&address)?;
    let memo = match payment.memo().map(Memo::try_from).transpose() {
        Ok(None | Some(Memo::Empty)) => None,
        Ok(Some(Memo::Text(text))) => Some(String::from(&*text)),
        _ => return Err(ApiError::bad_request("only text memos are supported")),
    };
    Ok(PaymentUri {
        to_account: development_account_for(store, &address)?,
        address,
        destination_pool,
        amount_zatoshi: payment.amount().map(u64::from),
        memo,
    })
}

fn development_account_for(store: &Store, address: &str) -> anyhow::Result<Option<u8>> {
    for id in 1..=USER_ACCOUNT_COUNT {
        let account = store.account(id)?;
        if account.unified_address == address || account.transparent_address == address {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
pub(crate) struct PaymentRequest {
    account_id: u8,
    amount_zatoshi: Option<u64>,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PaymentRequestResponse {
    uri: String,
}

pub(crate) async fn payment_request(
    State(state): State<AppState>,
    Json(req): Json<PaymentRequest>,
) -> ApiResult<Json<PaymentRequestResponse>> {
    require_user_account(req.account_id)?;
    let amount = req
        .amount_zatoshi
        .map(|amount| {
            if amount == 0 {
                return Err(ApiError::bad_request("amount must be greater than zero"));
            }
            Zatoshis::from_u64(amount).map_err(|_| {
                ApiError::bad_request(format!("amount_zatoshi must not exceed {MAX_MONEY}"))
            })
        })
        .transpose()?;
    let memo = parse_memo(req.memo.as_deref(), "ironwood")?;
    let account = state.0.store.account(req.account_id)?;
    let address = Address::decode(&regtest_network(), &account.unified_address)
        .context("stored account has an invalid unified address")?;
    let payment = Payment::new(
        address.to_zcash_address(&regtest_network()),
        amount,
        memo,
        None,
        None,
        vec![],
    )
    .context("constructing payment request")?;
    let uri = TransactionRequest::new(vec![payment])
        .context("constructing payment request")?
        .to_uri();
    Ok(Json(PaymentRequestResponse { uri }))
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
    require_started(&state)?;
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
    last_error: Option<String>,
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
    address: String,
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
        address,
        amount_zatoshi: activity.amount_zatoshi,
        txid: activity.txid,
        block_hash,
        status: activity.status,
        last_error: activity.last_error,
    })
}

fn external_faucet_response(payment: AddressFaucet) -> FaucetAddressResponse {
    FaucetAddressResponse {
        address: payment.address,
        amount_zatoshi: payment.amount_zatoshi,
        txid: payment.txid,
        block_hash: payment.block_hash,
        status: payment.status,
        last_error: payment.last_error,
    }
}

fn stored_address_faucet(
    store: &Store,
    key: &str,
) -> anyhow::Result<Option<FaucetAddressResponse>> {
    if let Some(payment) = store.address_faucet_for_key(key)? {
        return Ok(Some(external_faucet_response(payment)));
    }
    let Some(activity) = store
        .activity_for_key(key)?
        .filter(|activity| activity.kind == "faucet")
    else {
        return Ok(None);
    };
    let account = store.account(
        activity
            .to_account
            .context("internal faucet omitted its account")?,
    )?;
    let address = match activity.destination_pool.as_str() {
        "transparent" => account.transparent_address,
        "ironwood" => account.unified_address,
        pool => anyhow::bail!("faucet has unsupported destination pool {pool}"),
    };
    faucet_address_response(address, activity).map(Some)
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
        Ok(external_faucet_response(payment))
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
        Ok(Json(faucet_address_response(req.address, activity)?))
    } else {
        Ok(Json(runtime.fund_external(req).await?))
    }
}

pub(crate) async fn faucet_address(
    State(state): State<AppState>,
    Json(req): Json<FaucetAddressRequest>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_started(&state)?;
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
fn validate_send(req: &SendRequest) -> ApiResult<(Recipient<'_>, Option<MemoBytes>)> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.from_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    let recipient = match (req.to_account, req.to_address.as_deref()) {
        (Some(to_account), None) => {
            require_user_account(to_account)?;
            if req.from_account == to_account && req.source_pool == req.destination_pool {
                return Err(ApiError::bad_request(
                    "choose a different account or a different destination pool",
                ));
            }
            Recipient::Account(to_account)
        }
        (None, Some(address)) => {
            if require_send_address(address)? != req.destination_pool {
                return Err(ApiError::bad_request(
                    "destination_pool does not match the destination address",
                ));
            }
            Recipient::Address(address)
        }
        _ => {
            return Err(ApiError::bad_request(
                "provide exactly one of to_account or to_address",
            ));
        }
    };
    if req.amount_zatoshi == 0 || req.amount_zatoshi > MAX_MONEY {
        return Err(ApiError::bad_request(format!(
            "amount_zatoshi must be between 1 and {MAX_MONEY}"
        )));
    }
    let memo = parse_memo(req.memo.as_deref(), &req.destination_pool)?;
    Ok((recipient, memo))
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
fn parse_memo(memo: Option<&str>, destination_pool: &str) -> ApiResult<Option<MemoBytes>> {
    parse_payment_memo(memo, destination_pool).map_err(ApiError::bad_request)
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

/// Returns the pool a payment to `value` lands in. Ironwood shares the Orchard
/// receiver, so a unified address needs one to receive shielded funds.
fn require_send_address(value: &str) -> ApiResult<&'static str> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Unified(address)) if address.has_orchard() => Ok("ironwood"),
        Some(Address::Transparent(_)) => Ok("transparent"),
        Some(Address::Unified(_)) => Err(ApiError::bad_request(
            "unified destination must include an Ironwood receiver",
        )),
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

#[derive(Deserialize)]
pub(crate) struct FaucetAddressQuery {
    idempotency_key: String,
}

pub(crate) async fn faucet_address_status(
    State(state): State<AppState>,
    Query(query): Query<FaucetAddressQuery>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_key(&query.idempotency_key)?;
    stored_address_faucet(&state.0.store, &query.idempotency_key)?
        .map(Json)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "no address faucet uses that idempotency key".into(),
        })
}
