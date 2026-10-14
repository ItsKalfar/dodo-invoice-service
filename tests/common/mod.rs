use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{extract::Request, middleware::Next};
use sqlx::PgPool;

/// PSP client timeout in tests; tok_timeout (PSP_TOK_TIMEOUT) is longer, so it really times out.
pub const PSP_TIMEOUT: Duration = Duration::from_millis(500);
pub const PSP_TOK_TIMEOUT: Duration = Duration::from_millis(1500);

pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{addr}")
}

/// Idempotency-Key of every request the mock PSP received, in order. Lets tests check what the PSP
/// actually saw: number of calls, and number of distinct charges (distinct keys).
pub type PspCalls = Arc<Mutex<Vec<String>>>;

/// Starts a mock PSP (with a call recorder in front) and the real app wired to it, on random ports.
#[allow(dead_code)] // not every test file uses the state
pub async fn spawn_with_state(pool: PgPool) -> (dodo_payments::AppState, String, PspCalls) {
    let calls = PspCalls::default();
    let recorder = calls.clone();
    let psp = dodo_payments::mock_psp::app(PSP_TOK_TIMEOUT).layer(axum::middleware::from_fn(
        move |req: Request, next: Next| {
            let key = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            recorder.lock().unwrap().push(key);
            next.run(req)
        },
    ));
    let psp_url = serve(psp).await;
    let state = dodo_payments::AppState::new(pool, psp_url, PSP_TIMEOUT);
    let base = serve(dodo_payments::app(state.clone())).await;
    (state, base, calls)
}

#[allow(dead_code)]
pub async fn spawn(pool: PgPool) -> String {
    spawn_with_state(pool).await.1
}
