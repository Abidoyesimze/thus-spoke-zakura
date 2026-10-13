use crate::{
    db::IdempotencyConflict, mining::MiningAdmissionError, rpc::RpcError, wallet::PaymentError,
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
pub(crate) fn not_found(error: anyhow::Error, message: &str) -> ApiError {
    match error.downcast_ref::<RpcError>().map(|error| error.code) {
        Some(-5 | -8) => ApiError {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        },
        _ => error.into(),
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;
#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}
impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}
impl From<MiningAdmissionError> for ApiError {
    fn from(error: MiningAdmissionError) -> Self {
        let status = match error {
            MiningAdmissionError::InvalidBlocks | MiningAdmissionError::InvalidKey => {
                StatusCode::BAD_REQUEST
            }
            MiningAdmissionError::KeyConflict | MiningAdmissionError::Busy => StatusCode::CONFLICT,
            MiningAdmissionError::Capacity => StatusCode::SERVICE_UNAVAILABLE,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let status = if error.downcast_ref::<IdempotencyConflict>().is_some() {
            StatusCode::CONFLICT
        } else {
            match error.downcast_ref::<PaymentError>() {
                Some(PaymentError::TreasuryExhausted) => StatusCode::SERVICE_UNAVAILABLE,
                Some(PaymentError::InsufficientFunds { .. }) => StatusCode::UNPROCESSABLE_ENTITY,
                Some(PaymentError::TransparentMemo) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            }
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %format_args!("{error:#}"), "request failed");
        }
        Self {
            status,
            message: error.to_string(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"message":self.message,"status":self.status.as_u16()}})),
        )
            .into_response()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/error/tests.rs"]
pub(crate) mod tests;

pub(super) fn require_started(state: &crate::state::AppState) -> ApiResult<()> {
    if state.started() {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "the environment is still starting".into(),
        })
    }
}
