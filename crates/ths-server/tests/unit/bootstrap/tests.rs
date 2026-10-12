use super::*;

async fn assert_background_failure_stops_http(panics: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ready" }));
    let mut background = tokio::task::JoinSet::new();
    let (cancelled, cancellation) = tokio::sync::oneshot::channel::<()>();
    let (started, ready) = tokio::sync::oneshot::channel();
    background.spawn(async move {
        // Dropping this sender proves the sibling task was cancelled.
        let _cancelled = cancelled;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    let (exit, exiting) = tokio::sync::oneshot::channel();
    background.spawn(async move {
        exiting.await.unwrap();
        assert!(!panics, "injected recovery task panic");
    });
    let server = tokio::spawn(serve_with_background(listener, app, background));
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    exit.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    if panics {
        assert!(
            error
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
    } else {
        assert_eq!(
            error.to_string(),
            "server background task unexpectedly exited"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), cancellation)
            .await
            .unwrap()
            .is_err()
    );
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn background_task_exit_stops_http_and_cancels_siblings() {
    assert_background_failure_stops_http(false).await;
}

#[tokio::test]
async fn background_task_panic_stops_http_and_cancels_siblings() {
    assert_background_failure_stops_http(true).await;
}
