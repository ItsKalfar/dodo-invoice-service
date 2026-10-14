# Architecture

Three containers (`docker compose up`). The `app` runs the API and two background workers; the
`psp` container runs the mock PSP and the demo webhook sink. Details and trade-offs: [`DESIGN.md`](DESIGN.md).

## Services

```mermaid
flowchart LR
    C["Client<br/>(Bearer API key)"]

    subgraph app["app :8080"]
        API["HTTP API<br/>axum"]
        SW["Payment sweeper<br/>every 5 s"]
        WW["Webhook worker<br/>every 1 s"]
    end

    DB[("Postgres :5432<br/>invoices, payment_attempts,<br/>idempotency_keys, webhook_deliveries")]

    subgraph psp["psp :8081"]
        PSP["Mock PSP<br/>POST /charges"]
        SINK["/webhook-sink<br/>logs deliveries"]
    end

    C -->|"REST + Idempotency-Key"| API
    API -->|"one transaction:<br/>state change + outbox row"| DB
    API -->|"charge, 3 s timeout,<br/>attempt id = PSP idempotency key"| PSP
    SW -->|"find pending attempts"| DB
    SW -->|"re-ask with same key"| PSP
    WW -->|"claim due rows<br/>FOR UPDATE SKIP LOCKED"| DB
    WW -->|"HMAC-SHA256 signed POST,<br/>5 s timeout, backoff 1m to 24h"| SINK
```

- The API never waits on webhooks: it only writes an outbox row in the same transaction as the change.
- The PSP is called over HTTP like a real external service. No answer means "unknown", never "failed".
