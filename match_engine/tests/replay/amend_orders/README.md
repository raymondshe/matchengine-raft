# Amend replay

This case drives `examples/replay_orders.rs` with amend requests and compares
the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains twelve requests:

1. Buy 5 @ 100 and sell 5 @ 101 rest.
2. Amend the bid from 5 to 3 at the same price. It keeps sequence 0 and its time
   priority (`amended` with `previous_sequence: 0` and `order_sequence: 0`).
3. Amend the bid price from 100 to 99. It is re-inserted under the new sequence
   3 (`previous_sequence: 0`).
4. Amend the bid from 3 to 6 at 99. A volume increase re-queues it under
   sequence 4 (`previous_sequence: 3`).
5. Amend the bid price to 102. It would cross the 101 ask and is refused with
   `amend_would_cross`. The rejection reports the key order's current price and
   volume, not the proposed values.
6. Amend sequence 99. No order matches, so it is refused with
   `order_not_found`.
7. Sell iceberg 4 with display 2 @ 103 rests.
8. Amend the iceberg. Icebergs have no amend semantics for their hidden reserve
   and are refused with `unsupported_order`.
9. Cancel the amended bid at sequence 4: 6 units are cancelled.
10. Cancel the ask at sequence 1: 5 units are cancelled.
11. Cancel the iceberg at sequence 7: 4 units are cancelled (2 visible plus 2
    hidden).

Same-price volume reductions keep time priority; price changes and volume
increases re-queue under a new sequence. Crossing amendments are refused rather
than matched.

## Expected events

`expected-events.jsonl` holds twelve events: two rests, three `amended` events,
three `rejected` events, one iceberg rest and three `cancelled` events.

## Run

```shell
./tests/replay/amend_orders/run.sh
```

The script uses `127.0.0.1:24221` by default; override it with
`REPLAY_ADDR=127.0.0.1:24321 ./tests/replay/amend_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/amend_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/amend_orders/orders.jsonl \
  --actual tests/replay/amend_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24221
```
