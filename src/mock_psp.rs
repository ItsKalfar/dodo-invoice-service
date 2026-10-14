use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const NORMAL_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct Psp {
    charges: Arc<Mutex<HashMap<String, Option<Value>>>>,
    timeout_delay: Duration,
}

/// `timeout_delay` is how long `tok_timeout` sleeps (30 s in compose; shorter in tests).
pub fn app(timeout_delay: Duration) -> Router {
    Router::new()
        .route("/charges", post(charge))
        .route("/webhook-sink", post(webhook_sink))
        .with_state(Psp {
            charges: Default::default(),
            timeout_delay,
        })
}

#[derive(Deserialize)]
struct ChargeRequest {
    amount_cents: i64,
    card_token: String,
}

async fn charge(
    State(psp): State<Psp>,
    headers: HeaderMap,
    Json(req): Json<ChargeRequest>,
) -> (StatusCode, Json<Value>) {
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Idempotency-Key header is required"})),
        );
    };
    if req.amount_cents <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "amount_cents must be positive"})),
        );
    }

    // Same key seen before: return the stored result, or say it's still running. Never charge twice.
    {
        let mut charges = psp.charges.lock().unwrap();
        match charges.get(&key) {
            Some(Some(result)) => return (StatusCode::OK, Json(result.clone())),
            Some(None) => return (StatusCode::CONFLICT, Json(json!({"status": "processing"}))),
            None => {
                charges.insert(key.clone(), None);
            }
        }
    }

    let (delay, result) = match req.card_token.as_str() {
        "tok_success" | "tok_network_error" => (NORMAL_DELAY, succeeded(req.amount_cents)),
        "tok_timeout" => (psp.timeout_delay, succeeded(req.amount_cents)),
        "tok_insufficient_funds" => (NORMAL_DELAY, failed("insufficient_funds")),
        "tok_card_declined" => (NORMAL_DELAY, failed("card_declined")),
        _ => {
            psp.charges.lock().unwrap().remove(&key);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "unknown card_token"})),
            );
        }
    };

    // Run the charge in its own task: like a real PSP, it finishes even if the caller gives up
    // (e.g. our client times out during tok_timeout), so a later retry with the same key sees the result.
    let charges = psp.charges.clone();
    let stored = result.clone();
    let done = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        charges.lock().unwrap().insert(key, Some(stored));
    });
    let _ = done.await;

    if req.card_token == "tok_network_error" {
        tracing::warn!("tok_network_error: charged, now dropping the connection");
        panic!("mock PSP: simulated dropped connection");
    }
    (StatusCode::OK, Json(result))
}

async fn webhook_sink(headers: HeaderMap, body: String) -> StatusCode {
    let signature = headers
        .get("dodo-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let event: Value = serde_json::from_str(&body).unwrap_or_default();
    let field = |v: &Value| v.as_str().unwrap_or("?").to_owned();
    tracing::info!(
        r#type = field(&event["type"]),
        invoice = field(&event["data"]["id"]),
        status = field(&event["data"]["status"]),
        event_id = field(&event["id"]),
        signature,
        body,
        "webhook received"
    );
    StatusCode::OK
}

fn succeeded(amount_cents: i64) -> Value {
    json!({"status": "succeeded", "psp_ref": Uuid::new_v4(), "amount_cents": amount_cents})
}

fn failed(code: &str) -> Value {
    json!({"status": "failed", "code": code})
}
