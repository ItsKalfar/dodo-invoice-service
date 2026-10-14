use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// Outbox write: one webhook_deliveries row per active endpoint of the business.
/// Must run inside the same transaction as the state change it describes; the webhook worker (step 7) sends them.
pub async fn record(conn: &mut PgConnection, business_id: Uuid, event_type: &str, data: Value) -> sqlx::Result<()> {
    let payload = json!({
        "id": Uuid::new_v4(), // same event id for every endpoint, so receivers can dedupe
        "type": event_type,
        "created_at": Utc::now(),
        "data": data,
    });
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, endpoint_id, event_type, payload)
         SELECT gen_random_uuid(), id, $2, $3 FROM webhook_endpoints
         WHERE business_id = $1 AND disabled_at IS NULL",
    )
    .bind(business_id)
    .bind(event_type)
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(())
}
