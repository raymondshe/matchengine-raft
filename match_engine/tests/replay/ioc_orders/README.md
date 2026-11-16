# IOC order replay

This case drives `examples/replay_orders.rs` with immediate-or-cancel limit
orders and compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains six requests:

1. Sell 5 @ 100 rests.
2. IOC buy 8 @ 100 fills 5 and cancels the remaining 3.
3. IOC sell 4 @ 101 finds no bids and cancels in full.
4. IOC buy 1 @ 99 finds no ask at or below 99 and cancels in full.
5. Buy 2 @ 99 rests.
6. IOC sell 3 @ 99 fills 2 and cancels the remaining 1.

The case covers a partial IOC fill, an IOC with no liquidity, an IOC whose
limit does not cross, and an IOC that consumes a resting bid.

## Expected events

`expected-events.jsonl` holds eight events: two rests, two fills and four
cancelled remainders. IOC orders never rest; every unfilled remainder is
reported as `cancelled` with the volume that could not execute.

## Run

```shell
./tests/replay/ioc_orders/run.sh
```

The script uses `127.0.0.1:24212` by default; override it with
`REPLAY_ADDR=127.0.0.1:24312 ./tests/replay/ioc_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/ioc_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/ioc_orders/orders.jsonl \
  --actual tests/replay/ioc_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24212
```
