# Cancel replay

This case drives `examples/replay_orders.rs` with cancel requests and compares
the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains eleven requests:

1. Buy 3 @ 100 rests; sell 2 @ 103 rests.
2. Cancel the bid at sequence 0: 3 units are cancelled.
3. Cancel the same bid again: nothing matches, so no event is published. A
   cancel is a no-op when the order is already gone.
4. Cancel the ask with `symbol_id: 1`. The process trades symbol 0, so the
   request is refused with `unsupported_order` and the real ask stays in the
   book.
5. Park a buy stop-market for 5 with stop 110. Parking emits no event.
6. Cancel the parked stop by side and sequence: 5 units are cancelled. Parked
   stops are keyed by side and sequence because their price is a trigger, not a
   book key.
7. Sell 5 @ 102 rests.
8. Buy 2 @ 102 fills 2 against that ask, leaving 3.
9. Cancel the partially filled ask at sequence 7: 3 units are cancelled, not
   the original 5.
10. Cancel the untouched ask at sequence 1: 2 units are cancelled.

## Expected events

`expected-events.jsonl` holds nine events: three rests, four `cancelled`
events, one `rejected` event and one fill. The no-op cancel and the
stop-parking entry produce no events.

## Run

```shell
./tests/replay/cancel_orders/run.sh
```

The script uses `127.0.0.1:24222` by default; override it with
`REPLAY_ADDR=127.0.0.1:24322 ./tests/replay/cancel_orders/run.sh`. The node's
gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/cancel_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/cancel_orders/orders.jsonl \
  --actual tests/replay/cancel_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24222
```
