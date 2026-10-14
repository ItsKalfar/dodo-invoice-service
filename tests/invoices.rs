mod common;

use dodo_payments::auth::ensure_business_with_key;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Api {
    base: String,
    http: reqwest::Client,
    key: &'static str,
}

impl Api {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(self.key);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.unwrap();
        (res.status(), res.json().await.unwrap_or(Value::Null))
    }

    async fn new_invoice(&self, customer_id: &str) -> String {
        let (status, inv) = self
            .call(
                Method::POST,
                "/invoices",
                Some(json!({
                    "customer_id": customer_id,
                    "due_date": "2026-11-01",
                    "line_items": [
                        {"description": "Membership", "quantity": 1, "unit_amount_cents": 4000},
                        {"description": "Training", "quantity": 2, "unit_amount_cents": 1500}
                    ]
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{inv}");
        inv["id"].as_str().unwrap().to_owned()
    }
}

#[sqlx::test]
async fn invoice_lifecycle(pool: PgPool) {
    let business_id = ensure_business_with_key(&pool, "A", "sk_test_a").await.unwrap();
    ensure_business_with_key(&pool, "B", "sk_test_b").await.unwrap();
    // A webhook endpoint so we can check events land in the outbox.
    sqlx::query("INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, 'http://x', 's')")
        .bind(Uuid::new_v4())
        .bind(business_id)
        .execute(&pool)
        .await
        .unwrap();

    let base = common::spawn(pool.clone()).await;
    let a = Api { base: base.clone(), http: reqwest::Client::new(), key: "sk_test_a" };
    let b = Api { base, http: reqwest::Client::new(), key: "sk_test_b" };

    let (_, customer) = a.call(Method::POST, "/customers", Some(json!({"name": "Rahul", "email": "r@x.com"}))).await;
    let customer_id = customer["id"].as_str().unwrap();

    // Server computes the total; a client-sent total is rejected.
    let id = a.new_invoice(customer_id).await;
    let (_, inv) = a.call(Method::GET, &format!("/invoices/{id}"), None).await;
    assert_eq!(inv["total_cents"], 7000);
    assert_eq!(inv["status"], "draft");
    assert_eq!(inv["line_items"][1]["amount_cents"], 3000);
    let (status, _) = a
        .call(
            Method::POST,
            "/invoices",
            Some(json!({"customer_id": customer_id, "due_date": "2026-11-01", "total_cents": 1,
                         "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 100}]})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // B can't use A's customer or see A's invoice.
    let (status, _) = b
        .call(
            Method::POST,
            "/invoices",
            Some(json!({"customer_id": customer_id, "due_date": "2026-11-01",
                         "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 100}]})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = b.call(Method::GET, &format!("/invoices/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // draft -> open; finalizing twice is rejected with the standard error.
    let (status, inv) = a.call(Method::POST, &format!("/invoices/{id}/finalize"), None).await;
    assert_eq!((status, inv["status"].as_str()), (StatusCode::OK, Some("open")));
    let (status, err) = a.call(Method::POST, &format!("/invoices/{id}/finalize"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["code"], "invalid_state_transition");
    assert_eq!(err["error"]["message"], "cannot finalize an invoice in state 'open'");

    // A pending payment blocks void and mark_uncollectible.
    sqlx::query("INSERT INTO payment_attempts (id, invoice_id, status, amount_cents, card_token) VALUES ($1, $2, 'pending', 7000, 'tok')")
        .bind(Uuid::new_v4())
        .bind(Uuid::parse_str(&id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    for action in ["void", "mark_uncollectible"] {
        let (status, err) = a.call(Method::POST, &format!("/invoices/{id}/{action}"), None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(err["error"]["code"], "payment_in_progress");
    }
    sqlx::query("UPDATE payment_attempts SET status = 'failed'").execute(&pool).await.unwrap();

    // open -> uncollectible -> void (terminal).
    let (_, inv) = a.call(Method::POST, &format!("/invoices/{id}/mark_uncollectible"), None).await;
    assert_eq!(inv["status"], "uncollectible");
    let (_, inv) = a.call(Method::POST, &format!("/invoices/{id}/void"), None).await;
    assert_eq!(inv["status"], "void");
    let (status, _) = a.call(Method::POST, &format!("/invoices/{id}/void"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A second invoice stays a draft; filter by status.
    let draft_id = a.new_invoice(customer_id).await;
    let (_, list) = a.call(Method::GET, "/invoices?status=draft", None).await;
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![draft_id.as_str()]);
    let (status, _) = a.call(Method::GET, "/invoices?status=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Every change recorded its event, in order.
    let events: Vec<String> = sqlx::query_scalar("SELECT event_type FROM webhook_deliveries ORDER BY created_at")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        events,
        ["invoice.created", "invoice.finalized", "invoice.marked_uncollectible", "invoice.voided", "invoice.created"]
    );
}
