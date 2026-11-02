# Stop-market order replay

This case drives `examples/replay_orders.rs` with stop-market orders and
compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains nine requests:

1. Buy 10 @ 100 and buy 10 @ 95 rest.
2. Sell stop-market 10 with stop 95 parks off the book. Parking emits no event.
3. Sell 10 @ 100 fills the 100 bid; the last trade is 100, so the sell stop at
   95 is not elected.
4. Sell 3 @ 95 fills the 95 bid; the last trade is 95, which elects the stop:
   - `triggered` for 10 units at the 95 trigger price,
   - the child market sell fills 7 @ 95 against the remaining bid,
   - the child cancels its unexecuted 3.
5. Sell stop-market 1 with stop 96 is refused (`stop_would_trigger`) because the
   last trade at 95 already crosses it.
6. Sell 5 @ 110 rests.
7. Buy stop-market 4 with stop 110 parks.
8. Buy 5 @ 110 fills the ask; the last trade is 110, which elects the buy stop:
   - `triggered` for 4 units at 110,
   - the child market buy finds no asks and cancels all 4.

A stop is elected only by a new trade. A stop the last trade already crosses is
refused on entry rather than executed immediately. The child never rests.

## Expected events

`expected-events.jsonl` holds twelve events: two rests, two ordinary fills, two
`triggered` events, two child fills, two child `cancelled` events, one
`rejected` event and one later rest. The parking entries themselves produce no
events.

## Run

```shell
./tests/replay/stop_market_orders/run.sh
```

The script uses `127.0.0.1:24219` by default; override it with
`REPLAY_ADDR=127.0.0.1:24319 ./tests/replay/stop_market_orders/run.sh`. The
node's gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/stop_market_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/stop_market_orders/orders.jsonl \
  --actual tests/replay/stop_market_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24219
```
