#!/usr/bin/env bash
# simulate_driver.sh — simulates the driver app for one driver in the local test flow.
#
# Flow:
#   1. Polls app_ride_offers through Supabase REST (as the driver app sees it, RLS-scoped
#      to the signed-in driver) for an open offer or an unstarted assigned delivery
#   2. Accepts it (or taps Start) via POST /drivers/driver-response { offer_id, accepted }
#
# The driver is whoever DRIVER_JWT belongs to; the backend takes the id from the token.
#
# Usage: DRIVER_JWT=... SUPABASE_URL=... SUPABASE_ANON_KEY=... ./scripts/simulate_driver.sh [BASE_URL]
#   BASE_URL defaults to http://localhost:8081
#
# Run this in a separate terminal for each driver BEFORE POST /riders/ride-request.

set -euo pipefail

BASE_URL="${1:-http://localhost:8081}"
: "${DRIVER_JWT:?DRIVER_JWT (Supabase access token of the driver) is required}"
: "${SUPABASE_URL:?SUPABASE_URL is required}"
: "${SUPABASE_ANON_KEY:?SUPABASE_ANON_KEY is required}"

# The token's sub is the driver id (base64url payload, padded for base64 -d).
PAYLOAD=$(echo "$DRIVER_JWT" | cut -d. -f2 | tr '_-' '/+')
while [ $(( ${#PAYLOAD} % 4 )) -ne 0 ]; do PAYLOAD="$PAYLOAD="; done
DRIVER_UUID=$(echo "$PAYLOAD" | base64 -d 2>/dev/null | jq -r '.sub')

echo "[driver:$DRIVER_UUID] Polling for offers every 2s (Ctrl+C to stop)..."
echo ""

# ── Step 1: poll for an offer that still needs an answer ─────────────────────
OFFER=""
while true; do
  NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  OFFER=$(curl -sf -G "$SUPABASE_URL/rest/v1/app_ride_offers" \
    -H "apikey: $SUPABASE_ANON_KEY" \
    -H "Authorization: Bearer $DRIVER_JWT" \
    --data-urlencode "status=in.(pending,accepted)" \
    --data-urlencode "responded_at=is.null" \
    --data-urlencode "expires_at=gt.$NOW" \
    --data-urlencode "order=created_at.desc" \
    --data-urlencode "limit=1" 2>/dev/null | jq -c '.[0] // empty' || true)

  if [ -n "$OFFER" ]; then
    break
  fi
  sleep 2
done

echo "[driver:$DRIVER_UUID] Got offer:"
echo "$OFFER" | jq .
echo ""

OFFER_ID=$(echo "$OFFER" | jq -r '.id')
KIND=$(echo "$OFFER" | jq -r 'if .status == "accepted" then "assigned delivery — tapping Start" else "offer — accepting" end')
echo "[driver:$DRIVER_UUID] $KIND..."

# ── Step 2: answer it ────────────────────────────────────────────────────────
RESULT=$(curl -s -w '\n%{http_code}' -X POST "$BASE_URL/drivers/driver-response" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $DRIVER_JWT" \
  -d "{ \"offer_id\": \"$OFFER_ID\", \"accepted\": true }")

CODE=$(echo "$RESULT" | tail -n1)
BODY=$(echo "$RESULT" | sed '$d')

echo "[driver:$DRIVER_UUID] driver-response ($CODE):"
echo "$BODY" | jq . 2>/dev/null || echo "$BODY"
echo ""

if [ "$CODE" != "200" ]; then
  echo "[driver:$DRIVER_UUID] Answer was refused" >&2
  exit 1
fi

echo "[driver:$DRIVER_UUID] Simulation complete."
