//! Webhook endpoint registration, plus the worker that delivers the outbox (`webhook_deliveries`).

use std::time::Duration;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use crate::{AppState, auth::BusinessId, error::ApiError};

/// Wait after failed attempt 1, 2, ... 7. Attempt 8 failing ends it: 8 attempts over ~45 h (DESIGN.md §4).
const RETRY_DELAYS_SECS: [i32; 7] = [60, 5 * 60, 30 * 60, 2 * 3600, 6 * 3600, 12 * 3600, 24 * 3600];
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, sqlx::FromRow)]
pub struct Endpoint {
    pub id: Uuid,
    pub url: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateEndpoint {
    url: String,
}

/// The secret is returned only here, once; receivers need it to verify signatures.
pub async fn create(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    body: Result<Json<CreateEndpoint>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(body) = body?;
    // ponytail: scheme check only; production must also block private/internal addresses (SSRF).
    if !url::Url::parse(&body.url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
        return Err(ApiError::Validation("url must be a valid http(s) URL".into()));
    }
    let secret = format!("whsec_{}", hex::encode(rand::random::<[u8; 32]>()));
    let endpoint = sqlx::query_as::<_, Endpoint>(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4)
         RETURNING id, url, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(&body.url)
    .bind(&secret)
    .fetch_one(&state.pool)
    .await?;
    let mut out = json!(endpoint);
    out["secret"] = json!(secret);
    Ok((StatusCode::CREATED, Json(out)))
}

pub async fn list(State(state): State<AppState>, BusinessId(business_id): BusinessId) -> Result<Json<Value>, ApiError> {
    let endpoints = sqlx::query_as::<_, Endpoint>(
        "SELECT id, url, created_at FROM webhook_endpoints
         WHERE business_id = $1 AND disabled_at IS NULL ORDER BY created_at DESC",
    )
    .bind(business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "data": endpoints })))
}

/// `Dodo-Signature` header value: `t=<unix secs>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`.
/// Signing the timestamp lets receivers reject old replays (e.g. anything older than 5 minutes).
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
}

// ---------- Worker ----------

pub async fn run_worker(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        if let Err(e) = deliver_due(&state).await {
            tracing::error!(error = ?e, "webhook worker failed");
        }
    }
}

/// Sends up to 10 due deliveries. Claiming pushes `next_attempt_at` 30 s ahead (a lease), so no DB
/// transaction stays open during HTTP, two app instances never send the same row, and a crash
/// mid-send just makes the row due again. Delivery is at-least-once; receivers dedupe on the event id.
pub async fn deliver_due(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, Value, i32, String, String)> = sqlx::query_as(
        "UPDATE webhook_deliveries d SET next_attempt_at = now() + interval '30 seconds'
         FROM webhook_endpoints e
         WHERE e.id = d.endpoint_id AND d.id IN (
             SELECT id FROM webhook_deliveries WHERE status = 'pending' AND next_attempt_at <= now()
             ORDER BY next_attempt_at LIMIT 10 FOR UPDATE SKIP LOCKED)
         RETURNING d.id, d.payload, d.attempts, e.url, e.secret",
    )
    .fetch_all(&state.pool)
    .await?;

    // Concurrently, so one slow receiver doesn't hold up the others.
    let mut sends = tokio::task::JoinSet::new();
    for (id, payload, attempts, url, secret) in due {
        let state = state.clone();
        sends.spawn(async move {
            let result = send(&state, &url, &secret, &payload).await;
            record_result(&state, id, attempts, result).await
        });
    }
    while let Some(res) = sends.join_next().await {
        res??;
    }
    Ok(())
}

async fn send(state: &AppState, url: &str, secret: &str, payload: &Value) -> Result<(), String> {
    let body = payload.to_string();
    let res = state
        .http
        .post(url)
        .timeout(SEND_TIMEOUT)
        .header("content-type", "application/json")
        .header("dodo-signature", sign(secret, Utc::now().timestamp(), &body))
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if res.status().is_success() { Ok(()) } else { Err(format!("HTTP {}", res.status())) }
}

/// `attempts` is the count before this attempt.
async fn record_result(state: &AppState, id: Uuid, attempts: i32, result: Result<(), String>) -> sqlx::Result<()> {
    let query = match (result, RETRY_DELAYS_SECS.get(attempts as usize)) {
        (Ok(()), _) => sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempts = attempts + 1, last_error = NULL WHERE id = $1",
        )
        .bind(id),
        (Err(e), Some(&delay)) => sqlx::query(
            "UPDATE webhook_deliveries SET attempts = attempts + 1, last_error = $2,
             next_attempt_at = now() + $3 * interval '1 second' WHERE id = $1",
        )
        .bind(id)
        .bind(e)
        .bind(delay),
        (Err(e), None) => {
            // ponytail: log only; production would alert the business and offer a redeliver endpoint.
            tracing::warn!(delivery_id = %id, error = %e, "webhook retries exhausted; marked failed");
            sqlx::query(
                "UPDATE webhook_deliveries SET status = 'failed', attempts = attempts + 1, last_error = $2 WHERE id = $1",
            )
            .bind(id)
            .bind(e)
        }
    };
    query.execute(&state.pool).await?;
    Ok(())
}
