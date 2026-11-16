# Quote-asset fee replay

This case drives `examples/replay_orders.rs` with an ETF/fiat symbol that
settles both sides' fees in the quote asset, and compares the generated match
events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains four requests:

1. `ConfigureSymbol` sets symbol 0 to QQQ/USD with ETF/fiat metadata, 2 price
   decimals, a `0.01` tick, a `0.0001` lot and `fee_asset_policy: quote`.
   Configuration produces no event and does not consume a market sequence.
2. Maker sell 1 @ 400.10 with a 10 bps maker rate and a 20 bps taker rate.
3. Quote buy 1000 USD @ 400.10 with a 30 bps maker rate and a 40 bps taker
   rate. The quote amount normalizes to 2.4993 QQQ shares:
   - fill: notional 400.1, taker fee `1.6004` quote, maker fee `0.4001` quote;
   - rest: the unspent 1.4993 QQQ is inserted as a base bid at 400.10.
4. Sell 1.4993 @ 400.10 with a 20 bps taker rate:
   - fill: notional 599.86993, taker fee `1.19973986` quote;
   - maker fee `1.79960979` quote, using the resting bid's 30 bps maker rate.

An ETF/equity base forces quote fees: both `taker_fee_asset` and
`maker_fee_asset` are `quote` even when the taker is a buyer. Quote-volume limit
orders keep `requested_volume`/`requested_volume_unit` for audit while the
resting order is normalized to base.

## Expected events

`expected-events.jsonl` holds four events: one initial rest, one fill plus the
normalized remainder rest, and one fill against that remainder. The comparison
ignores `event_id.log_index`, so the `ConfigureSymbol` entry does not need to
match a fixed index.

## Run

```shell
./tests/replay/fee_quote_orders/run.sh
```

The script uses `127.0.0.1:24224` by default; override it with
`REPLAY_ADDR=127.0.0.1:24324 ./tests/replay/fee_quote_orders/run.sh`. The node's
gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/fee_quote_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/fee_quote_orders/orders.jsonl \
  --actual tests/replay/fee_quote_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24224
```
