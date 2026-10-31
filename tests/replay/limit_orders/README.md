# Regular limit-order replay

This test drives `examples/replay_orders.rs` with a small regular limit-order
scenario and compares the generated match events with a reference file.

## Files

- `orders.jsonl` — five `Place` requests:
  1. Buy 1 @ 100
  2. Buy 1 @ 100
  3. Sell 2 @ 100
  4. Sell 1 @ 100
  5. Buy 2 @ 100

  It exercises price-time priority (the two resting buys fill oldest first), a
  multi-fill taker, a new resting sell, and a partial taker that rests its
  remainder.

- `expected-events.jsonl` — the reference `--match-event-log` JSONL. It was
  generated with `--batch-size 2`; the comparison ignores `node_id` and
  `event_id.log_index`, but keeps ordinals and every event field.

- `run.sh` — starts an in-process node, replays the orders in batches of two,
  and compares the generated JSONL with `expected-events.jsonl`.

## Run

```shell
./tests/replay/limit_orders/run.sh
```

The script uses `127.0.0.1:24211` by default; override it with
`REPLAY_ADDR=127.0.0.1:24212 ./tests/replay/limit_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/limit_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/limit_orders/orders.jsonl \
  --actual tests/replay/limit_orders/expected-events.jsonl \
  --batch-size 2 \
  --addr 127.0.0.1:24211
```
