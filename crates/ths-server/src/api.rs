use crate::state::AppState;
use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
};
use std::path::PathBuf;
use tower_http::trace::TraceLayer;
mod dashboard;
pub(crate) mod error;
mod events;
pub(crate) mod explorer;
pub(crate) mod mining;
mod payments;
mod status;

pub fn router(state: AppState) -> Router {
    let static_dir = std::env::var("THS_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("web/dist"));
    Router::new()
        .route("/api/v1/health", get(status::health))
        .route("/api/v1/status", get(status::status))
        .route("/api/v1/accounts", get(status::accounts))
        .route("/api/v1/activity", get(status::activity))
        .route("/api/v1/send", post(payments::send))
        .route("/api/v1/zip321/parse", post(payments::parse_payment_uri))
        .route("/api/v1/zip321/request", post(payments::payment_request))
        .route("/api/v1/send/quote", post(payments::send_quote))
        .route("/api/v1/faucet", post(payments::faucet))
        .route(
            "/api/v1/faucet/address",
            get(payments::faucet_address_status).post(payments::faucet_address),
        )
        .route("/api/v1/mine", post(mining::mine))
        .route(
            "/api/v1/mining/jobs",
            get(mining::latest_mining_job).post(mining::start_mining_job),
        )
        .route("/api/v1/mining/jobs/{id}", get(mining::mining_job))
        .route("/api/v1/dev/seed", post(status::seed))
        .route("/api/v1/blocks", get(explorer::blocks))
        .route("/api/v1/blocks/{id}", get(explorer::block))
        .route("/api/v1/transactions/{txid}", get(explorer::transaction))
        .route("/api/v1/mempool", get(explorer::mempool))
        .route("/api/v1/addresses/{address}", get(explorer::address))
        .route("/api/v1/search", get(explorer::search))
        .route("/api/v1/events", get(events::events))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback_service(dashboard::dashboard_router(static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn method_not_allowed() -> error::ApiError {
    error::ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        message: "method is not allowed".into(),
    }
}
