use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    api,
    db::{Store, TREASURY_ACCOUNT_ID},
    payments::{confirmation, treasury},
    state::AppState,
    sync, wallet,
};
use config::{development_credentials, zakura_config};

mod config;

pub(super) fn init(data_dir: PathBuf, config_dir: PathBuf) -> Result<()> {
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

pub(super) async fn serve(data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    let store = Store::open(data_dir.join("ths.db"))?;
    store.initialize()?;
    let wallet = wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let state = AppState::new(
        store,
        wallet,
        std::env::var("THS_ZAKURA_RPC").unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
        std::env::var("THS_INSTANCE").unwrap_or_else(|_| "default".into()),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match sync::dependencies_ready(&state).await {
            Ok(()) => break,
            Err(error) if Instant::now() < deadline => {
                tracing::info!(
                    error = %format_args!("{error:#}"),
                    "waiting for Zakura and lightwalletd"
                );
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            Err(error) => return Err(error).context("waiting for startup dependencies"),
        }
    }
    treasury::provision_initial_balance(&state)
        .await
        .context("provisioning Account 1 with 5 Ironwood ZEC")?;
    let mut background = tokio::task::JoinSet::new();
    background.spawn(sync::wallet_sync_loop(state.clone()));
    background.spawn(confirmation::payment_confirmation_loop(state.clone()));
    let app = api::router(state);
    let address: SocketAddr = std::env::var("THS_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .context("invalid THS_LISTEN")?;
    tracing::info!(%address, "dashboard ready");
    let listener = tokio::net::TcpListener::bind(address).await?;
    serve_with_background(listener, app, background).await
}

async fn serve_with_background(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    mut background: tokio::task::JoinSet<()>,
) -> Result<()> {
    // A stopped or panicked recovery task must not leave HTTP serving as if
    // server-owned background work were still supervised.
    tokio::select! {
        result = axum::serve(listener, app).into_future() => result?,
        result = background.join_next() => {
            result.context("background task set unexpectedly empty")??;
            anyhow::bail!("server background task unexpectedly exited");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/bootstrap/tests.rs"]
mod tests;
