# Day order replay

This case drives `examples/replay_orders.rs` with good-for-the-day limit orders
and compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains six requests:

1. Day buy 1 @ 100 rests.
2. Day sell 2 @ 101 rests.
3. GTC buy 1 @ 99 rests as a control.
4. `PurgeExpired` with `day_close_ms: null` evicts nothing: a Day order behaves
   like GTC until a close is supplied.
5. `PurgeExpired` with `now_ms: 1000` and `day_close_ms: 1000` evicts both Day
   orders. The GTC bid survives.
6. Cancel the GTC bid to show it is still in the book.

The state machine never reads a clock: the close and the purge timestamp travel
in the request, so every replica expires the same orders.

## Expected events

`expected-events.jsonl` holds six events: three rests, two `expired` events for
the Day orders, and one `cancelled` event for the surviving GTC bid. The
`day_close_ms: null` purge produces no events.

## Run

```shell
./tests/replay/day_orders/run.sh
```

The script uses `127.0.0.1:24215` by default; override it with
`REPLAY_ADDR=127.0.0.1:24315 ./tests/replay/day_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/day_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/day_orders/orders.jsonl \
  --actual tests/replay/day_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24215
```
