# API Reference

Base URL: `http://localhost:8080`. Every request and response body is JSON.

## Conventions

- **Auth:** every endpoint except `/health` needs `Authorization: Bearer <api_key>`. Everything
  is scoped to the business that owns the key. Another business's resources return `404`,
  so their ids aren't revealed.
- **IDs:** UUIDs.
- **Money:** USD only, integer cents (`total_cents`, `unit_amount_cents`). The server computes
  every total; clients never send one.
- **Times:** RFC 3339 UTC (`2026-10-14T11:39:30.275750Z`). `due_date` is a plain date (`2026-11-30`).
- **Lists:** `{"data": [...]}`, newest first, at most 100 items, no pagination yet.
- **Unknown fields** in invoice and pay bodies are rejected (`400`). For example, a client-sent
  `total` is an error, not silently ignored.

## Errors

Every error has the same shape:

```json
{"error": {"code": "invalid_state_transition", "message": "cannot finalize an invoice in state 'void'"}}
```

`code` is stable and meant for programs. `message` is for humans and may change.

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | Malformed JSON, unknown field, bad UUID in the path, missing `Idempotency-Key` |
| 401 | `unauthorized` | Missing, invalid or revoked API key |
| 402 | `card_declined`, `insufficient_funds`, `card_token_rejected` | The PSP declined the payment (code comes from the PSP) |
| 404 | `not_found` | No such resource for this business, or unknown route |
| 409 | `invalid_state_transition` | The action isn't allowed from the invoice's current state |
| 409 | `payment_in_progress` | The invoice already has a pending payment (or void/uncollectible was tried while one is pending) |
| 409 | `request_in_progress` | Another request with the same `Idempotency-Key` is still running; retry shortly |
| 422 | `validation_error` | Body is well-formed but a value is invalid (empty name, zero total, unknown customer, …) |
| 422 | `idempotency_key_reused` | The `Idempotency-Key` was already used with a different request |
| 500 | `internal_error` | Unexpected server error (details are logged, not returned) |
| 503 | `psp_unavailable` | Couldn't connect to the PSP; nothing was charged, safe to retry |

---

## Customers

### `POST /customers`

```json
{"name": "Ada Lovelace", "email": "ada@example.com"}
```

`name` must not be empty; `email` must contain `@` and no spaces.

**201**

```json
{"id": "97a4…", "name": "Ada Lovelace", "email": "ada@example.com", "created_at": "2026-10-14T11:39:30.275750Z"}
```

### `GET /customers/{id}`

**200**: a customer object, as above.

### `GET /customers`

**200**: `{"data": [customer, ...]}`

---

## Invoices

### Invoice object

```json
{
  "id": "04f9…",
  "customer_id": "97a4…",
  "status": "open",
  "total_cents": 30000,
  "currency": "USD",
  "due_date": "2026-11-30",
  "created_at": "2026-10-14T11:39:30.291911Z",
  "updated_at": "2026-10-14T11:39:30.311548Z",
  "line_items": [
    {"description": "Consulting", "quantity": 2, "unit_amount_cents": 15000, "amount_cents": 30000}
  ]
}
```

`status` is one of `draft`, `open`, `paid`, `void`, `uncollectible`. The state machine is in
`DESIGN.md` §2.

### `POST /invoices`

```json
{
  "customer_id": "97a4…",
  "due_date": "2026-11-30",
  "line_items": [{"description": "Consulting", "quantity": 2, "unit_amount_cents": 15000}]
}
```

Rules (`422 validation_error` otherwise):

- 1 to 100 line items
- `description` not empty
- `quantity` ≥ 1
- `unit_amount_cents` ≥ 0
- the total is > 0 and fits in an `i64`
- `customer_id` belongs to your business

**201**: the invoice, with `status: "draft"`. Sends `invoice.created`.

### `GET /invoices/{id}`

**200**: the invoice with its line items.

### `GET /invoices?status=open`

`status` is optional. **200**: `{"data": [invoice, ...]}`. List items have **no** `line_items`;
fetch one invoice to see them.

### State transitions

No request body. **200**: the updated invoice. **409 `invalid_state_transition`** if the action
isn't allowed from the current state.

| Endpoint | From | To | Webhook |
|---|---|---|---|
| `POST /invoices/{id}/finalize` | `draft` | `open` | `invoice.finalized` |
| `POST /invoices/{id}/void` | `draft`, `open`, `uncollectible` | `void` | `invoice.voided` |
| `POST /invoices/{id}/mark_uncollectible` | `open` | `uncollectible` | `invoice.marked_uncollectible` |

`void` and `mark_uncollectible` return **409 `payment_in_progress`** while a payment attempt is
pending, because that payment could still succeed.

---

## Payments

### `POST /invoices/{id}/pay`

Headers:

- `Idempotency-Key: <1–255 chars>`: **required**
- `Authorization`, `Content-Type: application/json`

```json
{"card_token": "tok_success"}
```

The invoice must be `open` or `uncollectible`. The amount charged is always the invoice's
`total_cents`.

| Status | Meaning | Body |
|---|---|---|
| **200** | Charged. Invoice is `paid`. Sends `invoice.paid`. | `{"invoice": {...}, "payment_attempt": {...}}` |
| **202** | The PSP didn't answer in time (3 s), or the connection dropped. The attempt is `pending` and the invoice stays `open`. A background sweeper asks the PSP again and settles it; the result arrives as a webhook or via `GET /invoices/{id}`. | `{"invoice": {...}, "payment_attempt": {...}}` |
| **402** | Declined. Attempt `failed`, invoice stays `open` (can be paid again with a new key). Sends `invoice.payment_failed`. | error (`card_declined`, `insufficient_funds`, `card_token_rejected`) |
| **409** | `invalid_state_transition` (e.g. already `paid`), `payment_in_progress` (another attempt is pending), or `request_in_progress` (same key still running) | error |
| **422** | `idempotency_key_reused`: same key, different invoice or card token | error |
| **503** | `psp_unavailable`: couldn't connect to the PSP; nothing was charged | error |

Payment attempt object:

```json
{
  "id": "0504…",
  "invoice_id": "04f9…",
  "status": "succeeded",
  "amount_cents": 30000,
  "psp_ref": "fd19…",
  "failure_code": null,
  "created_at": "2026-10-14T11:39:30.352045Z",
  "updated_at": "2026-10-14T11:39:30.461851Z"
}
```

`status` is `pending`, `succeeded` or `failed`.

**Idempotency:** keys are per business and never expire.

- Same key + same request (same invoice and card token) → the saved response (status and
  body), with no second PSP call.
- Responses are saved for 200, 202, 402, 404, 409 `invalid_state_transition` and 422.
- 500, 503 and 409 `payment_in_progress` are **not** saved; the key is released so the same
  request can be retried.

**Test card tokens** (mock PSP):

| Token | PSP behaviour | API result |
|---|---|---|
| `tok_success` | succeeds after ~100 ms | 200 |
| `tok_insufficient_funds` | fails after ~100 ms | 402 `insufficient_funds` |
| `tok_card_declined` | fails after ~100 ms | 402 `card_declined` |
| `tok_timeout` | sleeps 30 s, then succeeds | 202 `pending`; sweeper settles it to `paid` |
| `tok_network_error` | charges, then drops the connection | 202 `pending`; sweeper settles it to `paid` |
| anything else | rejected by the PSP | 402 `card_token_rejected` |

---

## Webhooks

### `POST /webhook_endpoints`

```json
{"url": "https://example.com/hooks"}
```

`url` must be `http` or `https`. **201**:

```json
{"id": "…", "url": "https://example.com/hooks", "secret": "whsec_…", "created_at": "…"}
```

The `secret` is returned **only once**, in this response. An endpoint only receives events
that happen after it is registered.

### `GET /webhook_endpoints`

**200**: `{"data": [{"id", "url", "created_at"}, ...]}` (no secrets).

### Events

| Type | When | `data` |
|---|---|---|
| `invoice.created` | Invoice created | invoice |
| `invoice.finalized` | `draft` → `open` | invoice |
| `invoice.voided` | → `void` | invoice |
| `invoice.marked_uncollectible` | `open` → `uncollectible` | invoice |
| `invoice.paid` | Payment succeeded (directly or via the sweeper) | invoice + `payment_attempt` |
| `invoice.payment_failed` | Payment declined | invoice + `payment_attempt` |

Delivery body:

```json
{
  "id": "evt uuid",
  "type": "invoice.paid",
  "created_at": "2026-10-14T11:39:30.5Z",
  "data": {"id": "04f9…", "status": "paid", "...": "...", "payment_attempt": {...}}
}
```

Delivery is **at-least-once**: dedupe on `id`. Any 2xx response counts as delivered.
Retries: 1 min, 5 min, 30 min, 2 h, 6 h, 12 h, 24 h (8 attempts over ~45 h), then the
delivery is marked `failed`.

### Verifying the signature

Each delivery has a `Dodo-Signature` header:

```
Dodo-Signature: t=1792064370,v1=5f2b…
```

`v1` is hex `HMAC-SHA256(secret, "<t>.<raw body>")`. To verify:

1. Compute the HMAC over `t` + `.` + the raw request body.
2. Compare it to `v1` in constant time.
3. Reject the request if `t` is more than 5 minutes old (replay protection).

---

## Health

`GET /health` → `200 ok` (no auth).
