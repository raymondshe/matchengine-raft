#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"

ADDR="${REPLAY_ADDR:-127.0.0.1:24226}"

cargo run --quiet --release --example replay_orders -- \
  --orders tests/replay/fee_overflow_base_orders/orders.jsonl \
  --expected tests/replay/fee_overflow_base_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr "$ADDR"
