use std::time::Duration;

use seshat::{config::Config, routes::AppState};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().inspect_err(|error| {
        tracing::error!(config_code = error.code(), "invalid Seshat configuration");
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()?;
    let listener = TcpListener::bind(config.bind_addr()).await?;
    let address = listener.local_addr()?;
    tracing::info!(
        listen_addr = %address,
        search_upstream = ?config.search_upstream(),
        "Seshat listening"
    );

    axum::serve(
        listener,
        seshat::routes::build_router(AppState::new(config, client)),
    )
    .await?;
    Ok(())
}
