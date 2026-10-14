mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::post};
use dodo_payments::{auth::ensure_business_with_key, webhooks};
use serde_json::{Value, json};
use sqlx::PgPool;

/// (Dodo-Signature header, body) of every webhook the fake receiver got.
type Received = Arc<Mutex<Vec<(String, String)>>>;

/// A business's webhook receiver. Answers 200 while `ok` is true, else 500.
async fn receiver(ok: Arc<AtomicBool>) -> (String, Received) {
    let received = Received::default();
    let router = axum::Router::new()
        .route(
            "/hook",
            post(|State((ok, received)): State<(Arc<AtomicBool>, Received)>, headers: HeaderMap, body: String| async move {
                let sig = headers["dodo-signature"].to_str().unwrap().to_owned();
                received.lock().unwrap().push((sig, body));
                if ok.load(Ordering::SeqCst) { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR }
            }),
        )
        .with_state((ok, received.clone()));
    (format!("{}/hook", common::serve(router).await), received)
}

/// Registers `url` as a webhook endpoint and creates one invoice (-> one invoice.created delivery).
/// Returns the endpoint secret.
async fn register_and_create_invoice(base: &str, url: &str) -> String {
    let http = reqwest::Client::new();
    let post = |path: &str, body: Value| http.post(format!("{base}{path}")).bearer_auth("sk_test_wh").json(&body).send();

    let res = post("/webhook_endpoints", json!({"url": "ftp://nope"})).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = post("/webhook_endpoints", json!({ "url": url })).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let secret = res.json::<Value>().await.unwrap()["secret"].as_str().unwrap().to_owned();

    let customer: Value =
        post("/customers", json!({"name": "Asha", "email": "asha@example.com"})).await.unwrap().json().await.unwrap();
    let res = post(
        "/invoices",
        json!({
            "customer_id": customer["id"],
            "due_date": "2026-11-01",
            "line_items": [{"description": "Plan", "quantity": 1, "unit_amount_cents": 4000}]
        }),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    secret
}

async fn delivery(pool: &PgPool) -> (String, i32, bool) {
    sqlx::query_as("SELECT status, attempts, next_attempt_at > now() + interval '50 seconds' FROM webhook_deliveries")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn delivers_signed_webhook(pool: PgPool) {
    ensure_business_with_key(&pool, "WH", "sk_test_wh").await.unwrap();
    let (state, base, _) = common::spawn_with_state(pool.clone()).await;
    let (url, received) = receiver(Arc::new(AtomicBool::new(true))).await;
    let secret = register_and_create_invoice(&base, &url).await;

    webhooks::deliver_due(&state).await.unwrap();

    let received = received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    let (sig, body) = &received[0];
    let event: Value = serde_json::from_str(body).unwrap();
    assert_eq!(event["type"], "invoice.created");
    assert_eq!(event["data"]["total_cents"], 4000);
    // Receiver-side check: recompute the HMAC over "<t>.<body>" with the shared secret.
    let t: i64 = sig.strip_prefix("t=").unwrap().split(',').next().unwrap().parse().unwrap();
    assert_eq!(sig, &webhooks::sign(&secret, t, body));
    assert_eq!(delivery(&pool).await, ("delivered".into(), 1, false));
}

#[sqlx::test]
async fn retries_with_backoff_then_gives_up(pool: PgPool) {
    ensure_business_with_key(&pool, "WH", "sk_test_wh").await.unwrap();
    let (state, base, _) = common::spawn_with_state(pool.clone()).await;
    let (url, received) = receiver(Arc::new(AtomicBool::new(false))).await;
    register_and_create_invoice(&base, &url).await;

    // First failure: still pending, next try ~1 minute out.
    webhooks::deliver_due(&state).await.unwrap();
    assert_eq!(delivery(&pool).await, ("pending".into(), 1, true));

    // Not due yet, so the worker leaves it alone.
    webhooks::deliver_due(&state).await.unwrap();
    assert_eq!(received.lock().unwrap().len(), 1);

    // Jump to the 8th (last) attempt: failing it marks the delivery failed, no more retries.
    sqlx::query("UPDATE webhook_deliveries SET attempts = 7, next_attempt_at = now()").execute(&pool).await.unwrap();
    webhooks::deliver_due(&state).await.unwrap();
    assert_eq!(delivery(&pool).await.0, "failed");
    assert_eq!(delivery(&pool).await.1, 8);
}
