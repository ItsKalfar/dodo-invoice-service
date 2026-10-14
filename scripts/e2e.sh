#!/usr/bin/env bash
# End-to-end checks against the running compose stack (app + Postgres + mock PSP).
# Usage: docker compose up -d && scripts/e2e.sh      (works with podman too)
# Takes ~1 minute: the tok_timeout checks wait for the mock PSP's 30 s delay.
set -u

B=${BASE_URL:-http://localhost:8080}
K="Authorization: Bearer ${API_KEY:-sk_test_demo123}"
J="Content-Type: application/json"
CLI=$(command -v docker || command -v podman) || { echo "need docker or podman"; exit 1; }
container() { $CLI ps --format '{{.Names}}' | grep -E "[-_]$1[-_]1$" | head -1; }
DB=$(container db); APP=$(container app); PSP=$(container psp)
[ -n "$DB" ] && [ -n "$APP" ] && [ -n "$PSP" ] || { echo "compose stack not running"; exit 1; }

FAILS=0
check() { # check <name> <actual> <expected>
  if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: got '$2', want '$3'"; FAILS=$((FAILS + 1)); fi
}
sql() { $CLI exec "$DB" psql -U dodo -d dodo -tAc "$1" </dev/null; }
field() { grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4; } # first "key":"value" in JSON
code() { tail -1; }                                                # pay() prints the HTTP code last
new_invoice() {
  local cid
  cid=$(curl -s -X POST $B/customers -H "$K" -H "$J" -d '{"name":"E2E","email":"e2e@example.com"}' | field id)
  IID=$(curl -s -X POST $B/invoices -H "$K" -H "$J" -d "{\"customer_id\":\"$cid\",\"due_date\":\"2026-11-30\",
    \"line_items\":[{\"description\":\"x\",\"quantity\":2,\"unit_amount_cents\":250}]}" | field id)
  curl -s -o /dev/null -X POST $B/invoices/$IID/finalize -H "$K"
  echo "$IID"
}
pay() { # pay <invoice> <token> [idempotency key]
  curl -s -w '\n%{http_code}\n' -X POST $B/invoices/$1/pay -H "$K" -H "$J" \
    -H "Idempotency-Key: ${3:-$(uuidgen)}" -d "{\"card_token\":\"$2\"}"
}
status() { curl -s $B/invoices/$1 -H "$K" | field status; }
wait_paid() { # poll up to $2 seconds
  for _ in $(seq "$2"); do [ "$(status $1)" = paid ] && break; sleep 1; done
  status $1
}

[ "$(curl -s $B/health)" = ok ] || { echo "app not reachable at $B"; exit 1; }
# Webhook sink on the mock PSP, registered once.
[ "$(sql "select count(*) from webhook_endpoints where url like '%webhook-sink'")" = 0 ] &&
  curl -s -o /dev/null -X POST $B/webhook_endpoints -H "$K" -H "$J" -d '{"url":"http://psp:8081/webhook-sink"}'

echo "Invoices"
R=$(curl -s -w '\n%{http_code}' -X POST $B/invoices -H "$K" -H "$J" -d '{"customer_id":"00000000-0000-0000-0000-000000000000","due_date":"2026-11-30","line_items":[],"total_cents":1}')
check "client-sent total rejected" "$(echo "$R" | code)" 400
I=$(new_invoice)
check "total computed by server" "$(curl -s $B/invoices/$I -H "$K" | grep -o '"total_cents":[0-9]*')" '"total_cents":500'
check "finalized invoice is open" "$(status $I)" open
check "invalid transition -> 409" "$(curl -s -X POST $B/invoices/$I/finalize -H "$K" | field code)" invalid_state_transition
check "wrong API key -> 401" "$(curl -s -o /dev/null -w '%{http_code}' $B/invoices/$I -H 'Authorization: Bearer nope')" 401

echo "Payments"
check "decline -> 402" "$(pay $I tok_card_declined | code)" 402
check "declined invoice stays open" "$(status $I)" open
check "success -> 200" "$(pay $I tok_success | code)" 200
check "invoice paid" "$(status $I)" paid
check "pay a paid invoice -> 409" "$(pay $I tok_success | head -1 | field code)" invalid_state_transition

echo "Idempotency"
I=$(new_invoice); KEY=$(uuidgen)
A=$(pay $I tok_success $KEY); A2=$(pay $I tok_success $KEY)
check "same key -> same response" "$A2" "$A"
check "same key -> one attempt (one PSP call)" "$(sql "select count(*) from payment_attempts where invoice_id='$I'")" 1
check "same key, different body -> 422" "$(pay $I tok_card_declined $KEY | head -1 | field code)" idempotency_key_reused

echo "Concurrency: 10 simultaneous pays"
I=$(new_invoice); OUT=$(mktemp)
for _ in $(seq 10); do pay $I tok_success | code >>"$OUT" & done; wait
check "exactly one 200" "$(grep -c 200 "$OUT")" 1
check "the other nine 409" "$(grep -c 409 "$OUT")" 9
check "one succeeded attempt" "$(sql "select count(*) from payment_attempts where invoice_id='$I' and status='succeeded'")" 1
rm -f "$OUT"

echo "PSP failures"
I=$(new_invoice)
check "network drop -> 202 pending" "$(pay $I tok_network_error | code)" 202
check "settled by sweeper" "$(wait_paid $I 20)" paid

T=$(new_invoice)
START=$(date +%s); R=$(pay $T tok_timeout)
check "timeout -> 202 (no hang)" "$(echo "$R" | code)" 202
check "answered within 5 s" "$([ $(($(date +%s) - START)) -le 5 ] && echo yes)" yes
check "attempt pending" "$(echo "$R" | grep -o '"payment_attempt":{.*' | field status)" pending
check "void blocked while pending" "$(curl -s -X POST $B/invoices/$T/void -H "$K" | field code)" payment_in_progress

C=$(new_invoice)
pay $C tok_timeout >/dev/null
echo "  ..  restarting the app mid-payment"
$CLI restart "$APP" >/dev/null
until [ "$(curl -s $B/health)" = ok ]; do sleep 1; done
echo "  ..  waiting for the 30 s PSP timeout to finish"
check "timed-out payment settles" "$(wait_paid $T 60)" paid
check "payment survives app crash" "$(wait_paid $C 60)" paid
check "one charge after crash" "$(sql "select count(*) from payment_attempts where invoice_id='$C' and psp_ref is not null")" 1

echo "Webhooks"
for _ in $(seq 15); do
  [ "$(sql "select count(*) from webhook_deliveries where payload->'data'->>'id'='$C' and status='delivered'")" = 3 ] && break
  sleep 1
done
check "created/finalized/paid delivered" \
  "$(sql "select string_agg(event_type, ',' order by created_at) from webhook_deliveries where payload->'data'->>'id'='$C' and status='delivered'")" \
  invoice.created,invoice.finalized,invoice.paid
# Recompute the HMAC of the last delivery the sink logged, as a receiver would.
L=$($CLI logs "$PSP" 2>&1 | grep "webhook received" | tail -1 | sed 's/\x1b\[[0-9;]*m//g')
SIG=$(echo "$L" | sed 's/.*signature="\([^"]*\)".*/\1/'); TS=${SIG#t=}; TS=${TS%%,*}
BODY=$(echo "$L" | sed 's/.*body="\(.*\)"$/\1/; s/\\"/"/g')
MATCH=no
for SECRET in $(sql "select secret from webhook_endpoints where url like '%webhook-sink'"); do
  [ "$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $NF}')" = "${SIG#*v1=}" ] && MATCH=yes
done
check "signature verifies" "$MATCH" yes

echo
[ $FAILS -eq 0 ] && echo "All checks passed." || echo "$FAILS check(s) failed."
exit $((FAILS > 0))
