mod api;
mod db;
mod mining;
mod reconcile;
mod rpc;
mod wallet;

use std::{fmt::Write, fs, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use db::{Store, TREASURY_ACCOUNT_ID};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ths-server", version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "/config")]
        config_dir: PathBuf,
    },
    Serve {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ths_server=info,tower_http=info".into()),
        )
        .init();
    match Args::parse().command {
        Command::Init {
            data_dir,
            config_dir,
        } => init(data_dir, config_dir),
        Command::Serve { data_dir } => serve(data_dir).await,
    }
}

fn init(data_dir: PathBuf, config_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&config_dir)?;
    let store = Store::open(data_dir.join("ths.db"))?;
    store.initialize()?;
    wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let miner = store.account(TREASURY_ACCOUNT_ID)?.transparent_address;
    fs::write(config_dir.join("zakurad.toml"), zakura_config(&miner))?;
    println!("initialized five development accounts and a hidden treasury; miner address {miner}");
    println!("{}", development_credentials(&store)?);
    Ok(())
}

const DEVELOPMENT_CREDENTIAL_WARNING: &str = concat!(
    "WARNING: Never use this mnemonic or private key in production. ",
    "It is intended purely for development. Using it in production will result in loss of funds."
);

fn development_credentials(store: &Store) -> Result<String> {
    let secrets = store.development_secrets()?;
    let mut output =
        String::from("\n⚠ DISPOSABLE REGTEST SECRETS — NEVER SEND REAL FUNDS TO THESE KEYS\n");
    writeln!(output, "{DEVELOPMENT_CREDENTIAL_WARNING}")?;
    writeln!(output, "Mnemonic: {}", secrets.mnemonic)?;
    for account in secrets.accounts {
        writeln!(
            output,
            "Account {}: {}",
            account.id, account.unified_address
        )?;
        writeln!(
            output,
            "  Unified spending key (hex): {}",
            account.unified_spending_key_hex
        )?;
    }
    Ok(output.trim_end().to_owned())
}

/// A zero-value payment to the P2SH address of an all-zero script hash. Every
/// chain that activates NU6.1 must carry the one-time ZIP-271 lockbox
/// disbursement in its activation-block coinbase, and Regtest configures none,
/// so Zakura rejects that block. This marker satisfies the rule without
/// creating funds or drawing on the empty local lockbox; Zakura's own local
/// network generator uses the same marker.
const LOCKBOX_MARKER_ADDRESS: &str = "t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2";

fn zakura_config(miner: &str) -> String {
    // Keep these heights in sync with `wallet::regtest_network()`.
    format!(
        r#"[network]
network = "Regtest"
listen_addr = "0.0.0.0:18233"
[network.testnet_parameters]
lockbox_disbursements = [{{ address = "{LOCKBOX_MARKER_ADDRESS}", amount = 0 }}]
[network.testnet_parameters.activation_heights]
"NU6" = 1
"NU6.1" = 1
"NU6.2" = 1
"NU6.3" = 1

[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false

[state]
cache_dir = "/data"

[mining]
miner_address = "{miner}"
extra_coinbase_data = "thus-spoke-zakura"
"#
    )
}

async fn serve(data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    let store = Store::open(data_dir.join("ths.db"))?;
    store.initialize()?;
    let wallet = wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let state = api::AppState::new(
        store,
        wallet,
        std::env::var("THS_ZAKURA_RPC").unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
        std::env::var("THS_INSTANCE").unwrap_or_else(|_| "default".into()),
    );
    let address: SocketAddr = std::env::var("THS_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .context("invalid THS_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "serving stored status while startup continues");
    serve_with_background(
        listener,
        api::router(state.clone()),
        background_tasks(state),
    )
    .await
}

fn background_tasks(state: api::AppState) -> tokio::task::JoinSet<Result<()>> {
    let mut background = tokio::task::JoinSet::new();
    background.spawn(start_and_sync(state.clone()));
    background.spawn(async move {
        api::payment_confirmation_loop(state).await;
        Ok(())
    });
    background
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadinessError {
    #[error("chain is below the wallet birthday floor; waiting for canonical block 1")]
    BelowBirthday,
    #[error("lightwalletd has not indexed the canonical rewind checkpoint")]
    RewindCheckpoint,
    #[error("chain checkpoint changed before wallet publication")]
    CheckpointChanged,
    #[error("initial funding is awaiting confirmation")]
    FundingPending,
}

fn startup_retryable(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<rpc::RpcUnavailable>().is_some()
        || error.downcast_ref::<rpc::RpcTransport>().is_some()
        || error.downcast_ref::<ReadinessError>().is_some()
        || error
            .downcast_ref::<reconcile::ReconciliationDeadline>()
            .is_some()
        || error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    {
        return true;
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return error.is_connect() || error.is_timeout();
    }
    if let Some(error) = error.downcast_ref::<rpc::RpcError>() {
        return error.code == -28;
    }
    if let Some(error) = error.downcast_ref::<tonic::Status>() {
        return matches!(
            error.code(),
            tonic::Code::Unavailable
                | tonic::Code::DeadlineExceeded
                | tonic::Code::ResourceExhausted
        );
    }
    error.downcast_ref::<tonic::transport::Error>().is_some()
}

async fn retry_startup<F, Fut>(mut attempt: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) if startup_retryable(&error) => {
                tracing::info!(error = %format_args!("{error:#}"), "waiting for startup readiness");
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn start_and_sync(state: api::AppState) -> Result<()> {
    let already_provisioned = api::initial_balance_provisioned(&state)?;
    // Network stages have their own deadlines. Await provisioning to completion
    // so its payment guards remain owned while a wallet worker is running.
    retry_startup(|| async {
        api::dependencies_ready(&state)
            .await
            .context("waiting for startup dependencies")?;
        api::provision_initial_balance_with_history(&state, already_provisioned)
            .await
            .context("provisioning Account 1 with 5 Ironwood ZEC")
    })
    .await?;
    state.mark_started();
    tracing::info!("dashboard ready");
    api::wallet_sync_loop(state).await;
    Ok(())
}

async fn serve_with_background(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    mut background: tokio::task::JoinSet<Result<()>>,
) -> Result<()> {
    // A stopped or panicked recovery task must not leave HTTP serving as if
    // server-owned background work were still supervised.
    tokio::select! {
        result = axum::serve(listener, app).into_future() => result?,
        result = background.join_next() => {
            let task = result.context("background task set unexpectedly empty")?;
            task??;
            anyhow::bail!("server background task unexpectedly exited");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUNCATED_RPC_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n{";

    fn complete_rpc_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn rpc_responses(
        responses: Vec<String>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        tokio::sync::watch::Receiver<usize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (sent, progress) = tokio::sync::watch::channel(0);
        let server = tokio::spawn(async move {
            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    assert!(stream.read_buf(&mut request).await.unwrap() > 0);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        if headers.starts_with("HEAD ") {
                            // Local health probes can connect to ephemeral test ports.
                            stream = listener.accept().await.unwrap().0;
                            request.clear();
                            continue;
                        }
                        let length: usize = headers
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .unwrap()
                            .1
                            .trim()
                            .parse()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
                sent.send_replace(index + 1);
            }
        });
        (endpoint, server, progress)
    }

    async fn assert_startup_recovers_from_rpc_disconnect(response: &str) {
        let (endpoint, server, _) = rpc_responses(vec![
            response.to_owned(),
            complete_rpc_response(
                r#"{"result":{"chain":"regtest","blocks":1},"error":null,"id":0}"#,
            ),
        ])
        .await;
        let rpc = rpc::NodeRpc::new(endpoint);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            retry_startup(|| async { rpc.chain_info().await.map(|_| ()) }),
        )
        .await;
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled());
        }
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn startup_recovers_when_rpc_disconnects_before_response_headers() {
        assert_startup_recovers_from_rpc_disconnect("").await;
    }

    #[tokio::test]
    async fn startup_recovers_when_rpc_disconnects_during_the_response_body() {
        assert_startup_recovers_from_rpc_disconnect(TRUNCATED_RPC_RESPONSE).await;
    }

    #[tokio::test]
    async fn invalid_rpc_endpoints_remain_fatal_during_startup() {
        for endpoint in ["://invalid", "ftp://127.0.0.1:1"] {
            let error = rpc::NodeRpc::new(endpoint.into())
                .chain_info()
                .await
                .unwrap_err();
            assert!(!startup_retryable(&error), "{error:#}");
        }
    }

    #[tokio::test]
    async fn stored_status_survives_repeated_rpc_response_disconnects() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ths.db")).unwrap();
        store.initialize().unwrap();
        let activity = store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_000,
                "disconnect-send",
                None,
            )
            .unwrap();
        let faucet = store
            .claim_address_faucet("tm-external", 7, "disconnect-faucet")
            .unwrap();
        store
            .record_address_error(&faucet.id, "preparing", "", "awaiting node")
            .unwrap();
        let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let (endpoint, rpc_server, mut progress) =
            rpc_responses(vec![TRUNCATED_RPC_RESPONSE.to_owned(); 3]).await;
        let state = api::AppState::new(store.clone(), wallet.clone(), endpoint, "test".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_with_background(
            listener,
            api::router(state.clone()),
            background_tasks(state),
        ));
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            while *progress.borrow_and_update() < 3 {
                progress.changed().await?;
            }
            Ok::<_, tokio::sync::watch::error::RecvError>(())
        })
        .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let result = async {
            observed.context("startup did not retry the interrupted RPC")??;
            let response = client
                .get(format!(
                    "http://{address}/api/v1/activity?idempotency_key=disconnect-send"
                ))
                .send()
                .await?;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let body: serde_json::Value = response.json().await?;
            assert_eq!(body[0]["id"], activity.id);
            assert_eq!(body[0]["status"], "preparing");
            assert_eq!(body[0]["txid"], "");
            let response = client
                .get(format!(
                    "http://{address}/api/v1/faucet/address?idempotency_key=disconnect-faucet"
                ))
                .send()
                .await?;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let body: serde_json::Value = response.json().await?;
            assert_eq!(body["status"], "preparing");
            assert_eq!(body["last_error"], "awaiting node");
            let health = client
                .get(format!("http://{address}/api/v1/health"))
                .send()
                .await?;
            assert_eq!(health.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            assert!(!server.is_finished());
            assert!(store.prepared_transaction(&activity.id).is_err());
            assert!(wallet.prepared(&activity.id).await?.is_none());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled());
        }
        rpc_server.abort();
        if let Err(error) = rpc_server.await {
            assert!(error.is_cancelled());
        }
        result.unwrap();
    }

    #[tokio::test]
    async fn complete_malformed_rpc_json_stops_http_during_startup() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ths.db")).unwrap();
        store.initialize().unwrap();
        let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let (endpoint, rpc_server, _) = rpc_responses(vec![complete_rpc_response("{")]).await;
        let state = api::AppState::new(store, wallet, endpoint, "test".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            serve_with_background(
                listener,
                api::router(state.clone()),
                background_tasks(state),
            ),
        )
        .await;
        rpc_server.abort();
        if let Err(error) = rpc_server.await {
            assert!(error.is_cancelled());
        }
        let error = result.unwrap().unwrap_err();
        assert!(!startup_retryable(&error));
        assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn startup_retries_past_two_minutes_with_a_delay_then_recovers() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = attempts.clone();
        let started = tokio::time::Instant::now();
        retry_startup(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                if started.elapsed() < Duration::from_secs(125) {
                    Err(tonic::Status::unavailable("dependency unavailable").into())
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(125));
        assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) < 180);
    }

    #[tokio::test(start_paused = true)]
    async fn startup_does_not_cancel_a_slow_preparation_or_release_its_guard() {
        let guard = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let owned = guard.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let mut attempt = Some((entered, released));
        let startup = tokio::spawn(async move {
            retry_startup(|| {
                let owned = owned.clone();
                let (entered, released) = attempt.take().expect("preparation was restarted");
                async move {
                    let _guard = owned.lock().await;
                    entered.send(()).unwrap();
                    released.await.unwrap();
                    Ok(())
                }
            })
            .await
        });
        ready.await.unwrap();
        tokio::time::advance(Duration::from_secs(130)).await;
        assert!(!startup.is_finished());
        assert!(guard.try_lock().is_err());
        release.send(()).unwrap();
        startup.await.unwrap().unwrap();
        assert!(guard.try_lock().is_ok());
    }

    #[test]
    fn startup_retries_only_typed_transient_failures() {
        for code in [-28, -5, -32603] {
            let rpc: rpc::RpcError =
                serde_json::from_value(serde_json::json!({"code": code, "message": "waiting"}))
                    .unwrap();
            assert_eq!(
                startup_retryable(&anyhow::Error::new(rpc).context("startup")),
                code == -28
            );
        }
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
            tonic::Code::ResourceExhausted,
            tonic::Code::InvalidArgument,
            tonic::Code::DataLoss,
            tonic::Code::Internal,
            tonic::Code::Unknown,
        ] {
            let error = anyhow::Error::new(tonic::Status::new(code, "waiting"));
            assert_eq!(
                startup_retryable(&error),
                matches!(
                    code,
                    tonic::Code::Unavailable
                        | tonic::Code::DeadlineExceeded
                        | tonic::Code::ResourceExhausted
                )
            );
        }
        for error in [
            anyhow::anyhow!("node unavailable"),
            anyhow::anyhow!("invalid Regtest checkpoint"),
            rusqlite::Error::InvalidQuery.into(),
            db::IdempotencyConflict.into(),
            wallet::PaymentError::TreasuryExhausted.into(),
            wallet::PaymentError::ScanRequired.into(),
        ] {
            assert!(!startup_retryable(&error), "{error}");
        }
        for pending in [
            ReadinessError::BelowBirthday,
            ReadinessError::RewindCheckpoint,
            ReadinessError::CheckpointChanged,
            ReadinessError::FundingPending,
        ] {
            assert!(startup_retryable(&pending.into()));
        }
        assert!(startup_retryable(&reconcile::ReconciliationDeadline.into()));
    }

    #[tokio::test]
    async fn fatal_startup_error_stops_http_and_cancels_siblings() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (cancelled, cancellation) = tokio::sync::oneshot::channel::<()>();
        let mut background = tokio::task::JoinSet::new();
        background.spawn(async move {
            let _cancelled = cancelled;
            std::future::pending::<Result<()>>().await
        });
        background.spawn(retry_startup(|| async {
            Err(rusqlite::Error::InvalidQuery.into())
        }));
        let error = serve_with_background(listener, axum::Router::new(), background)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        assert!(cancellation.await.is_err());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

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
            std::future::pending::<Result<()>>().await
        });
        ready.await.unwrap();
        let (exit, exiting) = tokio::sync::oneshot::channel();
        background.spawn(async move {
            exiting.await.unwrap();
            assert!(!panics, "injected recovery task panic");
            Ok(())
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

    #[tokio::test]
    async fn stored_payments_are_served_after_restart_while_dependencies_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ths.db")).unwrap();
        store.initialize().unwrap();
        let failed = store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_000,
                "restart-failed",
                None,
            )
            .unwrap();
        store.fail(&failed.id, "insufficient funds").unwrap();
        let external = store
            .claim_address_faucet("tm-external", 7, "restart-external")
            .unwrap();
        store
            .record_address_error(&external.id, "preparing", "", "node unavailable")
            .unwrap();
        drop(store);

        let store = Store::open(dir.path().join("ths.db")).unwrap();
        store.initialize().unwrap();
        let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let state = api::AppState::new(
            store.clone(),
            wallet,
            "http://127.0.0.1:1".into(),
            "test".into(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_with_background(
            listener,
            api::router(state.clone()),
            background_tasks(state),
        ));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let get = |path: &str| {
            let request = client.get(format!("http://{address}{path}"));
            async move {
                let response = request.send().await.unwrap();
                let status = response.status();
                (status, response.json::<serde_json::Value>().await.unwrap())
            }
        };

        tokio::time::sleep(Duration::from_secs(122)).await;
        let (status, activity) = get("/api/v1/activity?idempotency_key=restart-failed").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(activity[0]["id"], failed.id);
        assert_eq!(activity[0]["status"], "failed");
        assert_eq!(activity[0]["last_error"], "insufficient funds");
        let (status, faucet) = get("/api/v1/faucet/address?idempotency_key=restart-external").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(faucet["status"], "preparing");
        assert_eq!(faucet["last_error"], "node unavailable");
        let (status, health) = get("/api/v1/health").await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(health["ok"], false);
        let response = client
            .post(format!("http://{address}/api/v1/faucet"))
            .json(&serde_json::json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 1_000, "idempotency_key": "restart-faucet"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert!(store.activity_for_key("restart-faucet").unwrap().is_none());
        assert!(!server.is_finished());
        server.abort();
    }

    #[test]
    fn configures_the_hidden_treasury_as_miner() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let user = store.account(1).unwrap().transparent_address;
        let treasury = store
            .account(TREASURY_ACCOUNT_ID)
            .unwrap()
            .transparent_address;
        let config = zakura_config(&treasury);
        assert!(config.contains(&format!("miner_address = \"{treasury}\"")));
        assert!(!config.contains(&format!("miner_address = \"{user}\"")));
    }

    #[test]
    fn activates_ironwood_with_a_lockbox_marker() {
        let config = zakura_config("tm-miner");
        for upgrade in ["NU6", "NU6.1", "NU6.2", "NU6.3"] {
            assert!(config.contains(&format!("\"{upgrade}\" = 1\n")));
        }
        assert!(config.contains(&format!(
            "lockbox_disbursements = [{{ address = \"{LOCKBOX_MARKER_ADDRESS}\", amount = 0 }}]"
        )));
        let network = wallet::regtest_network();
        assert!(matches!(
            zcash_keys::address::Address::decode(&network, LOCKBOX_MARKER_ADDRESS),
            Some(zcash_keys::address::Address::Transparent(_))
        ));
        assert_eq!(network.nu6_1, network.nu6);
        assert_eq!(network.nu6_2, network.nu6);
        assert_eq!(network.nu6_3, network.nu6);
    }

    #[test]
    fn prints_mnemonic_and_exactly_five_user_account_keys() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();

        let output = development_credentials(&store).unwrap();
        let mnemonic = store.development_secrets().unwrap().mnemonic;
        let warning_at = output
            .find(DEVELOPMENT_CREDENTIAL_WARNING)
            .expect("startup credentials must warn against production use");
        let mnemonic_at = output
            .find("Mnemonic:")
            .expect("startup credentials must include the mnemonic");
        assert!(
            warning_at < mnemonic_at,
            "the production-use warning must appear next to the mnemonic and spending keys"
        );
        assert!(output.contains("Never use this mnemonic or private key in production"));
        assert!(output.contains("purely for development"));
        assert!(output.contains("will result in loss of funds"));
        assert!(output.contains(&format!("Mnemonic: {mnemonic}")));
        assert_eq!(
            hex::encode(mnemonic.parse::<bip39::Mnemonic>().unwrap().to_seed("")),
            store.seed().unwrap()
        );
        assert_eq!(output.matches("Unified spending key (hex):").count(), 5);
        for id in 1..=5 {
            assert!(output.contains(&format!("Account {id}:")));
        }
        assert!(!output.contains("Account 6:"));
    }
}
