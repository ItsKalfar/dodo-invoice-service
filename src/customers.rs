use axum::{
    Json,
    extract::{
        Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{AppState, auth::BusinessId, error::ApiError};

#[derive(Serialize, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateCustomer {
    name: String,
    email: String,
}

pub async fn create(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    body: Result<Json<CreateCustomer>, JsonRejection>,
) -> Result<(StatusCode, Json<Customer>), ApiError> {
    let Json(body) = body?;
    let name = body.name.trim();
    let email = body.email.trim();
    if name.is_empty() {
        return Err(ApiError::Validation("name must not be empty".into()));
    }
    // ponytail: shape check only; real verification would mean sending an email, which is out of scope.
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(ApiError::Validation("email is not valid".into()));
    }

    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4)
         RETURNING id, name, email, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.pool)
    .await?;
    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Customer>, ApiError> {
    let Path(id) = id?;
    sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(business_id)
    .fetch_optional(&state.pool)
    .await?
    .map(Json)
    .ok_or(ApiError::NotFound)
}

pub async fn list(
    State(state): State<AppState>,
    BusinessId(business_id): BusinessId,
) -> Result<Json<Value>, ApiError> {
    // ponytail: fixed cap of 100, no pagination; add a created_at cursor when a business outgrows it.
    let customers = sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers WHERE business_id = $1
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "data": customers })))
}
