#!/usr/bin/env bash
# Mock checkout site — curl-driven acceptance test (bead till-6q0, SPEC §9).
#
# Hits all four modes, asserts each behaves as specified, and proves the
# /stats submission counter survives a restart of the site.
#
#   ./script/mock-checkout-curl.sh
#
# `script/checks` (what CI runs) covers the same logic from `cargo test`; this
# script is the end-to-end form of the acceptance criteria and needs only curl.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${MOCK_CHECKOUT_TEST_PORT:-9391}"
BASE="http://127.0.0.1:${PORT}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
WORK_DIR="$(mktemp -d)"
STATS_FILE="${WORK_DIR}/stats.jsonl"
SERVER_PID=""

# Placeholder values only — never a real (or real-looking) PAN.
FORM_DATA='card_number=0000-0000-0000-0000&expiry=00%2F00&cvv=000&cardholder_name=Placeholder+Only&billing_postcode=0000'

cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

start_server() {
  MOCK_CHECKOUT_BIND="127.0.0.1:${PORT}" \
  MOCK_CHECKOUT_STATS_FILE="$STATS_FILE" \
    "$TARGET_DIR/debug/mock-checkout" >"${WORK_DIR}/server.log" 2>&1 &
  SERVER_PID=$!

  for _ in $(seq 1 50); do
    if curl -fsS "${BASE}/stats" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      cat "${WORK_DIR}/server.log" >&2 || true
      fail "server exited before it became ready"
    fi
    sleep 0.2
  done
  cat "${WORK_DIR}/server.log" >&2 || true
  fail "server did not become ready on ${BASE}"
}

stop_server() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=""
}

status_of() {
  curl -sS -o /dev/null -w '%{http_code}' "$@"
}

echo "building mock-checkout…"
cargo build --quiet -p mock-checkout

start_server

# --- the form carries the spec field_kinds -------------------------------
form="$(curl -fsS "${BASE}/checkout?mode=success")"
for field in card_number expiry cvv cardholder_name billing_postcode; do
  grep -q "id=\"${field}\"" <<<"$form" || fail "form is missing input id=${field}"
done
grep -q 'id="submit"' <<<"$form" || fail "form is missing the submit button"
echo "ok   checkout form matches the spec field_kinds"

# --- success -------------------------------------------------------------
success_page="$(curl -fsS -X POST --data "$FORM_DATA" "${BASE}/checkout?mode=success")"
grep -q 'id="order-reference"' <<<"$success_page" || fail "success mode showed no order reference"
echo "ok   mode=success → order reference"

# --- declined ------------------------------------------------------------
declined_page="$(curl -fsS -X POST --data "$FORM_DATA" "${BASE}/checkout?mode=declined")"
grep -q 'id="card-declined"' <<<"$declined_page" || fail "declined mode showed no card-declined error"
grep -q 'order-reference' <<<"$declined_page" && fail "declined mode must not show an order reference"
echo "ok   mode=declined → card declined error"

# --- ambiguous -----------------------------------------------------------
ambiguous_page="$(curl -fsS -X POST --data "$FORM_DATA" "${BASE}/checkout?mode=ambiguous")"
grep -q 'id="processing-spinner"' <<<"$ambiguous_page" || fail "ambiguous mode showed no processing spinner"
echo "ok   mode=ambiguous → indefinite processing spinner"

# --- short_hold: inside the TTL -----------------------------------------
curl -fsS "${BASE}/checkout?mode=short_hold&token=inside&hold_seconds=60" >/dev/null \
  || fail "short_hold checkout form failed to load"
hold_page="$(curl -fsS -X POST --data "$FORM_DATA" "${BASE}/checkout?mode=short_hold&token=inside")"
grep -q 'id="order-reference"' <<<"$hold_page" || fail "short_hold submit inside the TTL should behave like success"
echo "ok   mode=short_hold inside the TTL → order reference"

# --- short_hold: past the TTL -------------------------------------------
curl -fsS "${BASE}/checkout?mode=short_hold&token=outside&hold_seconds=0" >/dev/null \
  || fail "short_hold checkout form failed to load"
expired_status="$(status_of -X POST --data "$FORM_DATA" "${BASE}/checkout?mode=short_hold&token=outside")"
[ "$expired_status" = "410" ] || fail "expired short_hold submit should answer 410, got ${expired_status}"
echo "ok   mode=short_hold past the TTL → 410 checkout expired"

# --- /stats counts accepted submits only --------------------------------
stats_json="$(curl -fsS "${BASE}/stats")"
grep -q '"total":4' <<<"$stats_json" || fail "/stats should report 4 accepted submits, got: ${stats_json}"
echo "ok   /stats counts each submit exactly once"

# --- /stats survives a restart -----------------------------------------
stop_server
start_server
stats_json="$(curl -fsS "${BASE}/stats")"
grep -q '"total":4' <<<"$stats_json" || fail "/stats lost its count across a restart, got: ${stats_json}"
echo "ok   /stats count survives a restart"

echo "mock-checkout curl acceptance: PASS"
