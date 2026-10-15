use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
/// A dashboard directory containing a recognisable shell and one asset.
fn dashboard() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><div id=\"root\">",
    )
    .expect("write shell");
    std::fs::create_dir(dir.path().join("assets")).expect("assets dir");
    std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").expect("write asset");
    dir
}

async fn get(dir: &tempfile::TempDir, path: &str) -> (StatusCode, String) {
    let response = dashboard_router(dir.path().to_path_buf())
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The dashboard is a single-page app: a path it owns is a route, not a
/// missing file. This previously returned 404 for every path but `/`,
/// because `not_found_service` wraps the fallback in `SetStatus(404)`.
#[tokio::test]
async fn client_side_routes_return_the_shell() {
    let dir = dashboard();
    for path in [
        "/",
        "/wallet",
        "/explorer",
        "/explorer/block/1209",
        "/network",
    ] {
        let (status, body) = get(&dir, path).await;
        assert_eq!(status, StatusCode::OK, "{path} should serve the shell");
        assert!(
            body.contains("id=\"root\""),
            "{path} should return the shell markup"
        );
    }
}

/// The fallback is scoped, not a catch-all. A missing asset answered with
/// HTML would be parsed by the browser as JavaScript or CSS.
#[tokio::test]
async fn missing_assets_and_unknown_api_paths_stay_404() {
    let dir = dashboard();
    for path in [
        "/api/v1/nope",
        "/assets/does-not-exist.js",
        "/favicon.ico",
        "/nested/path/styles.css",
    ] {
        let (status, body) = get(&dir, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} should be a 404");
        assert!(
            !body.contains("id=\"root\""),
            "{path} must not return the shell"
        );
    }
}

#[tokio::test]
async fn real_assets_are_still_served() {
    let dir = dashboard();
    let (status, body) = get(&dir, "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "console.log(1)");
}

/// A misconfigured THS_WEB_DIR should fail visibly rather than serving an
/// empty 200 that looks like a working dashboard.
#[tokio::test]
async fn a_missing_shell_is_reported_rather_than_served_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (status, _) = get(&dir, "/wallet").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
