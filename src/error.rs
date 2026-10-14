use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::invoices::{Action, Status};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("missing, invalid or revoked API key")]
    Unauthorized,
    #[error("resource not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Validation(String),
    #[error("cannot {} an invoice in state '{}'", action.name(), format!("{from:?}").to_lowercase())]
    InvalidTransition { from: Status, action: Action },
    #[error("a payment for this invoice is in progress; try again shortly")]
    PaymentInProgress,
    #[error("the payment was declined ({0})")]
    PaymentDeclined(String),
    #[error("the payment processor is unreachable; nothing was charged, retry shortly")]
    PspUnavailable,
    #[error("Idempotency-Key was already used with a different request")]
    IdempotencyKeyReused,
    #[error("a request with this Idempotency-Key is still in progress; retry shortly")]
    RequestInProgress,
    #[error("internal server error")]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        ApiError::BadRequest(e.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        ApiError::BadRequest(e.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(e: QueryRejection) -> Self {
        ApiError::BadRequest(e.body_text())
    }
}

impl ApiError {
    pub fn parts(&self) -> (StatusCode, serde_json::Value) {
        let (status, code) = match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, "validation_error"),
            ApiError::InvalidTransition { .. } => {
                (StatusCode::CONFLICT, "invalid_state_transition")
            }
            ApiError::PaymentInProgress => (StatusCode::CONFLICT, "payment_in_progress"),
            ApiError::PaymentDeclined(code) => (StatusCode::PAYMENT_REQUIRED, code.as_str()),
            ApiError::PspUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "psp_unavailable"),
            ApiError::IdempotencyKeyReused => {
                (StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused")
            }
            ApiError::RequestInProgress => (StatusCode::CONFLICT, "request_in_progress"),
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        (
            status,
            json!({ "error": { "code": code, "message": self.to_string() } }),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.parts();
        (status, Json(body)).into_response()
    }
}
