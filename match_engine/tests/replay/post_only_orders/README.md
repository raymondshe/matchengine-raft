# Post-only order replay

This case drives `examples/replay_orders.rs` with post-only limit orders and
compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains six requests:

1. Post-only buy 2 @ 100 rests on an empty book.
2. Post-only sell 1 @ 101 rests: it does not cross the 100 bid.
3. Post-only buy 1 @ 101 would cross the 101 ask and is refused with
   `post_only_would_cross`.
4. Post-only sell 1 @ 99 would cross the 100 bid and is refused.
5. Post-only buy 1 @ 100 joins the bid: it matches the best bid price but does
   not cross the 101 ask.
6. Market sell 3 takes liquidity from both post-only bids, 2 @ 100 and 1 @ 100.

Post-only orders never take liquidity. They either rest in full or are refused
outright; nothing partially executes.

## Expected events

`expected-events.jsonl` holds seven events: three rests, two `rejected` events
with reason `post_only_would_cross`, and two fills. The fills show that a
post-only order that rests can still be a maker.

## Run

```shell
./tests/replay/post_only_orders/run.sh
```

The script uses `127.0.0.1:24217` by default; override it with
`REPLAY_ADDR=127.0.0.1:24317 ./tests/replay/post_only_orders/run.sh`. The node's
gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/post_only_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/post_only_orders/orders.jsonl \
  --actual tests/replay/post_only_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24217
```
