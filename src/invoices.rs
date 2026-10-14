use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{AppState, auth::BusinessId, error::ApiError, events};

// ---------- State machine ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Finalize,
    Void,
    MarkUncollectible,
    Pay, // applied by the payment flow (step 5), never directly via the API
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Finalize => "finalize",
            Action::Void => "void",
            Action::MarkUncollectible => "mark_uncollectible",
            Action::Pay => "pay",
        }
    }

    fn event(self) -> &'static str {
        match self {
            Action::Finalize => "invoice.finalized",
            Action::Void => "invoice.voided",
            Action::MarkUncollectible => "invoice.marked_uncollectible",
            Action::Pay => "invoice.paid",
        }
    }
}

/// The whole state machine. Anything not listed is an invalid transition. Paid and Void are terminal.
pub fn next_status(from: Status, action: Action) -> Option<Status> {
    match (from, action) {
        (Status::Draft, Action::Finalize) => Some(Status::Open),
        (Status::Draft | Status::Open | Status::Uncollectible, Action::Void) => Some(Status::Void),
        (Status::Open, Action::MarkUncollectible) => Some(Status::Uncollectible),
        (Status::Open | Status::Uncollectible, Action::Pay) => Some(Status::Paid),
        _ => None,
    }
}

// ---------- Data ----------

#[derive(Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: Status,
    pub total_cents: i64,
    pub currency: String,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(skip)]
    pub line_items: Vec<LineItem>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

/// Loads an invoice with its line items, scoped to the business.
pub async fn load(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    let Some(mut invoice) = sqlx::query_as::<_, Invoice>(
        "SELECT id, customer_id, status, total_cents, currency, due_date, created_at, updated_at
         FROM invoices WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    invoice.line_items = sqlx::query_as::<_, LineItem>(
        "SELECT description, quantity, unit_amount_cents, amount_cents
         FROM line_items WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(invoice))
}

// ---------- Create ----------

// deny_unknown_fields: a client-sent `total` is rejected outright rather than silently ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoice {
    customer_id: Uuid,
    due_date: NaiveDate,
    line_items: Vec<NewLineItem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLineItem {
    description: String,
    quantity: i64,
    unit_amount_cents: i64,
}

const MAX_LINE_ITEMS: usize = 100;

/// Server-side total in integer cents. Rejects bad items and overflow instead of wrapping.
pub fn compute_total(items: &[NewLineItem]) -> Result<i64, ApiError> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(ApiError::Validation(format!("line_items must have 1 to {MAX_LINE_ITEMS} entries")));
    }
    let mut total: i64 = 0;
    for (i, item) in items.iter().enumerate() {
        if item.description.trim().is_empty() {
            return Err(ApiError::Validation(format!("line_items[{i}].description must not be empty")));
        }
        if item.quantity < 1 {
            return Err(ApiError::Validation(format!("line_items[{i}].quantity must be at least 1")));
        }
        if item.unit_amount_cents < 0 {
            return Err(ApiError::Validation(format!("line_items[{i}].unit_amount_cents must not be negative")));
        }
        total = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .and_then(|amount| total.checked_add(amount))
            .ok_or_else(|| ApiError::Validation("invoice total is too large".into()))?;
    }
    if total == 0 {
        return Err(ApiError::Validation("invoice total must be greater than zero".into()));
    }
    Ok(total)
}

pub async fn create(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    body: Result<Json<CreateInvoice>, JsonRejection>,
) -> Result<(StatusCode, Json<Invoice>), ApiError> {
    let Json(body) = body?;
    let total = compute_total(&body.line_items)?;
    let id = Uuid::new_v4();

    let mut tx = state.pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_cents, due_date)
         VALUES ($1, $2, $3, 'draft', $4, $5)",
    )
    .bind(id)
    .bind(business_id)
    .bind(body.customer_id)
    .bind(total)
    .bind(body.due_date)
    .execute(&mut *tx)
    .await;
    // The composite FK rejects customers that don't exist or belong to another business.
    if let Err(e) = &inserted
        && e.as_database_error().is_some_and(|d| d.is_foreign_key_violation())
    {
        return Err(ApiError::Validation("customer_id does not match any of your customers".into()));
    }
    inserted?;

    for (position, item) in body.line_items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO line_items (id, invoice_id, position, description, quantity, unit_amount_cents)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(position as i32)
        .bind(item.description.trim())
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .execute(&mut *tx)
        .await?;
    }

    let invoice = load(&mut tx, business_id, id).await?.expect("just inserted");
    events::record(&mut tx, business_id, "invoice.created", json!(invoice)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

// ---------- Read ----------

pub async fn get(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Invoice>, ApiError> {
    let Path(id) = id?;
    let mut conn = state.pool.acquire().await?;
    load(&mut conn, business_id, id).await?.map(Json).ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
pub struct ListParams {
    status: Option<Status>,
}

/// Line items are left out of the list; fetch one invoice to see them.
pub async fn list(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) = params?;
    // ponytail: fixed cap of 100, no pagination; add a created_at cursor when needed.
    let invoices = sqlx::query_as::<_, Invoice>(
        "SELECT id, customer_id, status, total_cents, currency, due_date, created_at, updated_at FROM invoices
         WHERE business_id = $1 AND ($2::text IS NULL OR status = $2)
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(business_id)
    .bind(params.status)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "data": invoices })))
}

// ---------- Transitions ----------

/// Every state change takes a row lock on the invoice first (SELECT ... FOR UPDATE), so concurrent
/// changes to the same invoice run one after another and each sees the result of the previous one.
async fn transition(state: &AppState, business_id: Uuid, id: Uuid, action: Action) -> Result<Json<Invoice>, ApiError> {
    let mut tx = state.pool.begin().await?;
    let current: Status = sqlx::query_scalar("SELECT status FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
        .bind(id)
        .bind(business_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound)?;

    let next = next_status(current, action).ok_or(ApiError::InvalidTransition { from: current, action })?;

    // Can't cancel or write off while money is moving: the payment could still succeed afterwards.
    if matches!(action, Action::Void | Action::MarkUncollectible) {
        let pending: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payment_attempts WHERE invoice_id = $1 AND status = 'pending')")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if pending {
            return Err(ApiError::PaymentInProgress);
        }
    }

    sqlx::query("UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2")
        .bind(next)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let invoice = load(&mut tx, business_id, id).await?.expect("locked above");
    events::record(&mut tx, business_id, action.event(), json!(invoice)).await?;
    tx.commit().await?;
    Ok(Json(invoice))
}

pub async fn finalize(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Invoice>, ApiError> {
    let Path(id) = id?;
    transition(&state, business_id, id, Action::Finalize).await
}

pub async fn void(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Invoice>, ApiError> {
    let Path(id) = id?;
    transition(&state, business_id, id, Action::Void).await
}

pub async fn mark_uncollectible(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Invoice>, ApiError> {
    let Path(id) = id?;
    transition(&state, business_id, id, Action::MarkUncollectible).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_table() {
        use Status::*;
        let all_states = [Draft, Open, Paid, Void, Uncollectible];
        let all_actions = [Action::Finalize, Action::Void, Action::MarkUncollectible, Action::Pay];
        let allowed = [
            (Draft, Action::Finalize, Open),
            (Draft, Action::Void, Void),
            (Open, Action::Void, Void),
            (Open, Action::MarkUncollectible, Uncollectible),
            (Open, Action::Pay, Paid),
            (Uncollectible, Action::Pay, Paid),
            (Uncollectible, Action::Void, Void),
        ];
        for from in all_states {
            for action in all_actions {
                let expected = allowed.iter().find(|(f, a, _)| *f == from && *a == action).map(|(_, _, to)| *to);
                assert_eq!(next_status(from, action), expected, "{from:?} + {action:?}");
            }
        }
    }

    fn item(quantity: i64, unit_amount_cents: i64) -> NewLineItem {
        NewLineItem { description: "x".into(), quantity, unit_amount_cents }
    }

    #[test]
    fn totals() {
        assert_eq!(compute_total(&[item(1, 4000), item(2, 1500)]).unwrap(), 7000);
        assert!(compute_total(&[]).is_err());
        assert!(compute_total(&[item(0, 100)]).is_err());
        assert!(compute_total(&[item(1, -1)]).is_err());
        assert!(compute_total(&[item(1, 0)]).is_err()); // zero total
        assert!(compute_total(&[item(i64::MAX, 2)]).is_err()); // overflow
        assert!(compute_total(&[item(1, i64::MAX), item(1, 1)]).is_err()); // overflow on sum
    }
}
