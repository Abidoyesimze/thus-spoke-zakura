use super::*;
use crate::db::Store;

#[test]
fn only_the_exact_already_queued_rejection_is_accepted() {
    check_broadcast_response(SendResponse {
        error_code: 0,
        error_message: String::new(),
    })
    .unwrap();
    check_broadcast_response(SendResponse {
        error_code: -1,
        error_message: "transaction dropped because it is already queued for download".into(),
    })
    .unwrap();
    for message in [
        "transaction dropped because the queue is full",
        "mempool is disabled since synchronization is behind the chain tip",
        "transaction is non-standard",
        "unexpected rejection: transaction dropped because it is already queued for download",
        "",
    ] {
        let error = check_broadcast_response(SendResponse {
            error_code: -1,
            error_message: message.into(),
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("lightwalletd rejected transaction: {message}")
        );
    }
}

#[tokio::test]
async fn wallet_broadcast_accepts_the_lightwalletd_queued_response() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        assert_eq!(
            request.uri().path(),
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction"
        );
        let message = b"transaction dropped because it is already queued for download";
        // SendResponse protobuf fields: error_code=1 and the queue message.
        let mut protobuf = vec![0x08, 1, 0x12, u8::try_from(message.len()).unwrap()];
        protobuf.extend_from_slice(message);
        let mut frame = vec![0];
        frame.extend_from_slice(&u32::try_from(protobuf.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&protobuf);
        let mut stream = response
            .send_response(
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .body(())
                    .unwrap(),
                false,
            )
            .unwrap();
        stream
            .send_data(axum::body::Bytes::from(frame), false)
            .unwrap();
        let mut trailers = axum::http::HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        stream.send_trailers(trailers).unwrap();
        while connection.accept().await.is_some() {}
    });
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = RealWallet::open(dir.path(), &hex::encode([7_u8; 64])).unwrap();
    wallet.lightwalletd = endpoint;
    tokio::time::timeout(
        Duration::from_secs(5),
        wallet.broadcast(b"original signed bytes"),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn broadcast_times_out_after_lightwalletd_accepts_the_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, response) = connection.accept().await.unwrap().unwrap();
        assert_eq!(
            request.uri().path(),
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction"
        );
        accepted_tx.send(()).unwrap();
        let _response = response;
        std::future::pending::<()>().await;
    });

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet.lightwalletd = endpoint;
    let broadcast = tokio::spawn(async move { wallet.broadcast(&[1, 2, 3]).await });

    tokio::time::timeout(Duration::from_secs(5), accepted_rx)
        .await
        .expect("lightwalletd never received the broadcast")
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert!(broadcast.is_finished(), "broadcast ignored its deadline");
    let error = broadcast.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    server.abort();
}
