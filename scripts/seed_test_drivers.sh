#!/usr/bin/env bash
# seed_test_drivers.sh — creates 6 test drivers (3 Lagos, 3 Ekpoma) and sets their locations.
# Usage: ./scripts/seed_test_drivers.sh [BASE_URL]
# Default BASE_URL: http://localhost:8080

set -euo pipefail

BASE_URL="${1:-http://localhost:8081}"

# ── Coordinates ───────────────────────────────────────────────────────────────
LAGOS_LAT="6.442932"
LAGOS_LNG="3.511259"
LAGOS_NAME="Lagos"

EKPOMA_LAT="6.7376"
EKPOMA_LNG="6.2202"
EKPOMA_NAME="Ekpoma"

# ── Helpers ───────────────────────────────────────────────────────────────────
create_driver() {
  local name="$1" email="$2" phone="$3" pubkey="$4"
  local http_code body
  body=$(curl -s -o /tmp/create_driver_resp.txt -w "%{http_code}" -X POST "$BASE_URL/admin/create-drivers" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$name\",\"email\":\"$email\",\"phone\":\"$phone\",\"driver_pubkey\":\"$pubkey\",\"vehicle_type\":\"Bike\",\"vehicle\":\"Honda CB125\"}")
  http_code="$body"
  local resp
  resp=$(cat /tmp/create_driver_resp.txt)
  if [ "$http_code" = "200" ]; then
    echo "  created: $name"
  else
    echo "  skipped: $name (HTTP $http_code — $resp)"
  fi
}

get_driver_id() {
  local email="$1"
  curl -sf "$BASE_URL/admin/get-drivers" \
    | jq -r ".[] | select(.email==\"$email\") | .driver_id"
}

set_location() {
  local driver_id="$1" lat="$2" lng="$3" loc_name="$4"
  local resp
  resp=$(curl -sf -X POST "$BASE_URL/matching/process-geolocation/$driver_id" \
    -H "Content-Type: application/json" \
    -d "{\"lat\": $lat, \"lng\": $lng, \"name\": \"$loc_name\", \"kind\": \"driver_location\"}")
  echo "  location set for $driver_id → $loc_name ($resp)"
}

# ── Create Lagos drivers ──────────────────────────────────────────────────────
echo ""
echo "=== Creating Lagos drivers ==="
create_driver "Emeka Lagos"   "emeka.lagos@test.com"   "+2348011111101" "pubkey_emeka_lagos"
create_driver "Chidi Lagos"   "chidi.lagos@test.com"   "+2348011111102" "pubkey_chidi_lagos"
create_driver "Tunde Lagos"   "tunde.lagos@test.com"   "+2348011111103" "pubkey_tunde_lagos"

# ── Create Ekpoma drivers ─────────────────────────────────────────────────────
echo ""
echo "=== Creating Ekpoma drivers ==="
create_driver "Osagie Ekpoma" "osagie.ekpoma@test.com" "+2348011111201" "pubkey_osagie_ekpoma"
create_driver "Igho Ekpoma"   "igho.ekpoma@test.com"   "+2348011111202" "pubkey_igho_ekpoma"
create_driver "Efosa Ekpoma"  "efosa.ekpoma@test.com"  "+2348011111203" "pubkey_efosa_ekpoma"

# ── Resolve UUIDs ─────────────────────────────────────────────────────────────
echo ""
echo "=== Resolving driver UUIDs ==="

LAGOS_IDS=()
for email in "emeka.lagos@test.com" "chidi.lagos@test.com" "tunde.lagos@test.com"; do
  id=$(get_driver_id "$email")
  if [ -z "$id" ]; then
    echo "ERROR: could not find driver with email $email" >&2
    exit 1
  fi
  LAGOS_IDS+=("$id")
  echo "  $email → $id"
done

EKPOMA_IDS=()
for email in "osagie.ekpoma@test.com" "igho.ekpoma@test.com" "efosa.ekpoma@test.com"; do
  id=$(get_driver_id "$email")
  if [ -z "$id" ]; then
    echo "ERROR: could not find driver with email $email" >&2
    exit 1
  fi
  EKPOMA_IDS+=("$id")
  echo "  $email → $id"
done

# ── Set locations ─────────────────────────────────────────────────────────────
echo ""
echo "=== Setting Lagos locations ==="
for id in "${LAGOS_IDS[@]}"; do
  set_location "$id" "$LAGOS_LAT" "$LAGOS_LNG" "$LAGOS_NAME"
done

echo ""
echo "=== Setting Ekpoma locations ==="
for id in "${EKPOMA_IDS[@]}"; do
  set_location "$id" "$EKPOMA_LAT" "$EKPOMA_LNG" "$EKPOMA_NAME"
done

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "=== Done. Driver UUIDs ==="
echo ""
echo "Lagos drivers (use for Lagos ride requests):"
for id in "${LAGOS_IDS[@]}"; do
  echo "  $id"
done

echo ""
echo "Ekpoma drivers (use for Ekpoma ride requests):"
for id in "${EKPOMA_IDS[@]}"; do
  echo "  $id"
done

echo ""
echo "Run simulate_driver.sh for each UUID in a separate terminal before triggering a ride."
echo "Example: ./scripts/simulate_driver.sh $BASE_URL ${LAGOS_IDS[0]}"
