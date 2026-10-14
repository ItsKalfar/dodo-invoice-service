use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};

async fn spawn_psp(timeout_delay: Duration) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, dodo_payments::mock_psp::app(timeout_delay)).into_future());
    format!("http://{addr}/charges")
}

async fn charge(http: &reqwest::Client, url: &str, key: &str, token: &str) -> reqwest::Result<(StatusCode, Value)> {
    let res = http
        .post(url)
        .header("Idempotency-Key", key)
        .json(&json!({"amount_cents": 7000, "card_token": token}))
        .send()
        .await?;
    Ok((res.status(), res.json().await?))
}

#[tokio::test]
async fn outcomes_and_idempotency() {
    let url = spawn_psp(Duration::from_millis(500)).await;
    let http = reqwest::Client::new();

    // Success, and the same key returns the same charge (no second charge).
    let (status, first) = charge(&http, &url, "k1", "tok_success").await.unwrap();
    assert_eq!((status, first["status"].as_str()), (StatusCode::OK, Some("succeeded")));
    let (_, again) = charge(&http, &url, "k1", "tok_success").await.unwrap();
    assert_eq!(again["psp_ref"], first["psp_ref"]);

    let (_, res) = charge(&http, &url, "k2", "tok_card_declined").await.unwrap();
    assert_eq!(res, json!({"status": "failed", "code": "card_declined"}));
    let (_, res) = charge(&http, &url, "k3", "tok_insufficient_funds").await.unwrap();
    assert_eq!(res["code"], "insufficient_funds");

    // Timeout: our client gives up early, the PSP keeps going; same key reports
    // "processing" while running, then the final result.
    let short = reqwest::Client::builder().timeout(Duration::from_millis(100)).build().unwrap();
    assert!(charge(&short, &url, "k4", "tok_timeout").await.unwrap_err().is_timeout());
    let (status, res) = charge(&http, &url, "k4", "tok_timeout").await.unwrap();
    assert_eq!((status, res["status"].as_str()), (StatusCode::CONFLICT, Some("processing")));
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, res) = charge(&http, &url, "k4", "tok_timeout").await.unwrap();
    assert_eq!(res["status"], "succeeded");

    // Network error: connection dropped with no response, but the charge happened.
    let err = charge(&http, &url, "k5", "tok_network_error").await.unwrap_err();
    assert!(!err.is_timeout() && !err.is_status(), "{err:?}");
    let (_, res) = charge(&http, &url, "k5", "tok_network_error").await.unwrap();
    assert_eq!(res["status"], "succeeded");

    // Missing key / unknown token.
    let res = http.post(&url).json(&json!({"amount_cents": 1, "card_token": "tok_success"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let (status, _) = charge(&http, &url, "k6", "tok_bogus").await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
