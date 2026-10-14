//! `POST /invoices/{id}/pay`, plus the sweeper that resolves attempts the PSP never answered.
//!
//! Core rule: no answer from the PSP means "unknown", never "failed". The attempt id is the PSP
//! idempotency key, so asking again later can never charge twice. One exception: if the very
//! first call can't even open a connection, the request was never sent, so that is a definite failure.

use std::time::Duration;

use axum::{
    Json,
    extract::{
        Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    AppState,
    auth::BusinessId,
    error::ApiError,
    events,
    invoices::{self, Action, Status, next_status},
};

#[derive(Serialize, sqlx::FromRow)]
pub struct Attempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub status: String,
    pub amount_cents: i64,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

async fn load_attempt(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Attempt> {
    sqlx::query_as(
        "SELECT id, invoice_id, status, amount_cents, psp_ref, failure_code, created_at, updated_at
         FROM payment_attempts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(conn)
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayRequest {
    card_token: String,
}

pub async fn pay(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    headers: HeaderMap,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<PayRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path(invoice_id) = id?;
    let Json(body) = body?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 255)
        .ok_or_else(|| {
            ApiError::BadRequest("Idempotency-Key header is required (1 to 255 characters)".into())
        })?
        .to_owned();
    if body.card_token.trim().is_empty() {
        return Err(ApiError::Validation("card_token must not be empty".into()));
    }
    let request_hash = Sha256::digest(format!("pay {invoice_id} {}", body.card_token)).to_vec();

    let claimed = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(business_id)
    .bind(&key)
    .bind(&request_hash)
    .execute(&state.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        let saved: Option<(Vec<u8>, Option<i32>, Option<Value>)> = sqlx::query_as(
            "SELECT request_hash, response_status, response_body FROM idempotency_keys WHERE business_id = $1 AND key = $2",
        )
        .bind(business_id)
        .bind(&key)
        .fetch_optional(&state.pool)
        .await?;
        return match saved {
            Some((hash, _, _)) if hash != request_hash => Err(ApiError::IdempotencyKeyReused),
            Some((_, Some(status), Some(body))) => {
                let status = StatusCode::from_u16(status as u16).map_err(anyhow::Error::from)?;
                Ok((status, Json(body)))
            }
            _ => Err(ApiError::RequestInProgress),
        };
    }

    // Spawned so the work finishes even if the client disconnects mid-request; otherwise the
    // key would stay "in progress" forever.
    let task = tokio::spawn(async move {
        let result = pay_invoice(&state, business_id, invoice_id, &body.card_token).await;
        let transient = matches!(
            result,
            Err(ApiError::Internal(_) | ApiError::PaymentInProgress | ApiError::PspUnavailable)
        );
        let (status, body) = result.unwrap_or_else(|e| e.parts());
        if transient {
            // Release the key so the same request can be retried. Safe: the DB index still allows
            // only one pending/succeeded attempt per invoice.
            sqlx::query("DELETE FROM idempotency_keys WHERE business_id = $1 AND key = $2")
                .bind(business_id)
                .bind(&key)
                .execute(&state.pool)
                .await?;
        } else {
            sqlx::query(
                "UPDATE idempotency_keys SET response_status = $3, response_body = $4 WHERE business_id = $1 AND key = $2",
            )
            .bind(business_id)
            .bind(&key)
            .bind(status.as_u16() as i32)
            .bind(&body)
            .execute(&state.pool)
            .await?;
        }
        Ok::<_, ApiError>((status, Json(body)))
    });
    task.await.map_err(anyhow::Error::from)?
}

async fn pay_invoice(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    card_token: &str,
) -> Result<(StatusCode, Value), ApiError> {
    let mut tx = state.pool.begin().await?;
    let (current, total_cents): (Status, i64) = sqlx::query_as(
        "SELECT status, total_cents FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    next_status(current, Action::Pay).ok_or(ApiError::InvalidTransition {
        from: current,
        action: Action::Pay,
    })?;

    let attempt_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, status, amount_cents, card_token) VALUES ($1, $2, 'pending', $3, $4)",
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(total_cents)
    .bind(card_token)
    .execute(&mut *tx)
    .await;
    if let Err(e) = &inserted
        && e.as_database_error()
            .is_some_and(|d| d.is_unique_violation())
    {
        return Err(ApiError::PaymentInProgress);
    }
    inserted?;
    tx.commit().await?;

    let outcome = charge(state, attempt_id, total_cents, card_token, true).await;
    settle(state, attempt_id, &outcome).await?;
    if matches!(outcome, Outcome::NotSent) {
        return Err(ApiError::PspUnavailable);
    }

    let mut conn = state.pool.acquire().await?;
    let invoice = invoices::load(&mut conn, business_id, invoice_id)
        .await?
        .expect("exists");
    let attempt = load_attempt(&mut conn, attempt_id).await?;
    let body = json!({ "invoice": invoice, "payment_attempt": attempt });
    Ok(match outcome {
        Outcome::Succeeded(_) => (StatusCode::OK, body),
        Outcome::Failed(code) => ApiError::PaymentDeclined(code).parts(),
        Outcome::Unknown => (StatusCode::ACCEPTED, body),
        Outcome::NotSent => unreachable!("returned above"),
    })
}

// ---------- PSP ----------

enum Outcome {
    Succeeded(String), // psp_ref
    Failed(String),    // decline code
    NotSent,           // first call couldn't connect: the PSP never saw this attempt
    Unknown,
}

async fn charge(
    state: &AppState,
    attempt_id: Uuid,
    amount_cents: i64,
    card_token: &str,
    first_call: bool,
) -> Outcome {
    let res = state
        .psp
        .post(format!("{}/charges", state.psp_url))
        .header("Idempotency-Key", attempt_id.to_string())
        .json(&json!({ "amount_cents": amount_cents, "card_token": card_token }))
        .send()
        .await;
    let res = match res {
        Ok(res) => res,
        Err(e) if first_call && e.is_connect() && !e.is_timeout() => {
            tracing::warn!(%attempt_id, error = %e, "PSP unreachable; request never sent");
            return Outcome::NotSent;
        }
        Err(e) => {
            tracing::warn!(%attempt_id, error = %e, "PSP gave no answer; attempt stays pending");
            return Outcome::Unknown;
        }
    };
    let status = res.status();
    let body: Value = match res.json().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(%attempt_id, %status, error = %e, "unreadable PSP response; attempt stays pending");
            return Outcome::Unknown;
        }
    };
    match (status, body["status"].as_str()) {
        (StatusCode::OK, Some("succeeded")) => {
            Outcome::Succeeded(body["psp_ref"].as_str().unwrap_or_default().into())
        }
        (StatusCode::OK, Some("failed")) => {
            Outcome::Failed(body["code"].as_str().unwrap_or("card_declined").into())
        }
        (s, _) if s.is_client_error() && s != StatusCode::CONFLICT => {
            Outcome::Failed("card_token_rejected".into())
        }
        _ => Outcome::Unknown,
    }
}

async fn settle(state: &AppState, attempt_id: Uuid, outcome: &Outcome) -> Result<(), ApiError> {
    let (status, psp_ref, failure_code, event) = match outcome {
        Outcome::Succeeded(psp_ref) => ("succeeded", Some(psp_ref.as_str()), None, "invoice.paid"),
        Outcome::Failed(code) => (
            "failed",
            None,
            Some(code.as_str()),
            "invoice.payment_failed",
        ),
        Outcome::NotSent => (
            "failed",
            None,
            Some("psp_unavailable"),
            "invoice.payment_failed",
        ),
        Outcome::Unknown => return Ok(()),
    };

    let mut tx = state.pool.begin().await?;
    let (business_id, invoice_id, current): (Uuid, Uuid, Status) = sqlx::query_as(
        "SELECT i.business_id, i.id, i.status FROM invoices i
         JOIN payment_attempts a ON a.invoice_id = i.id WHERE a.id = $1 FOR UPDATE OF i",
    )
    .bind(attempt_id)
    .fetch_one(&mut *tx)
    .await?;

    let updated = sqlx::query(
        "UPDATE payment_attempts SET status = $2, psp_ref = $3, failure_code = $4, updated_at = now()
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(psp_ref)
    .bind(failure_code)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(());
    }

    if matches!(outcome, Outcome::Succeeded(_)) {
        let next = next_status(current, Action::Pay).ok_or(ApiError::InvalidTransition {
            from: current,
            action: Action::Pay,
        })?;
        sqlx::query("UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2")
            .bind(next)
            .bind(invoice_id)
            .execute(&mut *tx)
            .await?;
    }

    let invoice = invoices::load(&mut tx, business_id, invoice_id)
        .await?
        .expect("locked above");
    let attempt = load_attempt(&mut tx, attempt_id).await?;
    let mut data = json!(invoice);
    data["payment_attempt"] = json!(attempt);
    events::record(&mut tx, business_id, event, data).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn run_sweeper(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        if let Err(e) = sweep(&state).await {
            tracing::error!(error = ?e, "payment sweeper failed");
        }
    }
}

pub async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let stuck: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payment_attempts WHERE status = 'pending' AND created_at < now() - interval '10 minutes'",
    )
    .fetch_one(&state.pool)
    .await?;
    if stuck > 0 {
        tracing::error!(
            stuck,
            "payment attempts unresolved for over 10 minutes; check the PSP (still retrying every minute)"
        );
    }

    let due: Vec<(Uuid, i64, String)> = sqlx::query_as(
        "SELECT id, amount_cents, card_token FROM payment_attempts
         WHERE status = 'pending' AND created_at < now() - interval '5 seconds'
           AND (created_at > now() - interval '10 minutes' OR updated_at < now() - interval '1 minute')
         ORDER BY updated_at LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await?;
    for (attempt_id, amount_cents, card_token) in due {
        let outcome = charge(state, attempt_id, amount_cents, &card_token, false).await;
        if matches!(outcome, Outcome::Unknown) {
            sqlx::query("UPDATE payment_attempts SET updated_at = now() WHERE id = $1 AND status = 'pending'")
                .bind(attempt_id)
                .execute(&state.pool)
                .await?;
        }
        settle(state, attempt_id, &outcome).await?;
    }
    Ok(())
}
