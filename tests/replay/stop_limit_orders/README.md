# Stop-limit order replay

This case drives `examples/replay_orders.rs` with stop-limit orders and compares
the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains twelve requests:

1. Buy 10 @ 100, buy 10 @ 96 and buy 10 @ 95 rest.
2. Sell stop-limit 10 with limit 96 and stop 96 parks. Parking emits no event.
3. Sell 10 @ 100 fills the 100 bid.
4. Sell 4 @ 96 fills 4 @ 96; the last trade is 96, which elects the stop:
   - `triggered` for 10 units at 96,
   - the child limit sell 10 @ 96 fills 6 against the rest of the 96 bid,
   - the child cancels 4 because the 95 bid is outside its limit.
5. Sell 3 @ 95 fills 3 @ 95; the last trade becomes 95.
6. Sell stop-limit 1 with limit 97 and stop 97 is refused
   (`stop_would_trigger`) because the last trade at 95 already crosses it.
7. Sell 3 @ 109 and sell 3 @ 110 rest.
8. Buy stop-limit 5 with limit 110 and stop 110 parks.
9. Buy 4 @ 110 fills 3 @ 109 and 1 @ 110; the last trade is 110, which elects
   the buy stop:
   - `triggered` for 5 units at 110,
   - the child limit buy 5 @ 110 fills 2 against the remaining 110 ask,
   - the child cancels 3.

A stop-limit child is bounded by its limit price, so it can fill only the part
of the book at or inside that limit. Whatever it cannot execute is cancelled;
the child never rests.

## Expected events

`expected-events.jsonl` holds seventeen events: five rests, five ordinary
fills, two `triggered` events, two child fills, two child `cancelled` events
and one `rejected` event. The two triggered children show both a partial fill
and a full cancellation.

## Run

```shell
./tests/replay/stop_limit_orders/run.sh
```

The script uses `127.0.0.1:24220` by default; override it with
`REPLAY_ADDR=127.0.0.1:24320 ./tests/replay/stop_limit_orders/run.sh`. The
node's gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/stop_limit_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/stop_limit_orders/orders.jsonl \
  --actual tests/replay/stop_limit_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24220
```
