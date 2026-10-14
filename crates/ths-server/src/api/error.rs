use crate::{
    db::IdempotencyConflict, mining::MiningAdmissionError, rpc::RpcError, wallet::PaymentError,
};
use axum::{
    Json,
    extract::{
        FromRequest, FromRequestParts, Request,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use serde_json::json;
use zcash_protocol::value::MAX_MONEY;
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

/// JSON body extractor whose failures use the API error envelope.
///
/// Axum's `Json` extractor rejects before the handler runs, so those responses
/// never become [`ApiError`]. A type mismatch is also 422 plain text, which the
/// dashboard cannot read. This extractor keeps Axum's parsing, returns
/// `{"error":{"message","status"}}`, and reports a type mismatch as 400.
pub(crate) struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let path = req.uri().path().to_owned();
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(parsing_rejection(rejection, &path)),
        }
    }
}

/// Query-string extractor whose failures use the API error envelope.
pub(crate) struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let path = parts.uri.path().to_owned();
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(parsing_rejection(rejection, &path)),
        }
    }
}

/// JSON type mismatches arrive as 422. A body or query that does not match the
/// handler's type is a malformed request, so the envelope reports 400.
fn parsing_rejection(rejection: impl ParsingRejection, path: &str) -> ApiError {
    let status = rejection.rejection_status();
    ApiError {
        status: if status == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            status
        },
        message: friendly_parse_message(&rejection.rejection_message(), path),
    }
}

/// Axum's rejection text names Rust types and a byte position in the body.
/// Callers see the same kind of sentence a handler writes: which field, and
/// what it must be.
fn friendly_parse_message(raw: &str, path: &str) -> String {
    if raw.contains("Content-Type") {
        return "Content-Type must be application/json".into();
    }
    if raw.starts_with("Failed to buffer the request body") {
        return "request body could not be read".into();
    }
    if raw.contains("valid UTF-8") {
        return "request body must be valid text".into();
    }

    let detail = without_source_location(strip_parser_prefix(raw));
    if let Some(field) = missing_field(detail) {
        return format!("{field} is required");
    }
    if let Some((field, problem)) = field_problem(detail) {
        return field_message(field, problem, path);
    }
    if raw.starts_with("Failed to parse the request body as JSON") {
        return "request body must be valid JSON".into();
    }
    if detail.contains("expected struct") || detail.starts_with("invalid length") {
        return "request body must be a JSON object".into();
    }
    "request is not valid".into()
}

fn strip_parser_prefix(raw: &str) -> &str {
    for prefix in [
        "Failed to deserialize the JSON body into the target type: ",
        "Failed to parse the request body as JSON: ",
        "Failed to deserialize query string: ",
    ] {
        if let Some(detail) = raw.strip_prefix(prefix) {
            return detail;
        }
    }
    raw
}

fn without_source_location(detail: &str) -> &str {
    let Some(index) = detail.rfind(" at line ") else {
        return detail;
    };
    let Some((line, column)) = detail[index + " at line ".len()..].split_once(" column ") else {
        return detail;
    };
    if !line.is_empty()
        && line.chars().all(|c| c.is_ascii_digit())
        && !column.is_empty()
        && column.chars().all(|c| c.is_ascii_digit())
    {
        detail[..index].trim()
    } else {
        detail
    }
}

fn missing_field(detail: &str) -> Option<&str> {
    detail
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split('`').next())
        .filter(|field| !field.is_empty())
}

fn field_problem(detail: &str) -> Option<(&str, &str)> {
    let (field, problem) = detail.split_once(": ")?;
    field
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then_some((field, problem))
}

/// A value that fails to parse should name the rule the handler would apply,
/// so the caller does not first hear about the Rust type and then the real limit.
fn field_message(field: &str, problem: &str, path: &str) -> String {
    match field {
        "from_account" | "to_account" | "account_id" => format!("{field} must be between 1 and 5"),
        "blocks" => "blocks must be between 1 and 10,000".into(),
        "amount_zatoshi" => amount_message(path),
        "pool" | "source_pool" | "destination_pool" => {
            format!("{field} must be transparent or ironwood")
        }
        "idempotency_key" => "idempotency_key must contain 8-128 visible ASCII characters".into(),
        "confirmation" => "exact confirmation phrase required".into(),
        "uri" => "uri must be a valid payment URI".into(),
        "address" | "to_address" => {
            format!("{field} must be a unified or transparent Regtest address")
        }
        "memo" => "memo must be text".into(),
        _ => format!("{field} must be {}", generic_requirement(problem)),
    }
}

fn amount_message(path: &str) -> String {
    if path == "/api/v1/faucet" || path == "/api/v1/faucet/address" {
        "amount_zatoshi must be greater than zero and no more than 5 ZEC".into()
    } else {
        format!("amount_zatoshi must be between 1 and {MAX_MONEY}")
    }
}

fn generic_requirement(problem: &str) -> &'static str {
    let expected = problem
        .rsplit_once("expected ")
        .map(|(_, expected)| expected.trim());
    match expected {
        Some("u8") => "a whole number from 0 to 255",
        Some("u16" | "u32" | "u64" | "usize" | "i32" | "i64") => "a whole number",
        Some("a string" | "string") => "a string",
        Some("a boolean" | "bool") => "true or false",
        _ if problem.contains("invalid digit")
            || problem.contains("parse integer")
            || problem.contains("number") =>
        {
            "a whole number"
        }
        _ => "valid",
    }
}

trait ParsingRejection {
    fn rejection_status(&self) -> StatusCode;
    fn rejection_message(&self) -> String;
}

impl ParsingRejection for JsonRejection {
    fn rejection_status(&self) -> StatusCode {
        JsonRejection::status(self)
    }

    fn rejection_message(&self) -> String {
        JsonRejection::body_text(self)
    }
}

impl ParsingRejection for QueryRejection {
    fn rejection_status(&self) -> StatusCode {
        QueryRejection::status(self)
    }

    fn rejection_message(&self) -> String {
        QueryRejection::body_text(self)
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
