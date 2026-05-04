#!/usr/bin/env bash
# simulate_driver.sh — simulates the driver app for one driver in the local test flow.
#
# Flow:
#   1. Long-polls GET /drivers/notify-driver/{uuid}  (blocks until assign_driver fires)
#   2. Auto-accepts via POST /drivers/driver-response
#   3. Feeds DriverResponsePayloadOut to POST /riders/wait-driver-response
#
# Usage: ./scripts/simulate_driver.sh [BASE_URL] <DRIVER_UUID>
# Example: ./scripts/simulate_driver.sh http://localhost:8080 e5f2a1b3-...
#
# Run this in a separate terminal for each driver you want to simulate BEFORE
# the rider/Zazu triggers POST /riders/assign-driver.

set -euo pipefail

if [ $# -lt 1 ]; then
  echo "Usage: $0 [BASE_URL] <DRIVER_UUID>" >&2
  echo "  BASE_URL defaults to http://localhost:8081" >&2
  exit 1
fi

# Accept optional BASE_URL as first arg if it starts with http
if [[ "$1" == http* ]]; then
  BASE_URL="$1"
  DRIVER_UUID="${2:?Driver UUID required as second argument}"
else
  BASE_URL="http://localhost:8081"
  DRIVER_UUID="$1"
fi

echo "[driver:$DRIVER_UUID] Starting long-poll loop..."
echo "[driver:$DRIVER_UUID] Endpoint: GET $BASE_URL/drivers/notify-driver/$DRIVER_UUID"
echo "[driver:$DRIVER_UUID] (Ctrl+C to stop)"
echo ""

# ── Step 1: long-poll loop — reconnects automatically if connection drops ─────
RIDE_REQUEST=""
while true; do
  echo "[driver:$DRIVER_UUID] Waiting for ride request..."
  RIDE_REQUEST=$(curl -sf --max-time 0 \
    "$BASE_URL/drivers/notify-driver/$DRIVER_UUID" 2>/dev/null)

  if [ -n "$RIDE_REQUEST" ]; then
    break  # got a ride request, proceed
  fi

  echo "[driver:$DRIVER_UUID] Connection dropped or server error — reconnecting in 3s..."
  sleep 3
done

echo "[driver:$DRIVER_UUID] Got ride request:"
echo "$RIDE_REQUEST" | jq .
echo ""

# Extract rider_id from the NewRideRequest JSON
RIDER_ID=$(echo "$RIDE_REQUEST" | jq -r '.rider_id // empty')

if [ -z "$RIDER_ID" ]; then
  echo "[driver:$DRIVER_UUID] ERROR: could not extract rider_id from ride request" >&2
  echo "Raw response: $RIDE_REQUEST" >&2
  exit 1
fi

echo "[driver:$DRIVER_UUID] rider_id=$RIDER_ID — auto-accepting..."
echo ""

# ── Step 2: call driver-response to accept the ride ──────────────────────────
DRIVER_RESPONSE=$(curl -sf -X POST "$BASE_URL/drivers/driver-response" \
  -H "Content-Type: application/json" \
  -d "{
    \"driver_id\": \"$DRIVER_UUID\",
    \"rider_id\":  \"$RIDER_ID\",
    \"response\":  \"accepted\"
  }")

if [ -z "$DRIVER_RESPONSE" ]; then
  echo "[driver:$DRIVER_UUID] ERROR: empty response from /drivers/driver-response" >&2
  exit 1
fi

echo "[driver:$DRIVER_UUID] driver-response result:"
echo "$DRIVER_RESPONSE" | jq .
echo ""

# Check that we got an "accepted" status back
STATUS=$(echo "$DRIVER_RESPONSE" | jq -r '.status // empty')
if [ "$STATUS" != "accepted" ]; then
  echo "[driver:$DRIVER_UUID] WARNING: unexpected status '$STATUS' — response may have been rejected by server" >&2
fi

# ── Step 3: feed DriverResponsePayloadOut to assign_driver's oneshot channel ─
echo "[driver:$DRIVER_UUID] Forwarding acceptance to /riders/wait-driver-response..."

ASSIGN_RESULT=$(curl -sf -X POST "$BASE_URL/riders/wait-driver-response" \
  -H "Content-Type: application/json" \
  -d "$DRIVER_RESPONSE")

echo "[driver:$DRIVER_UUID] assign_driver result:"
echo "$ASSIGN_RESULT" | jq . 2>/dev/null || echo "$ASSIGN_RESULT"
echo ""

echo "[driver:$DRIVER_UUID] Simulation complete."
