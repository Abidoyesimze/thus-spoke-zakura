use super::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::{
    db::{Account, Activity, USER_ACCOUNT_COUNT},
    rpc::ChainInfo,
    state::WalletSyncStatus,
};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::HOST},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub(crate) async fn health(State(state): State<AppState>) -> Response {
    let node = state.0.rpc.chain_info().await.ok();
    let wallet = state.wallet_sync_status().await;
    let status = if node.is_some() && wallet.last_success_at.is_some() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({"ok": node.is_some() && wallet.last_success_at.is_some(), "instance": state.0.instance, "node": node, "wallet_sync": wallet})),
    )
        .into_response()
}

#[derive(Serialize)]
pub(crate) struct Status {
    instance: String,
    node: Option<ChainInfo>,
    account_count: usize,
    auto_mine: bool,
    network: &'static str,
    endpoints: PublicEndpoints,
    wallet_sync: WalletSyncStatus,
}

#[derive(Serialize)]
pub(crate) struct PublicEndpoints {
    dashboard: String,
    zakura_rpc: String,
    lightwalletd: String,
    p2p: String,
}

pub(crate) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Status>> {
    let dashboard_host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    Ok(Json(Status {
        instance: state.0.instance.clone(),
        node: state.0.rpc.chain_info().await.ok(),
        account_count: usize::from(USER_ACCOUNT_COUNT),
        auto_mine: true,
        network: "Regtest",
        endpoints: PublicEndpoints {
            dashboard: format!("http://{dashboard_host}"),
            zakura_rpc: std::env::var("THS_PUBLIC_ZAKURA_RPC")
                .unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
            lightwalletd: std::env::var("THS_PUBLIC_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
            p2p: std::env::var("THS_PUBLIC_P2P").unwrap_or_else(|_| "127.0.0.1:18233".into()),
        },
        wallet_sync: state.wallet_sync_status().await,
    }))
}
pub(crate) async fn accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<Account>>> {
    let mut accounts = state.accounts().await;
    accounts.retain(|account| account.id <= USER_ACCOUNT_COUNT);
    Ok(Json(accounts))
}

#[derive(Deserialize)]
pub(crate) struct SeedRequest {
    confirmation: String,
}
pub(crate) async fn seed(
    State(state): State<AppState>,
    Json(req): Json<SeedRequest>,
) -> ApiResult<Json<Value>> {
    if req.confirmation != "I understand this seed is for regtest only" {
        return Err(ApiError::bad_request("exact confirmation phrase required"));
    }
    Ok(Json(
        json!({"seed_hex":state.0.store.seed()?,"warning":"Never send real funds to this development seed."}),
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/api/status/tests.rs"]
pub(crate) mod tests;

#[derive(Deserialize)]
pub(crate) struct Page {
    limit: Option<u32>,
}
pub(crate) async fn activity(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<Activity>>> {
    Ok(Json(state.0.store.activities(page.limit.unwrap_or(30))?))
}
