# Replay scenarios

Each directory here is a deterministic replay case for
[`examples/replay_orders.rs`](../../examples/replay_orders.rs). The harness
starts an in-process single-node Raft cluster, submits the requests in
`orders.jsonl`, writes the match-event JSONL to a file and compares it with
`expected-events.jsonl`. Every case directory also has a `run.sh` and a
scenario `README.md`.

The comparison ignores `node_id` and `event_id.log_index` (unless
`--strict-log-index` is passed), but checks every event kind, order, sequence,
side, price, volume, fee field and reason. The event ordinals inside an entry
must match too.

[`examples/replay_orders_events.rs`](../../examples/replay_orders_events.rs) replays the same
`orders.jsonl` but reads events from the replicated `POST /events` log instead of the node-local
file sink. Use it to validate what feed consumers actually page through:

```shell
cargo run --release --example replay_orders_events -- \
  --orders tests/replay/ioc_orders/orders.jsonl \
  --expected tests/replay/ioc_orders/expected-events.jsonl \
  --batch-size 1 --addr 127.0.0.1:24202
```

## Cases

| Directory | Default port | Coverage |
|---|---|---|
| `limit_orders/` | `24211` | GTC price-time priority, a multi-fill taker and a partial rest; replayed in batches of two |
| `ioc_orders/` | `24212` | IOC partial fill, no liquidity, a non-crossing limit and a remainder cancellation |
| `fok_orders/` | `24213` | Unfillable FOK rejection, multi-level FOK fill and an exactly fillable FOK |
| `gtd_orders/` | `24214` | GTD rest, `gtd_already_expired` rejection, a before-deadline purge and an at-deadline expiry |
| `day_orders/` | `24215` | Day orders surviving a purge without a close and expiring when a close is supplied; GTC control |
| `market_orders/` | `24216` | Multi-level market sweeps, an empty-book cancellation, residual liquidity and a partial remainder |
| `post_only_orders/` | `24217` | Resting post-only orders, crossing rejections and maker fills against post-only liquidity |
| `iceberg_orders/` | `24218` | Visible tranche, hidden refill, price priority, and cancel/expiry reporting the full open volume |
| `stop_market_orders/` | `24219` | Sell and buy stop election, child fills/cancellations and `stop_would_trigger` rejection |
| `stop_limit_orders/` | `24220` | Stop-limit children bounded by their limit, partial fill/cancellation and crossed-stop rejection |
| `amend_orders/` | `24221` | Priority-preserving reductions, re-queuing price/size changes and amend rejection reasons |
| `cancel_orders/` | `24222` | No-op cancel, parked-stop cancel and partial-fill cancel |
| `fee_orders/` | `24223` | Positive maker/taker fees, maker and taker rebates, both fee assets under `Received`, and a zero-rate fill |
| `fee_quote_orders/` | `24224` | ETF/fiat quote-fee policy, quote-volume limit normalization and both-side quote fees |
| `fee_overflow_orders/` | `24225` | Notional overflow zeroes quote volume and quote fees, and sets the overflow error codes |
| `fee_overflow_base_orders/` | `24226` | Notional overflow under `Received`: the quote maker fee is unavailable but the base taker fee stays exact |

Most cases use `--batch-size 1` so each request is its own Raft entry. The
`limit_orders` case keeps `--batch-size 2` to exercise batching as well.

## Run

Run every case sequentially with the file-sink harness:

```shell
./tests/replay/run-all.sh
```

Run the same matrix through `POST /events` (the API-source companion):

```shell
EVENTS_LIMIT=100 ./tests/replay/run-all.sh events
```

`EVENTS_REPLAY_ADDR` overrides the shared sequential address (default
`127.0.0.1:24202`); `EVENTS_LIMIT` sets the `/events` page size. Each case keeps
its own `--batch-size`, since batching changes event ordinals within a log
entry.

Run one case:

```shell
./tests/replay/ioc_orders/run.sh
```

Each script uses its own default HTTP port from the table; the gRPC port is the
HTTP port + 1000. Override the HTTP address with `REPLAY_ADDR`, for example:

```shell
REPLAY_ADDR=127.0.0.1:24312 ./tests/replay/ioc_orders/run.sh
```

The replay cases are manual checks and are not part of `cargo test`.

## Regenerate a reference

`FileLogSink` appends, so remove the old file first. Use the same port and batch
size as the case's `run.sh`:

```shell
rm -f tests/replay/ioc_orders/expected-events.jsonl
cargo run --release --example replay_orders -- \
  --orders tests/replay/ioc_orders/orders.jsonl \
  --actual tests/replay/ioc_orders/expected-events.jsonl \
  --batch-size 1 \
  --addr 127.0.0.1:24212
```

The individual `README.md` files describe each scenario and its event
expectations.
