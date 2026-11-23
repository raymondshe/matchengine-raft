#!/usr/bin/env bash
# Continuously replay the demo order cases into one order-system target.
#
# Each pass runs scripts/replay-order-system.py, which first cancels leftover
# open orders/stops for the configured user, so the book does not grow without
# bound. Stop with SIGTERM (or kill the PID printed when launched in background).
set -uo pipefail

TARGET="${TARGET:-gbpusd}"
HOSTS="${HOSTS:-http://terminal.match-eu-1.example}"
USERS="${USERS:-1}"
PRICE_SCALE="${PRICE_SCALE:-0.011}"
DELAY="${DELAY:-0.05}"
PAUSE_SECS="${PAUSE_SECS:-5}"

cleanup() {
  echo "continuous replay stopped"
  exit 0
}
trap cleanup TERM INT

cd "$(dirname "$0")/.."
while true; do
  echo "=== replay pass $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
  python3 scripts/replay-order-system.py \
    --hosts ${HOSTS} \
    --users "${USERS}" \
    --target "${TARGET}" \
    --price-scale "${PRICE_SCALE}" \
    --delay "${DELAY}" || echo "replay pass failed; retrying"
  sleep "${PAUSE_SECS}"
done
