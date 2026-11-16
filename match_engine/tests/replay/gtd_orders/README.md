# GTD order replay

This case drives `examples/replay_orders.rs` with good-'til-date limit orders
and compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains five requests:

1. GTD buy 1 @ 100 with deadline `4102444800000` rests.
2. GTD sell 2 @ 101 with the same deadline rests.
3. GTD buy 1 @ 99 with deadline `1` is refused (`gtd_already_expired`)
   because the deadline is not after the order's intake timestamp.
4. `PurgeExpired` one millisecond before the deadline evicts nothing.
5. `PurgeExpired` at the deadline evicts both resting orders.

The deadline is deliberately far in the future (year 2100) so the reference
does not depend on the wall clock. The explicit `PurgeExpired` requests carry
the timestamps through the Raft log, making expiry deterministic on every
replica. Bids are evicted before asks.

## Expected events

`expected-events.jsonl` holds five events: two rests, one `rejected` with
reason `gtd_already_expired`, and two `expired` events carrying the still-open
volume. The before-deadline purge produces no events.

## Run

```shell
./tests/replay/gtd_orders/run.sh
```

The script uses `127.0.0.1:24214` by default; override it with
`REPLAY_ADDR=127.0.0.1:24314 ./tests/replay/gtd_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/gtd_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/gtd_orders/orders.jsonl \
  --actual tests/replay/gtd_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24214
```
