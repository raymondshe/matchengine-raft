#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"

ADDR="${REPLAY_ADDR:-127.0.0.1:24218}"

cargo run --quiet --release --example replay_orders -- \
  --orders tests/replay/iceberg_orders/orders.jsonl \
  --expected tests/replay/iceberg_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr "$ADDR"
