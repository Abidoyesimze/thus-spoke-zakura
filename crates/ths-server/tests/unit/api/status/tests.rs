use crate::api::router;
use crate::state::tests::state_with_local_wallet;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
#[tokio::test]
async fn status_counts_accounts_without_reading_the_seed() {
    let (state, dir) = state_with_local_wallet();
    rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .execute("UPDATE metadata SET value='not-hex' WHERE key='seed'", [])
        .unwrap();

    let response = router(state)
        .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["account_count"], 5);
}
