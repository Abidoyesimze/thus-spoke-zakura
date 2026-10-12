use super::*;
use crate::api::router;
use crate::state::{notify, tests::state_with_local_wallet};
use axum::{body::Body, http::Request};
use tower::ServiceExt;
/// a client that falls behind the event buffer must be told to refetch
/// everything, since it cannot know which topics it missed.
#[tokio::test]
async fn a_lagging_event_stream_asks_the_client_to_resync() {
    use futures_core::Stream;

    let (state, _dir) = state_with_local_wallet();
    let response = router(state.clone())
        .oneshot(Request::get("/api/v1/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    for _ in 0..=128 {
        notify(&state, "chain");
    }

    let mut received = String::new();
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout(
        Duration::from_millis(200),
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_next(cx)),
    )
    .await
    {
        received.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(received.contains("data: resync"), "{received}");
}
