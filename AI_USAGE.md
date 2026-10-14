# AI Usage

## Tools and what I used them for

- **Claude Code (Claude Opus, in the terminal)** for most of the build:
  - **Design proposals:** before each step, Claude explained the options and recommended one;
    I made the call.
  - **Code:** handlers, schema, mock PSP, workers, and the integration tests.
    each with the alternative it rejected. I reviewed it to keep, change or reject each one.
  - **Docs:** first drafts of the README, `API.md` and `DESIGN.md` sections, which I then rewrote.
- **"Ponytail" plugin for Claude Code:** a prompt that pushes Claude toward the smallest working
  code and makes it mark deliberate shortcuts with `ponytail:` comments. Used to keep scope
  small, since the brief rewards restraint.

**How I checked the output:**
- I ran the full test suite against real Postgres.
- I exercised every README curl against `docker compose up`.
- I read the payment path (`src/payments.rs`) line by line, since it's where a mistake costs money.

## Decisions I made myself

**1. The payment sweeper never gives up.** Claude's first design re-checked a `pending` payment
with the PSP for 10 minutes and then stopped. I rejected that: once it stops, the invoice is stuck
for good. It can't be paid (a payment is "in progress") and it can't be voided, and no API exists
to fix it. Now, after 10 minutes, it logs an alert and keeps retrying once a minute.

**2. "Connection refused" counts as a definite failure, on the first call only.** Claude suggested
it, then argued against its own idea when I asked whether it broke our rule that "no answer from
the PSP = unknown". I decided to keep it. "Unknown" should only cover cases where money might have
moved, and a connection that never opened sent nothing. So `/pay` returns `503 psp_unavailable`
and the client can retry. Claude added one safeguard I agreed with: a refused *retry* stays
unknown, because the original request may already have charged.

**3. A startup seed instead of a signup or admin endpoint.** API keys need a business to exist
first. Of the options Claude laid out, I chose to create one demo business and key on startup from
an environment variable (`SEED_API_KEY`). An admin endpoint would add a second auth mechanism that
the spec never asks for, just to create test data.

Other calls I made: the 3 s PSP timeout, returning `202` + `pending` when the PSP doesn't answer,
`409` (not blocking) for an idempotency key that's still in flight, and `tok_network_error` in the
mock charging the card *before* dropping the connection, which is the case that exposes
double-charge bugs.

## What the AI got wrong

**It misread my request about per-state timestamps.** I said invoices only have `created_at`
and `updated_at`, so you can't tell when a bill was paid. I wanted that called out in `DESIGN.md`
as something a real system needs. Claude took it as a request to build the columns and started
planning them. I had to clarify that they're out of scope and belong in "What You Cut".

Also:
**Gave up a sound idea too easily:** in decision 2 above, it dropped a correct suggestion
under light pushback instead of explaining the distinction. I had to work out the reasoning
myself before keeping it.

**Database rule weaker than the app rule:** the schema Claude wrote allowed an invoice total of
zero (`CHECK (total_cents >= 0)`), while the app rejects any total that isn't positive. Nothing could
hit it through the API, but the database is the last line of defence and shouldn't accept what the
app forbids. I spotted the mismatch while reviewing the money rules and had it tightened to `> 0`.
In the same review I asked why amounts are `i64` and not `u64`; I kept `i64`, because Postgres has
no unsigned integers, and negative amounts are already rejected by validation and `CHECK` constraints.
