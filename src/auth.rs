use axum::{extract::FromRequestParts, http::header::AUTHORIZATION, http::request::Parts};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppState, error::ApiError};

pub struct BusinessId(pub Uuid);

impl FromRequestParts<AppState> for BusinessId {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;

        let business_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
        )
        .bind(hash_key(key))
        .fetch_optional(&state.pool)
        .await?;

        business_id.map(BusinessId).ok_or(ApiError::Unauthorized)
    }
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

pub async fn ensure_business_with_key(pool: &PgPool, name: &str, key: &str) -> sqlx::Result<Uuid> {
    let hash = hash_key(key);
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT business_id FROM api_keys WHERE key_hash = $1")
            .bind(&hash)
            .fetch_optional(pool)
            .await?;
    if let Some(id) = existing {
        return Ok(id);
    }

    let business_id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, $2)")
        .bind(business_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO api_keys (id, business_id, prefix, key_hash) VALUES ($1, $2, $3, $4)")
        .bind(Uuid::new_v4())
        .bind(business_id)
        .bind(key.chars().take(12).collect::<String>())
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(business_id)
}
