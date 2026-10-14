use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into());
    let timeout_secs: u64 = std::env::var("TIMEOUT_DELAY_SECS").map_or(Ok(30), |v| v.parse())?;

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("mock PSP listening on {bind_addr} (tok_timeout sleeps {timeout_secs}s)");
    axum::serve(listener, dodo_payments::mock_psp::app(Duration::from_secs(timeout_secs))).await?;
    Ok(())
}
