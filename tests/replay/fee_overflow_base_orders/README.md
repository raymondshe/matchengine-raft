# Fee overflow with an exact base fee replay

This case drives `examples/replay_orders.rs` with a fill whose quote notional
cannot be represented at the scale-18 fixed-point range, while one of its fees
still can. It pins the rule that an overflow error names only the unavailable
field.

## Scenario

`orders.jsonl` contains two requests on the default BTC/USDT symbol (the
`Received` fee policy):

1. Maker sell 10,000,000,000 BTC @ 100,000,000,000 USDT with a 10 bps maker
   rate and a 20 bps taker rate.
2. Taker buy 10,000,000,000 BTC @ 100,000,000,000 USDT with the same rates.

The fill's notional is 10^21, which does not fit the scale-18 `i128`
representation.

## Expected events

`expected-events.jsonl` holds two events: the maker rest and the fill.

- `price`, `volume` and `volume_unit` stay exact.
- `quote_volume` is `0` and `error` contains `notional_overflow`.
- The maker sells and receives quote, so its 10 bps fee depends on the
  overflowing notional: `maker_fee` is `0`, `maker_fee_asset` stays `quote`,
  and `error` adds `maker_fee_overflow`.
- The buyer taker receives base, so its 20 bps fee depends only on the matched
  volume: `taker_fee` is the exact `20000000` in `taker_fee_asset` `base`.
- `error` is `["notional_overflow","maker_fee_overflow"]`; it does **not**
  contain `taker_fee_overflow`.

A zero fee is only a real zero when its code is absent. Here `taker_fee` is
exact while `maker_fee` is unavailable, even though both are zero or nonzero in
the same fill.

## Run

```bash
./tests/replay/fee_overflow_base_orders/run.sh
```

Use a different port with
`REPLAY_ADDR=127.0.0.1:24326 ./tests/replay/fee_overflow_base_orders/run.sh`.
The run also works against a live node on that address. Regenerate the expected
file after deliberate behaviour changes:

```bash
rm -f tests/replay/fee_overflow_base_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/fee_overflow_base_orders/orders.jsonl \
  --actual tests/replay/fee_overflow_base_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24226
```

then review the diff before committing.
