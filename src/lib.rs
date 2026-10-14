pub mod auth;
pub mod customers;
pub mod error;
pub mod events;
pub mod invoices;
pub mod mock_psp;
pub mod payments;
pub mod webhooks;

use axum::{
    Router,
    routing::{get, post},
};
use std::time::Duration;

use sqlx::PgPool;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Client for the PSP; every call gives up after `psp_timeout` (3 s in production).
    pub psp: reqwest::Client,
    pub psp_url: String,
    /// Client for webhook deliveries; the timeout is set per request.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(pool: PgPool, psp_url: String, psp_timeout: Duration) -> Self {
        let psp = reqwest::Client::builder()
            .timeout(psp_timeout)
            .build()
            .expect("static client config");
        AppState {
            pool,
            psp,
            psp_url,
            http: reqwest::Client::new(),
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/finalize", post(invoices::finalize))
        .route("/invoices/{id}/void", post(invoices::void))
        .route(
            "/invoices/{id}/mark_uncollectible",
            post(invoices::mark_uncollectible),
        )
        .route("/invoices/{id}/pay", post(payments::pay))
        .route(
            "/webhook_endpoints",
            post(webhooks::create).get(webhooks::list),
        )
        .fallback(|| async { error::ApiError::NotFound })
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
