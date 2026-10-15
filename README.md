# Dodo Payments: Invoice & Payment Service

A small invoice and payment API in Rust (axum + sqlx + Postgres). It talks to a mock payment
processor (PSP) over HTTP and sends signed webhooks.

- Design and trade-offs: [`DESIGN.md`](DESIGN.md) (diagrams: [`ARCHITECTURE.md`](ARCHITECTURE.md))
- API reference: [`API.md`](API.md)
- How AI was used: [`AI_USAGE.md`](AI_USAGE.md)

## Demo Video

[Watch the walkthrough on Loom](https://www.loom.com/share/2859fb9f5d7a473da194ebfc12d21ec0)

## Run it

```sh
docker compose up --build
```

This starts three containers. No manual steps are needed: migrations run on startup.

Shortcuts (they use `docker`, or `podman` if Docker isn't installed): `make up`, `make down`
(also wipes the database), `make logs` (webhooks arriving), `make test`, `make e2e`.

| Service | Port | What it is |
|---|---|---|
| `app` | 8080 | The API, plus two background workers (webhook delivery, pending-payment sweeper) |
| `psp` | 8081 | Mock PSP (`mock_psp` binary, same image). Also hosts `/webhook-sink`, which logs the webhooks it receives |
| `db` | 5432 | Postgres 17 (`dodo` / `dodo`) |

At startup the app creates a demo business with the API key **`sk_test_demo123`** (set by
`SEED_API_KEY` in `docker-compose.yaml`).

## Try it

The quickest way: open [`requests.http`](requests.http) in VS Code with the REST Client
extension and click **Send Request** on each block, top to bottom. It covers every step below
plus the timeout and network-error cases. Or use curl:

```sh
export KEY="Authorization: Bearer sk_test_demo123"
export JSON="Content-Type: application/json"
```

Optional: register the demo webhook sink first, so you can watch events arrive with
`docker compose logs -f psp`:

```sh
curl -s -X POST localhost:8080/webhook_endpoints -H "$KEY" -H "$JSON" \
  -d '{"url":"http://psp:8081/webhook-sink"}'
# → 201 {"id":"…","url":"http://psp:8081/webhook-sink","secret":"whsec_…","created_at":"…"}
```

### 1. Create a customer

```sh
curl -s -X POST localhost:8080/customers -H "$KEY" -H "$JSON" \
  -d '{"name":"Ada Lovelace","email":"ada@example.com"}'
# → 201 {"id":"<customer_id>","name":"Ada Lovelace","email":"ada@example.com","created_at":"…"}
```

### 2. Create an invoice, then finalize it

The server computes the total from the line items. A client-sent total is rejected.
Invoices start as `draft` and can only be paid once they are `open`.

```sh
curl -s -X POST localhost:8080/invoices -H "$KEY" -H "$JSON" -d '{
  "customer_id": "<customer_id>",
  "due_date": "2026-11-30",
  "line_items": [{"description":"Consulting","quantity":2,"unit_amount_cents":15000}]
}'
# → 201 {"id":"<invoice_id>","status":"draft","total_cents":30000,"currency":"USD",…}

curl -s -X POST localhost:8080/invoices/<invoice_id>/finalize -H "$KEY"
# → 200 {"id":"<invoice_id>","status":"open",…}
```

### 3. Pay: failure case

Each new payment attempt needs a new `Idempotency-Key`. Reusing a key returns the saved
response; reusing it with a different body returns `422 idempotency_key_reused`.

```sh
curl -s -X POST localhost:8080/invoices/<invoice_id>/pay -H "$KEY" -H "$JSON" \
  -H "Idempotency-Key: $(uuidgen)" -d '{"card_token":"tok_card_declined"}'
# → 402 {"error":{"code":"card_declined","message":"the payment was declined (card_declined)"}}
# The invoice stays "open", so it can be paid again.
```

### 4. Pay: success case

```sh
curl -s -X POST localhost:8080/invoices/<invoice_id>/pay -H "$KEY" -H "$JSON" \
  -H "Idempotency-Key: $(uuidgen)" -d '{"card_token":"tok_success"}'
# → 200 {"invoice":{"status":"paid",…},"payment_attempt":{"status":"succeeded","psp_ref":"…",…}}
```

Other test tokens: `tok_insufficient_funds` (402), `tok_timeout` (PSP sleeps 30 s; the API
answers `202` with a `pending` attempt after 3 s, and a background sweeper settles it), and
`tok_network_error` (the PSP charges, then drops the connection; also `202` + `pending`).
`DESIGN.md` §3 explains why.

## Tests

The tests need the compose Postgres. Each test starts its own app and mock PSP in-process.

```sh
docker compose up -d db
DATABASE_URL=postgres://dodo:dodo@localhost:5432/dodo cargo test
```

All three required tests are in `tests/payments.rs`:

| Requirement | Test |
|---|---|
| N concurrent `/pay` calls → at most one success | `concurrent_pays_charge_once` |
| Same idempotency key → same response, no second PSP call | `same_key_replays_without_second_psp_call` |
| `tok_timeout` → invoice not stuck | `psp_timeout_does_not_leave_invoice_stuck` |

Other test files cover customers, invoices and the state machine, the mock PSP, and webhooks.

**End-to-end check** against the running stack (about 1 minute, restarts the `app` container once):

```sh
docker compose up -d
scripts/e2e.sh
```

It checks the following against the real containers, and exits non-zero if any check fails:

- totals are computed by the server, and the state machine rejects invalid moves
- success and decline payments
- idempotent replay, and reusing a key with a different body
- 10 concurrent pays
- `tok_timeout` and `tok_network_error`
- a crash mid-payment (it restarts the app)
- webhook delivery, and verifying a webhook's signature
