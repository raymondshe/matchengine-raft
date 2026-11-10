# Testing

## Test suite

`cargo test` builds one integration target (`tests/cluster`) plus unit tests in the library.

| Test | What it proves |
|---|---|
| `src/order_book/property_tests.rs` | Randomized differential tests against a reference model plus mixed GTC/IOC/FOK/GTD/Day/market/post-only/iceberg/stop orders and positive, zero and rebate fee rates under all fee-asset policies: visible book, fills, priority, no crossed book, volume conservation, sequence/last-trade state and deterministic replay |
| `src/order_book/conformance_tests.rs` | Explicit market-FOK, no-liquidity market, hidden-iceberg cancel/expiry, parked-stop GTD/Day expiry and fee-reporting cases for market, IOC and stop-child fills |
| `tests/cluster/test_cluster.rs` | 3-node gRPC cluster: init, learner, membership, write/read, linearizable read, leader forwarding, membership shrink; owns and shuts down every node |
| `tests/cluster/test_snapshot.rs` | Snapshot creation, log purge, a learner installing the purged state over the gRPC snapshot stream, and the periodic snapshot timer with the log-count policy disabled |
| `tests/cluster/test_storage.rs` | WAL framing, torn-tail truncation, segment rotation, a 20k-record multi-segment reopen, truncate/purge across segments, vote/committed persistence, snapshot build/install/reopen, corrupt-file rejection |
| `tests/cluster/test_restart.rs` | Full node restart recovering the order book, sequence, event window and idempotency map via WAL replay and via snapshot + newer WAL tail; also 2k-order large-log replay and a multi-chunk (~120 KB) snapshot boot |
| `tests/cluster/test_match_results.rs` | A node started with `--match-event-log` records rest/fill/cancel events in log order while the `/write` response stays unchanged; a second node checks stop triggers, GTD expiry and the new `/read` keys |
| `tests/cluster/test_market_data.rs` | A node started with `--market-data-log` publishes a reset snapshot, one trade message per taker, BBO, post-entry depth totals and a closed OHLC bar; the assertions check `market_seq` anchors and `/read marketdata_status` counters |
| `demos/market_data_system` | Stream parsing and payloads, WebSocket snapshots/conflation/combined streams/control errors, multiple symbols on one connection with live `UNSUBSCRIBE`/`SUBSCRIBE`, and an embedded-Aeron engine→service→WebSocket test (feature `aeron`) |
| `tests/cluster/test_events.rs` | `/write` returns events, `/events` pages by cursor, `client_order_id` retries do not duplicate an order, `--max-events` eviction is visible in `/events/status`, long-poll wakes on a racing write or returns an empty page at the deadline, gRPC `StreamEvents` streams a fill and an idle heartbeat, and the bridge sidecar publishes events and checkpoints its cursor |
| `tests/cluster/test_membership.rs` | Kill the elected leader: a surviving voter takes over with the committed book and accepts new writes. A learner added after the writes catches up through normal log replication, is promoted, and writes through its own node |
| `tests/cluster/test_late_join.rs` | A node with a stale snapshot/WAL is removed, misses 40 writes while the leader snapshots and purges, then rejoins as a learner and catches up from the live cluster |
| `tests/cluster/test_exactness.rs` | An 18-fractional-digit price and quantity round-trip through `/write`, `/read`, `/events`, the JSONL market-data feed, snapshot v11 and a restart without passing through `f64` |
| `tests/cluster/test_symbols.rs` | The HTTP edge rejects wrong-symbol `Place`/`Cancel`/`Amend` and whole batches with `400` before Raft, including sequence/book checks; a batch that configures a new symbol and places in one entry still works. Also covers precision rejection, the symbol lock and snapshot restart |
| `tests/cluster/test_fees.rs` | Maker and taker fee rates produce exact fill fees in `/write`, `/events`, the file sink and the market-data feed, and survive snapshot restart |
| `tests/cluster/test_fee_assets.rs` | QQQ/USD ETF/fiat and BTC/USDT crypto/stablecoin metadata, tick/lot enforcement, quote-volume limit normalization, and quote-asset plus received-asset fee deduction |
| `tests/cluster/test_request_limits.rs` | Oversized JSON bodies, malformed input, invalid/oversized `client_order_id`s, oversized read keys and out-of-range event pages are rejected with `400`/`413` before Raft changes the book |
| `tests/cluster/test_health.rs` | `/health` reports the gRPC listener, leadership and WAL/state-machine state; `/ready` is `503` before initialization and `200` after a leader is elected |
| `tests/cluster/test_console.rs` | Three-node cluster: embedded console assets, node/cluster APIs, unsafe-action rejection and leadership transfer through `/console/api/actions/transfer-leader` |
| `tests/cluster/test_batch_write.rs` | Multiple requests are applied in one Raft entry with per-item sequences, continuous event ordinals, and edge validation for empty, nested, oversized and duplicate-id batches |
| `tests/cluster/test_feature_matrix.rs` | Sends one order of every advertised type through `/write` and asserts the returned event kinds; follows rest → amend → partial fill → cancel and expiry/stop lifecycles, proving accepted actions emit explicit events rather than rejections; checks malformed bodies and unknown read keys |
| `tests/cluster/test_client.rs` | Typed no-leader and HTTP edge errors, retry/backoff after a dropped connection, and forwarding to the new leader after failover |
| `tests/cluster/test_failure.rs` | Fail-stop when the Raft core stops, injected WAL `ENOSPC`/`EIO`, SIGKILL during a write stream and SIGKILL during a learner snapshot install; the SIGKILL cases spawn the real binary |
| `tests/replay/*/run.sh` | Shell-driven replay matrix for GTC, IOC, FOK, GTD, Day, market, post-only, iceberg, stop-market, stop-limit, amend, cancel, maker/taker fee, quote-fee and fee-overflow scenarios; each case compares the generated event sequence against its checked-in `expected-events.jsonl` |
| `fuzz/` | `cargo-fuzz` targets for `OrderBook::execute_order`, WAL byte decoding, snapshot byte/content decoding and market-data wire decoding; the nightly scheduled workflow builds all four and runs a smoke pass |

The replay cases are not part of `cargo test`; run the whole matrix with
`./tests/replay/run-all.sh`, or one case with `./tests/replay/<case>/run.sh`. They use HTTP ports
`24211-24226` (gRPC `25211-25226`), one per case, and compare the generated match-event JSONL with
the checked-in reference. See `tests/replay/README.md` for the case matrix and regeneration steps.

The fuzz targets are a separate nightly-only crate; see `fuzz/README.md` and
`.github/workflows/fuzz.yml`.

Every integration test uses a unique temporary data directory. The cluster test uses fixed ports
`21001-21003` (gRPC `22001-22003`), the snapshot tests `21101-21103`, the restart tests
`21201-21205`, the match-result tests `21301`/`21302`, the market-data test `21303`, the event-log test
`21304`, the failover test `21401-21403`, the learner catch-up test `21501-21502`, the late-join
test `21601-21603`, the exactness test `21701` (gRPC `22701`), the symbol-config test `21801-21802` (gRPC `22801-22802`), the fee test `21901` (gRPC `22901`), the
fee-asset tests `21902` and `21924` (gRPC `22902`/`22924`), the request-limit test `21903` (gRPC
`22903`), the batch-write test `21904` (gRPC `22904`), the health test `21905` (gRPC `22905`), the
console test `21906-21908` (gRPC `22906-22908`), the feature-matrix/lifecycle tests `21909-21911`
(gRPC `22909-22911`), the client tests `21912-21917` (gRPC `22912-22917`) and the failure tests
`21918-21923` (gRPC `22918-22923`); stop leftover processes (`pkill -f matchengine-raft`) before
rerunning.

## Replaying orders

`replay_orders` starts an in-process node with `--match-event-log`, replays an orders JSONL file in
`/write_batch` batches, then compares the generated match events with a reference file:

```shell
cargo run --release --example replay_orders --   --orders orders.jsonl   --expected expected-events.jsonl
```

Each orders line is either a `MatchEngineRequest` or a bare `Order` (wrapped as `Place`); blank
lines and `#` comments are ignored. `Batch` lines are submitted as one entry, and individual
requests are grouped by `--batch-size` (default 128). The comparison ignores `node_id` and
`event_id.log_index` by default; pass `--strict-log-index` for an exact event id. Use `--actual` to
keep the generated JSONL, `--keep` to keep the temporary node data, and `--help` for the rest.

`replay_orders_events` is the API-source companion: it leaves the node-local event sink off,
replays the same input, then pages the replicated `POST /events` log and compares that. It verifies
what feed consumers actually read, including retention and event ids:

```shell
cargo run --release --example replay_orders_events -- \
  --orders orders.jsonl \
  --expected expected-events.jsonl \
  --events-limit 100
```

Run the whole replay matrix through the API source with `./tests/replay/run-all.sh events`.

Checked-in cases live under `tests/replay/`: one directory per order type or lifecycle
operation, each with `orders.jsonl`, `expected-events.jsonl`, `run.sh` and a scenario README. The
matrix covers GTC, IOC, FOK, GTD, Day, market, post-only, iceberg, stop-market, stop-limit, amend,
cancel, maker/taker fee deduction, quote-asset fees and fee overflow. Run everything with
`./tests/replay/run-all.sh`, or one case with `./tests/replay/<case>/run.sh`.
