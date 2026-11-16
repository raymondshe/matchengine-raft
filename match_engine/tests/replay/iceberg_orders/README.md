# Iceberg order replay

This case drives `examples/replay_orders.rs` with iceberg limit orders and
compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains eight requests:

1. Sell iceberg 10 with display 3 @ 100. Only the 3-unit visible tranche rests;
   the other 7 units are hidden.
2. Buy 5 @ 100 fills 3 from the visible tranche and 2 from the next tranche that
   is drawn out of the hidden reserve.
3. Sell iceberg 4 with display 2 @ 101 rests with a 2-unit visible tranche.
4. Buy 1 @ 101 still executes at 100: the first iceberg has better price
   priority, and it refills its visible tranche from the hidden reserve.
5. Cancel the first iceberg. The `cancelled` event reports 4 units: 3 visible
   plus 1 still hidden.
6. Cancel the second iceberg. It reports 4 units: 2 visible plus 2 hidden.
7. Sell GTD iceberg 6 with display 2 @ 102 rests with a 2-unit visible tranche.
8. `PurgeExpired` at its deadline reports the full 6 units, including the hidden
   reserve.

The second iceberg at 101 is intentionally left untouched to show that a better
priced iceberg with hidden depth keeps priority. Cancel and expiry always
report `open_volume`, not just the visible tranche.

## Expected events

`expected-events.jsonl` holds nine events: three rests (each showing only the
visible tranche), three fills, two `cancelled` events and one `expired` event.
The fill against the first iceberg is split into a 3-unit and a 2-unit print
because the taker consumed a full tranche and part of the refilled one.

## Run

```shell
./tests/replay/iceberg_orders/run.sh
```

The script uses `127.0.0.1:24218` by default; override it with
`REPLAY_ADDR=127.0.0.1:24318 ./tests/replay/iceberg_orders/run.sh`. The node's
gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/iceberg_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/iceberg_orders/orders.jsonl \
  --actual tests/replay/iceberg_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24218
```
