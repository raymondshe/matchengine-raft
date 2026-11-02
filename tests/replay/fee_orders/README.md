# Fee deduction replay

This case drives `examples/replay_orders.rs` with maker/taker fee rates and
compares the generated match events with `expected-events.jsonl`.

## Scenario

`orders.jsonl` contains nine requests under the default BTC/USDT symbol and
`Received` fee-asset policy: buyer fees settle in base, seller fees in quote.

1. Maker sell 2 @ 100 with a 10 bps maker rate and a 20 bps taker rate.
2. Maker sell 2 @ 101 with a -5 bps maker rebate and a 30 bps taker rate.
3. Taker buy 4 @ 101 with a 40 bps taker rate. It fills 2 @ 100 and 2 @ 101:
   - fill 1: notional 200, taker fee `0.008` base, maker fee `0.2` quote;
   - fill 2: notional 202, taker fee `0.008` base, maker fee `-0.101` quote.
4. Maker buy 2 @ 99 with a 20 bps maker rate and a 50 bps taker rate.
5. Taker sell 1 @ 99 with a 60 bps taker rate:
   - fill: notional 99, taker fee `0.594` quote, maker fee `0.002` base.
6. A zero-rate maker sell 1 @ 102 and a zero-rate taker buy 1 @ 102:
   - fill: notional 102, both fees zero.
7. Maker sell 1 @ 103 with a 10 bps maker rate; taker buy 1 @ 103 with a
   -10 bps taker rate:
   - fill: notional 103, taker fee `-0.001` base (rebate), maker fee `0.103`
     quote.

The maker fee uses the resting order's `maker_fee_rate`; the taker fee uses the
incoming order's `taker_fee_rate`. An order's other rate is not used for that
fill. Fees are exact at 18 decimals, and negative rates are rebates.

## Expected events

`expected-events.jsonl` holds ten events: five rests and five fills. The fills
cover positive maker and taker fees, a maker rebate, a taker rebate, both fee
assets under the `Received` policy, and a zero-rate fill.

## Run

```shell
./tests/replay/fee_orders/run.sh
```

The script uses `127.0.0.1:24223` by default; override it with
`REPLAY_ADDR=127.0.0.1:24323 ./tests/replay/fee_orders/run.sh`. The node's gRPC
port is the HTTP port + 1000.

To regenerate the reference after an intentional engine change:

```shell
rm -f tests/replay/fee_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/fee_orders/orders.jsonl \
  --actual tests/replay/fee_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24223
```
