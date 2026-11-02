# FOK order replay

This case drives `examples/replay_orders.rs` with fill-or-kill limit orders and
compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains seven requests:

1. Sell 2 @ 100 rests.
2. Sell 2 @ 101 rests.
3. FOK buy 5 @ 100 is refused: only 2 are reachable at or below 100.
4. FOK buy 4 @ 101 executes fully, taking 2 @ 100 and 2 @ 101.
5. FOK sell 3 @ 100 is refused because the bid side is empty.
6. Buy 3 @ 99 rests.
7. FOK sell 3 @ 99 executes fully against that bid.

The case covers an unfillable FOK, a multi-level FOK fill, an FOK with no
liquidity, and a FOK that is exactly fillable. FOK never rests and never
partially fills; an unfillable order leaves the book untouched.

The FOK preflight counts visible and hidden iceberg depth. This case uses plain
limit orders so the reachable volume is easy to read from the file.

## Expected events

`expected-events.jsonl` holds eight events: three rests, three fills and two
`rejected` events with reason `fok_unfillable`.

## Run

```shell
./tests/replay/fok_orders/run.sh
```

The script uses `127.0.0.1:24213` by default; override it with
`REPLAY_ADDR=127.0.0.1:24313 ./tests/replay/fok_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/fok_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/fok_orders/orders.jsonl \
  --actual tests/replay/fok_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24213
```
