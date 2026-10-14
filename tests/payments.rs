mod common;

use dodo_payments::auth::ensure_business_with_key;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Clone)]
struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    async fn post(&self, path: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = self.http.post(format!("{}{path}", self.base)).bearer_auth("sk_test_a").json(&body);
        if let Some(key) = key {
            req = req.header("Idempotency-Key", key);
        }
        let res = req.send().await.unwrap();
        (res.status(), res.json().await.unwrap())
    }

    /// A finalized (open) invoice of 7000 cents.
    async fn open_invoice(&self) -> String {
        let (_, customer) = self.post("/customers", None, json!({"name": "Ana", "email": "ana@example.com"})).await;
        let (_, invoice) = self
            .post(
                "/invoices",
                None,
                json!({"customer_id": customer["id"], "due_date": "2026-11-01",
                       "line_items": [{"description": "Membership", "quantity": 1, "unit_amount_cents": 7000}]}),
            )
            .await;
        let id = invoice["id"].as_str().unwrap().to_owned();
        let (status, _) = self.post(&format!("/invoices/{id}/finalize"), None, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        id
    }

    async fn pay(&self, invoice: &str, key: &str, token: &str) -> (StatusCode, Value) {
        self.post(&format!("/invoices/{invoice}/pay"), Some(key), json!({"card_token": token})).await
    }
}

async fn setup(pool: PgPool) -> (dodo_payments::AppState, Api, common::PspCalls) {
    ensure_business_with_key(&pool, "A", "sk_test_a").await.unwrap();
    let (state, base, calls) = common::spawn_with_state(pool).await;
    (state, Api { base, http: reqwest::Client::new() }, calls)
}

#[sqlx::test]
async fn success_decline_and_key_rules(pool: PgPool) {
    let (_, api, _) = setup(pool).await;

    // Missing key is rejected before anything happens.
    let invoice = api.open_invoice().await;
    let (status, body) = api.post(&format!("/invoices/{invoice}/pay"), None, json!({"card_token": "tok_success"})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("bad_request")));

    // Decline: 402 with the PSP's code; invoice stays open and can be paid with a new key.
    let (status, body) = api.pay(&invoice, "k1", "tok_insufficient_funds").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::PAYMENT_REQUIRED, Some("insufficient_funds")));

    let (status, paid) = api.pay(&invoice, "k2", "tok_success").await;
    assert_eq!(status, StatusCode::OK, "{paid}");
    assert_eq!(paid["invoice"]["status"], "paid");
    assert_eq!(paid["payment_attempt"]["status"], "succeeded");

    // Same key + same body: identical saved response.
    assert_eq!(api.pay(&invoice, "k2", "tok_success").await, (StatusCode::OK, paid));
    // Same key + different body: rejected.
    let (status, body) = api.pay(&invoice, "k2", "tok_card_declined").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("idempotency_key_reused")));
    // Paying a paid invoice: invalid transition.
    let (status, body) = api.pay(&invoice, "k3", "tok_success").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("invalid_state_transition")));
}

#[sqlx::test]
async fn network_drop_stays_pending_until_sweeper_resolves(pool: PgPool) {
    let (state, api, _) = setup(pool.clone()).await;
    let invoice = api.open_invoice().await;

    // The mock PSP charges and then drops the connection: we don't know, so 202 + pending.
    let (status, body) = api.pay(&invoice, "k1", "tok_network_error").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["payment_attempt"]["status"], "pending");
    assert_eq!(body["invoice"]["status"], "open");

    // A second payment can't start while the first is unresolved.
    let (status, body) = api.pay(&invoice, "k2", "tok_success").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("payment_in_progress")));

    // Past 10 minutes the sweeper only retries once a minute: asked just now, so it's skipped.
    sqlx::query("UPDATE payment_attempts SET created_at = now() - interval '11 minutes', updated_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    dodo_payments::payments::sweep(&state).await.unwrap();
    let pending: String = sqlx::query_scalar("SELECT status FROM payment_attempts").fetch_one(&pool).await.unwrap();
    assert_eq!(pending, "pending");

    // Last asked over a minute ago: the sweeper asks again with the same PSP key and learns the charge succeeded.
    sqlx::query("UPDATE payment_attempts SET updated_at = now() - interval '2 minutes'").execute(&pool).await.unwrap();
    dodo_payments::payments::sweep(&state).await.unwrap();
    let (status, attempts): (String, i64) = sqlx::query_as(
        "SELECT i.status, (SELECT count(*) FROM payment_attempts WHERE invoice_id = i.id) FROM invoices i",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), attempts), ("paid", 1));
}

#[sqlx::test]
async fn psp_unreachable_is_a_definite_failure_only_on_the_first_call(pool: PgPool) {
    ensure_business_with_key(&pool, "A", "sk_test_a").await.unwrap();
    // A port with nothing listening: connections are refused.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let psp_url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let state = dodo_payments::AppState::new(pool.clone(), psp_url, common::PSP_TIMEOUT);
    let api = Api { base: common::serve(dodo_payments::app(state.clone())).await, http: reqwest::Client::new() };
    let invoice = api.open_invoice().await;

    // First call never reached the PSP: 503, attempt failed, invoice still open.
    let (status, body) = api.pay(&invoice, "k1", "tok_success").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("psp_unavailable")));
    let (attempt, code, invoice_status): (String, Option<String>, String) = sqlx::query_as(
        "SELECT a.status, a.failure_code, i.status FROM payment_attempts a JOIN invoices i ON i.id = a.invoice_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempt.as_str(), code.as_deref(), invoice_status.as_str()), ("failed", Some("psp_unavailable"), "open"));

    // Transient: the key was released, so retrying with it runs again (a second failed attempt), not a replay.
    let (status, _) = api.pay(&invoice, "k1", "tok_success").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // A sweeper retry that is refused proves nothing about the original call: stays pending.
    sqlx::query("UPDATE payment_attempts SET status = 'pending', failure_code = NULL, created_at = now() - interval '6 seconds'
                 WHERE id = (SELECT id FROM payment_attempts LIMIT 1)")
        .execute(&pool)
        .await
        .unwrap();
    dodo_payments::payments::sweep(&state).await.unwrap();
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM payment_attempts WHERE status = 'pending'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 1);
}

// ---------- The three required tests (ASSIGNMENT.md "Testing") ----------

/// Required test 1: N concurrent `/pay` calls on one invoice → exactly one success, one PSP charge,
/// and a consistent final state.
#[sqlx::test]
async fn concurrent_pays_charge_once(pool: PgPool) {
    let (_, api, psp_calls) = setup(pool.clone()).await;
    // A webhook endpoint, so we can count invoice.paid events too.
    sqlx::query(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) SELECT gen_random_uuid(), id, 'http://x', 's' FROM businesses",
    )
    .execute(&pool)
    .await
    .unwrap();
    let invoice = api.open_invoice().await;

    // Different keys: like 10 separate clients clicking "pay" at once.
    const N: usize = 10;
    let tasks: Vec<_> = (0..N)
        .map(|i| {
            let (api, invoice) = (api.clone(), invoice.clone());
            tokio::spawn(async move { api.pay(&invoice, &format!("key-{i}"), "tok_success").await })
        })
        .collect();
    let mut successes = 0;
    for task in tasks {
        let (status, body) = task.await.unwrap();
        match status {
            StatusCode::OK => successes += 1,
            // Lost the race: either the winner's attempt is still pending, or the invoice is already paid.
            StatusCode::CONFLICT => assert!(
                matches!(body["error"]["code"].as_str(), Some("payment_in_progress" | "invalid_state_transition")),
                "{body}"
            ),
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(psp_calls.lock().unwrap().len(), 1, "the PSP must be called exactly once");

    let (status, attempts, succeeded, paid_events): (String, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT status FROM invoices),
                (SELECT count(*) FROM payment_attempts),
                (SELECT count(*) FROM payment_attempts WHERE status = 'succeeded'),
                (SELECT count(*) FROM webhook_deliveries WHERE event_type = 'invoice.paid')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), attempts, succeeded, paid_events), ("paid", 1, 1, 1));
}

/// Required test 2: retrying with the same Idempotency-Key returns the same response and does not
/// call the PSP again.
#[sqlx::test]
async fn same_key_replays_without_second_psp_call(pool: PgPool) {
    let (_, api, psp_calls) = setup(pool).await;

    let invoice = api.open_invoice().await;
    let first = api.pay(&invoice, "k1", "tok_success").await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    let again = api.pay(&invoice, "k1", "tok_success").await;
    assert_eq!(again, first, "a retry must get the saved response");
    assert_eq!(psp_calls.lock().unwrap().len(), 1, "a retry must not reach the PSP");

    // Same key sent twice at the same moment: one runs; the other is told to retry (or, if it
    // arrives after the first finished, gets the replay). Still only one PSP call.
    let invoice = api.open_invoice().await;
    let (a, b) = tokio::join!(api.pay(&invoice, "k2", "tok_success"), api.pay(&invoice, "k2", "tok_success"));
    for (status, body) in [&a, &b] {
        assert!(*status == StatusCode::OK || body["error"]["code"] == "request_in_progress", "{status}: {body}");
    }
    assert!(a.0 == StatusCode::OK || b.0 == StatusCode::OK);
    assert_eq!(psp_calls.lock().unwrap().len(), 2);
}

/// Required test 3: the PSP takes longer than our timeout (tok_timeout). `/pay` must not hang, the
/// invoice must not be left in a bad state, and the payment must resolve once the PSP answers.
#[sqlx::test]
async fn psp_timeout_does_not_leave_invoice_stuck(pool: PgPool) {
    let (state, api, psp_calls) = setup(pool.clone()).await;
    let invoice = api.open_invoice().await;

    // 1. No hang: we give up after PSP_TIMEOUT (0.5 s) while the PSP keeps working until 1.5 s.
    let started = std::time::Instant::now();
    let (status, body) = api.pay(&invoice, "k1", "tok_timeout").await;
    assert!(started.elapsed() < common::PSP_TOK_TIMEOUT, "/pay waited for the slow PSP");
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["payment_attempt"]["status"], "pending");
    assert_eq!(body["invoice"]["status"], "open");

    // 2. Result unknown, so nothing may act on the invoice: no second payment, no void.
    let (status, body) = api.pay(&invoice, "k2", "tok_success").await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("payment_in_progress")));
    let (status, body) = api.post(&format!("/invoices/{invoice}/void"), None, json!({})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("payment_in_progress")));

    // 3. Sweeper asks while the PSP is still working: it hears "processing" and does not guess.
    sqlx::query("UPDATE payment_attempts SET created_at = now() - interval '6 seconds'").execute(&pool).await.unwrap();
    dodo_payments::payments::sweep(&state).await.unwrap();
    let attempt: String = sqlx::query_scalar("SELECT status FROM payment_attempts").fetch_one(&pool).await.unwrap();
    assert_eq!(attempt, "pending");

    // 4. The PSP finishes; the next sweep learns the charge succeeded.
    tokio::time::sleep_until((started + common::PSP_TOK_TIMEOUT + std::time::Duration::from_millis(200)).into()).await;
    dodo_payments::payments::sweep(&state).await.unwrap();
    let (status, attempts): (String, i64) =
        sqlx::query_as("SELECT (SELECT status FROM invoices), (SELECT count(*) FROM payment_attempts)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("paid", 1));

    // Three calls reached the PSP (/pay + two sweeps), all with the same key: one charge.
    let calls = psp_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|k| *k == calls[0]), "every call must reuse the attempt's key: {calls:?}");
}
