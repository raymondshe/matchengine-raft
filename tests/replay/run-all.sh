#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="${1:-file}"
case "$MODE" in
  file | --file) MODE=file ;;
  events | --events) MODE=events ;;
  *)
    echo "usage: $0 [file|events]" >&2
    exit 2
    ;;
esac

if [ "$MODE" = "file" ]; then
  for case_dir in tests/replay/*/; do
    case_name="$(basename "$case_dir")"
    [ -x "$case_dir/run.sh" ] || continue
    echo "==> $case_name (file sink)"
    "$case_dir/run.sh"
  done
  exit 0
fi

# The events runner starts one in-process node per case, so it can reuse a
# single address sequentially. EVENTS_LIMIT exercises /events pagination.
ADDR="${EVENTS_REPLAY_ADDR:-127.0.0.1:24202}"
EVENTS_LIMIT="${EVENTS_LIMIT:-1000}"

cargo build --quiet --release --example replay_orders_events
BIN="$ROOT/target/release/examples/replay_orders_events"

for case_dir in tests/replay/*/; do
  case_name="$(basename "$case_dir")"
  [ -f "$case_dir/orders.jsonl" ] || continue
  [ -f "$case_dir/expected-events.jsonl" ] || continue

  # Keep each case's batching: it changes event ordinals inside a log entry.
  batch="$(awk '/--batch-size/ {for (i = 1; i <= NF; i++) if ($i == "--batch-size") print $(i + 1)}' "$case_dir/run.sh" 2>/dev/null | head -1)"
  batch="${batch:-128}"

  echo "==> $case_name (POST /events, batch $batch)"
  "$BIN" \
    --orders "$case_dir/orders.jsonl" \
    --expected "$case_dir/expected-events.jsonl" \
    --batch-size "$batch" \
    --events-limit "$EVENTS_LIMIT" \
    --addr "$ADDR"
done
