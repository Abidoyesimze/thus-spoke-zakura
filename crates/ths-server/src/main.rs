#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ths_server=info,tower_http=info".into()),
        )
        .init();
    match ths_server::run_from(std::env::args_os()).await {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}
