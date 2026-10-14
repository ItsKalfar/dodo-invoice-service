-- Money: BIGINT cents everywhere. States: TEXT + CHECK (easier to evolve than PG enums).

CREATE TABLE businesses (
    id         UUID PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Only the SHA-256 of the key is stored; prefix is the non-secret part shown in UIs/logs.
CREATE TABLE api_keys (
    id          UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id),
    prefix      TEXT NOT NULL,
    key_hash    BYTEA NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX api_keys_business_idx ON api_keys(business_id);

CREATE TABLE customers (
    id          UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id),
    name        TEXT NOT NULL,
    email       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (business_id, id) -- target for the tenant-safe FK below
);
CREATE INDEX customers_business_created_idx ON customers(business_id, created_at DESC);

CREATE TABLE invoices (
    id          UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id),
    customer_id UUID NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('draft', 'open', 'paid', 'void', 'uncollectible')),
    total_cents BIGINT NOT NULL CHECK (total_cents > 0),
    currency    TEXT NOT NULL DEFAULT 'USD' CHECK (currency = 'USD'),
    due_date    DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Composite FK: an invoice can only reference a customer of the same business.
    FOREIGN KEY (business_id, customer_id) REFERENCES customers(business_id, id)
);
CREATE INDEX invoices_business_status_created_idx ON invoices(business_id, status, created_at DESC);

CREATE TABLE line_items (
    id                UUID PRIMARY KEY,
    invoice_id        UUID NOT NULL REFERENCES invoices(id),
    position          INT NOT NULL,
    description       TEXT NOT NULL,
    quantity          BIGINT NOT NULL CHECK (quantity > 0),
    unit_amount_cents BIGINT NOT NULL CHECK (unit_amount_cents >= 0),
    amount_cents      BIGINT GENERATED ALWAYS AS (quantity * unit_amount_cents) STORED,
    UNIQUE (invoice_id, position)
);

CREATE TABLE payment_attempts (
    id           UUID PRIMARY KEY, -- also sent to the PSP as its idempotency key
    invoice_id   UUID NOT NULL REFERENCES invoices(id),
    status       TEXT NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed')),
    amount_cents BIGINT NOT NULL CHECK (amount_cents > 0),
    card_token   TEXT NOT NULL,
    psp_ref      TEXT,
    failure_code TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX payment_attempts_invoice_idx ON payment_attempts(invoice_id);
-- Double-charge guard enforced by the DB: at most one in-flight or successful attempt per invoice.
CREATE UNIQUE INDEX payment_attempts_one_live_per_invoice
    ON payment_attempts(invoice_id) WHERE status IN ('pending', 'succeeded');
-- Sweeper for attempts left pending by PSP timeouts / crashes.
CREATE INDEX payment_attempts_pending_idx ON payment_attempts(created_at) WHERE status = 'pending';

-- response_* NULL = request still in flight.
CREATE TABLE idempotency_keys (
    business_id     UUID NOT NULL REFERENCES businesses(id),
    key             TEXT NOT NULL,
    request_hash    BYTEA NOT NULL,
    response_status INT,
    response_body   JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, key)
);

CREATE TABLE webhook_endpoints (
    id          UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id),
    url         TEXT NOT NULL,
    secret      TEXT NOT NULL, -- needed in plaintext to compute HMAC
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at TIMESTAMPTZ
);
CREATE INDEX webhook_endpoints_business_idx ON webhook_endpoints(business_id);

-- Outbox: rows inserted in the same transaction as the state change; a worker delivers them.
CREATE TABLE webhook_deliveries (
    id              UUID PRIMARY KEY, -- doubles as event id for receiver-side dedupe
    endpoint_id     UUID NOT NULL REFERENCES webhook_endpoints(id),
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries(next_attempt_at) WHERE status = 'pending';
