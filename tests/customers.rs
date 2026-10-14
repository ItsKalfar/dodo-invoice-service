mod common;

use dodo_payments::auth::ensure_business_with_key;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

#[sqlx::test]
async fn auth_and_tenant_isolation(pool: PgPool) {
    ensure_business_with_key(&pool, "A", "sk_test_a").await.unwrap();
    ensure_business_with_key(&pool, "B", "sk_test_b").await.unwrap();
    let base = common::spawn(pool).await;
    let http = reqwest::Client::new();

    // No key / wrong key -> 401 in the standard error shape.
    let res = http.get(format!("{base}/customers")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = http.get(format!("{base}/customers")).bearer_auth("sk_nope").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "unauthorized");

    // Validation error.
    let res = http
        .post(format!("{base}/customers"))
        .bearer_auth("sk_test_a")
        .json(&json!({"name": " ", "email": "x@y.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // A creates a customer and can read it back.
    let res = http
        .post(format!("{base}/customers"))
        .bearer_auth("sk_test_a")
        .json(&json!({"name": "Rahul", "email": "rahul@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let id = res.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_owned();

    let res = http.get(format!("{base}/customers/{id}")).bearer_auth("sk_test_a").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // B cannot see A's customer, by id or in its list.
    let res = http.get(format!("{base}/customers/{id}")).bearer_auth("sk_test_b").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let list: Value = http.get(format!("{base}/customers")).bearer_auth("sk_test_b").send().await.unwrap().json().await.unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 0);
}
