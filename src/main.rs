use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let database_url = std::env::var("DATABASE_URL")?;
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let psp_url = std::env::var("PSP_URL")?;

    let pool = PgPoolOptions::new().max_connections(10).connect(&database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    tracing::info!("migrations applied");

    // Demo bootstrap so `docker compose up` + README curls work with no manual steps (see DESIGN.md).
    if let Ok(key) = std::env::var("SEED_API_KEY") {
        let business_id = dodo_payments::auth::ensure_business_with_key(&pool, "Demo Business", &key).await?;
        tracing::info!(%business_id, "seed business ready");
    }

    let state = dodo_payments::AppState::new(pool, psp_url, std::time::Duration::from_secs(3));
    tokio::spawn(dodo_payments::payments::run_sweeper(state.clone()));
    tokio::spawn(dodo_payments::webhooks::run_worker(state.clone()));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("listening on {bind_addr}");
    axum::serve(listener, dodo_payments::app(state)).await?;
    Ok(())
}
