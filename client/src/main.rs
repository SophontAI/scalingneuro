use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "neuro_sync=warn".into()),
        )
        .with_target(false)
        .compact()
        .init();
    neuro_sync::cli::ensure_no_unexpected_args()?;
    neuro_sync::cli::execute(neuro_sync::cli::parse()).await
}
