#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"

ADDR="${REPLAY_ADDR:-127.0.0.1:24213}"

cargo run --quiet --release --example replay_orders -- \
  --orders tests/replay/fok_orders/orders.jsonl \
  --expected tests/replay/fok_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr "$ADDR"
