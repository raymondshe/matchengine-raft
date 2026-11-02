# Market order replay

This case drives `examples/replay_orders.rs` with market orders and compares the
generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains ten requests:

1. Sell 2 @ 100 and sell 3 @ 101 rest.
2. Market buy 4 sweeps both asks: 2 @ 100 and 2 @ 101. One ask unit remains at
   101.
3. Market sell 5 finds no bids and cancels in full.
4. Buy 2 @ 99 and buy 3 @ 98 rest.
5. Market sell 4 fills 2 @ 99 and 2 @ 98. One bid unit remains at 98.
6. Market buy 1 picks up the residual ask at 101.
7. Sell 1 @ 102 rests.
8. Market buy 3 fills 1 @ 102 and cancels the remaining 2.

Market orders ignore their own price, execute at the maker's price and never
rest. Whatever they cannot execute is reported as `cancelled`; the cancelled
event keeps the market order's own price (`0`).

## Expected events

`expected-events.jsonl` holds thirteen events: five rests, six fills and two
cancelled remainders. The fills cover a two-level sweep, a two-level sweep of
the bid side, a residual single-unit fill and a partial fill that leaves a
cancelled remainder.

## Run

```shell
./tests/replay/market_orders/run.sh
```

The script uses `127.0.0.1:24216` by default; override it with
`REPLAY_ADDR=127.0.0.1:24316 ./tests/replay/market_orders/run.sh`. The node's
gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/market_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/market_orders/orders.jsonl \
  --actual tests/replay/market_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24216
```
