# Fee overflow replay

This case drives `examples/replay_orders.rs` with a fill whose notional cannot
be represented at the scale-18 fixed-point range, and compares the generated
match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains three requests:

1. `ConfigureSymbol` sets symbol 0 to QQQ/USD with the ETF/fiat quote-fee
   policy. Configuration produces no event and does not consume a sequence.
2. Maker sell 10,000,000,000 QQQ @ 100,000,000,000 USD with a 10 bps maker rate
   and a 20 bps taker rate.
3. Taker buy 10,000,000,000 QQQ @ 100,000,000,000 USD with the same rates.

The fill's notional is 10^21, which does not fit the scale-18 `i128`
representation. The engine reports the fill normally, but:

- `quote_volume` is `0`;
- `taker_fee` and `maker_fee` are `0`;
- `taker_fee_asset` and `maker_fee_asset` are still `quote`;
- `fee_overflow` is `true`.

The quote policy makes both fees depend on the overflowing notional, so both
are zeroed together. A fill marked `fee_overflow` must not be treated as having
real zero fees; the flag says the fee fields are unreliable.

## Expected events

`expected-events.jsonl` holds two events: the maker rest and the overflowing
fill. Price and volume remain exact; only the quote-derived fields are zeroed.

## Run

```shell
./tests/replay/fee_overflow_orders/run.sh
```

The script uses `127.0.0.1:24225` by default; override it with
`REPLAY_ADDR=127.0.0.1:24325 ./tests/replay/fee_overflow_orders/run.sh`. The
node's gRPC port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/fee_overflow_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/fee_overflow_orders/orders.jsonl \
  --actual tests/replay/fee_overflow_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24225
```
