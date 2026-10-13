use super::error::ApiError;
use axum::{
    Router,
    handler::HandlerWithoutStateExt,
    http::{StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};
use std::{path::PathBuf, sync::Arc};
use tower_http::services::ServeDir;

/// Serves the dashboard shell for client-side routes only.
///
/// The dashboard is a single-page app, so an unknown path is usually a route
/// like `/explorer/block/42` and must return the shell with `200`. It is not
/// a blanket catch-all: an unknown `/api/` path is a genuine 404, and a
/// missing asset must stay a 404 rather than returning HTML that the browser
/// would then try to parse as JavaScript or CSS.
async fn spa_fallback(uri: Uri, index: Arc<Option<String>>) -> Response {
    let path = uri.path();
    let looks_like_a_file = path
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));

    if path.starts_with("/api/") || looks_like_a_file {
        return ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("{path} does not exist"),
        }
        .into_response();
    }

    match index.as_ref() {
        Some(html) => Html(html.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "dashboard assets are not installed").into_response(),
    }
}

/// Static assets plus the single-page fallback.
pub(crate) fn dashboard_router(static_dir: PathBuf) -> Router {
    // Read once: the shell is small and immutable for the life of the process.
    let index_html = Arc::new(std::fs::read_to_string(static_dir.join("index.html")).ok());
    Router::new().fallback_service(
        // `not_found_service` would wrap the fallback in `SetStatus(404)`,
        // which renders correctly but reports every deep link as missing.
        ServeDir::new(static_dir)
            .fallback((move |uri: Uri| spa_fallback(uri, Arc::clone(&index_html))).into_service()),
    )
}

#[cfg(test)]
#[path = "../../tests/unit/api/dashboard/tests.rs"]
pub(crate) mod tests;
