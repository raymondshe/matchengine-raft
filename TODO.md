# TODO

Last reviewed: 2026-11-04. This is the working roadmap; see `AGENTS.md` for commands and the
current branch state.

**Current baseline:** the OpenRaft 0.10 port, durable WAL/snapshot storage and exact scale-18
`Fixed` migration are done. Order prices/quantities, last trade, match events, depth/BBO, `/read`,
the market-data feed and wire v6 are exact; snapshot v10 stores fixed-point orders/events, the
single-symbol precision configuration, asset metadata, volume units, maker/taker fee assets and the
fill-level `error` code set.
One process/cluster trades exactly one symbol; multi-symbol is isolated by running one process per
symbol. Base/quote volume and asset-aware fee deduction are done, and the market-data processor
runs as a tokio task. Edge request/snapshot limits and the loopback/private-network contract are in
place. The remaining work is measured performance evidence and event delivery; TLS/authentication
stays at the gateway and is out of scope for this process.

Priority key: **P0** = correctness/security, do next; **P1** = production readiness; **P2** =
important but not blocking; **P3** = later/optional.

## Top priorities

No P0/P1 feature work remains: the measured baselines are in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md), the third-party follow-ups are P2/P3 (§14), and
measurement scope, peak RSS, the book-only replay benchmark, the fuzz targets, health/readiness and
the operational console are in place. Equity/ETF sessions, holidays, auctions, T+1/T+2 settlement,
corporate actions and fixed/regulatory/minimum/tax fees are owned by the external order system
(§13).

Failure behavior is complete (§15): the node fail-stops on WAL `ENOSPC`/`EIO`, recovers from
`SIGKILL` during writes, keeps snapshot writes atomic, and a learner killed mid-snapshot-install
restarts without a partial snapshot and retries cleanly. Pre-Vote is enabled with a dedicated gRPC
RPC (§4), so an isolated node asks before campaigning. The wire v6 reference decoder and golden
tests are complete (§6).

The next work, in order (re-prioritised 2026-11-04 after the external optimization-notes
re-review, whose verdicts are in [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md)):

1. **Benchmark measurement discipline (§8, P1).** Add p50/p99 to `book_replay` and `level_book`,
   label cache warmth per row, and run one pinned-core `e2e` pass. Best-of-5 whole-round times hide
   the tails that a regression gate needs; this is the prerequisite for item 2.
2. **Benchmark regression gate (§8, P2).** Gate CI on `apply_path` allocs/entry (deterministic) plus
   loose percentile thresholds from item 1.
3. **Event delivery Phase 4 (§5).** Long-poll `wait_ms`, gRPC `StreamEvents` and the optional MQ
   bridge; this also settles the event-time/cursor item in §11.
4. **Realistic-mix replay benchmark (§8, P2).** Replay a large fixture with a real
   add/cancel/amend mix (stops, icebergs, fees) and report percentiles - the load analogue of
   `tests/replay` and the closest equivalent to the ITCH-style replay the external notes treat as
   the only production-like signal.

The quick wins from the last review are complete: `Order` identity `PartialEq`, optional
`Order::sequence`, a BTC/USDT quote-volume case, and the `fee_overflow_base_orders` replay case.

## 1. Exact fixed-point end-to-end — done

Completed:

- [x] `Order.price`, `volume`, `stop_price`, `display_volume` and `hidden_volume` are scale-18
  `Fixed(i128)`; book keys, `MatchResult.matches`, amendment values and matching arithmetic use
  the same representation.
- [x] `OrderBook.last_trade_price` is `Option<Fixed>`; stop election and snapshots no longer round
  a print through `f64`.
- [x] `MatchEvent.price/volume` and `StoredMatchEvent` are `Fixed`; `/events`, `/write` events and
  the file sink emit decimal strings.
- [x] `DepthLevel`, `DepthChange`, `spread`, `mid_price`, `/read` depth/BBO/spread/last-trade and
  the shadow book are exact. The shadow book is keyed by `Fixed`.
- [x] Market-data `TradeRecord`, `BboRecord`, depth records, lifecycle and `BarRecord` are `Fixed`.
  Wire **v6** encodes prices, quantities, quote volume and fees as little-endian scale-18 `i128`.
- [x] Snapshot **v10** stores the last trade, match events, asset metadata, tick/lot rules, fee
  assets and fill fees as `Fixed`, plus the single-symbol precision configuration; older snapshots
  are rejected.
- [x] `Fixed::notional` multiplies through an `i256` intermediate and truncates toward zero at 18
  places; unit tests cover an intermediate that overflows `i128`.
- [x] Fill fees use the taker order's taker rate and the maker order's maker rate; `MatchResult`
  carries notional/fees and an ordered `error` set, events and the JSONL feed carry both fees, and
  snapshot v10 preserves the order rates and event fees.
- [x] `tests/cluster/test_exactness.rs` round-trips 18 fractional digits through `/write`,
  `/read`, `/events`, the JSONL feed, snapshot v10 and a restart.
- [x] README/README_CN, AGENTS, DECIMAL, MATCH_RESULTS, MARKET_DATA_PLAN and AERON record wire v6,
  snapshot v10 and the new decimal/fee JSON output.

Note: wire v6 and snapshot v10 are breaking changes; existing data directories must be recreated
and older feed consumers must implement the scale-18 integer layout, volume units and fee assets.

## 2. One process, one symbol [done]

Decision: one process/cluster supports exactly one symbol. Multi-symbol runs one process/cluster
per symbol, so CPU, memory, logs, snapshots and failure domains stay isolated. The engine never
routes two books or switches symbols after trading starts.

Done:

- [x] Replicated `StateMachineData.symbol_config` + `symbol_locked` in snapshot v10.
  `MatchEngineRequest::ConfigureSymbol` sets the one symbol and its precision through Raft before
  trading.
- [x] The configuration is locked after the first accepted order; changing symbol or precision
  while orders are open, or after trading starts, is refused.
- [x] `Place` and `Amend` validate price/quantity decimals, price tick and base lot for that
  symbol; invalid values produce a `rejected` event. `Cancel`, `Amend` and `Place` reject a
  different `symbol_id`.
- [x] Gateway symbol guard: the leader's `/write` and `/write_batch` reject a wrong-symbol
  `Place`/`Cancel`/`Amend` with `400` before the Raft proposal, so a misrouted order costs no log
  entry, sequence or `rejected` event. A batch is guarded up to its first `ConfigureSymbol`; the
  state machine still re-checks the symbol as defense in depth.
- [x] `/read symbol_config` and `/read symbol_id` expose the configuration; a publishing
  market-data feed is reset to the new symbol after a successful configuration change.
- [x] `tests/cluster/test_symbols.rs` covers the gateway guard (wrong-symbol requests and batches,
  sequence/book checks, and a configure-and-place batch), precision rejection, the symbol lock and
  snapshot restart.
- [x] Documented the deployment contract: one cluster/process group per symbol, separate data
  directories, ports, service-discovery names and market-data streams.

Remaining:

- [ ] **P2** Add a provisioning helper for `ConfigureSymbol` and a pre-trade readiness check.

## 3. Public trust boundary and request safety [P1]

- [x] **P0** Bound `client_order_id` to 1–128 printable ASCII bytes from `[A-Za-z0-9._:-]`;
  empty strings are rejected and invalid ids return `400` before the Raft proposal.
- [x] **P0** Cap application JSON bodies at 1 MiB, validate `/read` keys at 1–128 printable ASCII
  bytes, and reject `/events` limits outside 1–1000 with `400`. Malformed JSON and oversized
  bodies fail in the extractor.
- [x] **P0** Cap gRPC snapshot streams at 256 MiB, checking the cumulative total before each
  chunk is appended.
- [x] **P0** Adopt and document a loopback/trusted-private-network-only deployment contract because
  TLS/mTLS and HTTP authentication are not implemented. The node warns when bound to a
  non-loopback address.
- [x] **P1** Keep TLS/mTLS and API authentication out of this process by design: the matching
  engine is an internal service behind a gateway that owns client authentication and TLS
  termination. The loopback/private-network warning stays as a deployment guard.
- [x] **P1** Add `GET /health` (liveness: gRPC listener, Raft running state, WAL and state-machine
  frontier) and `GET /ready` (healthy plus a known leader). The response reports leadership and
  durable storage state; both return `503` when a required component is unavailable.
- [x] **P1** Implement the operational console in
  [`docs/OPERATIONAL_CONSOLE.md`](./docs/OPERATIONAL_CONSOLE.md): read-only node/Raft/storage/events
  views, then guarded leadership transfer/election actions. The gRPC `TransferLeader` RPC is
  implemented; `/console/` and `/console/api/*` are live.
- [x] **P1** Fail fast when the gRPC server task dies: `MatchEngineNode::run()` stops HTTP and
  background tasks, shuts Raft down and returns the transport error.
- [x] **P2** Remove the duplicate `Logger` middleware in `src/lib.rs`; every request is logged
  twice.
- [x] **P2** `Order::sequence` is optional in the public deserializer (`#[serde(default)]`); it
  defaults to `0`, and `apply` overwrites it.
- [x] **P2** `impl PartialEq for Order` compares `id`, `side`, `symbol_id`, `sequence`, `price` and
  `volume`, so two unrelated orders no longer compare equal.

## 4. Persistence, Raft and transport

Persistence is done:

- [x] Segmented WAL (`MRFT` header, CRC32-framed postcard records, one `write_all` + one
  `sync_data` per append batch, `pread` reads, torn-tail recovery).
- [x] Atomic snapshots (`MRFSNAP`, temp file → fsync → rename → directory fsync) with validation
  before replacing the durable copy.
- [x] Per-node directories from `StorageConfig`; `--data-dir` / `--instance-prefix` and env
  equivalents.
- [x] Restart recovery via log replay and snapshot + newer WAL tail; the snapshot load restores
  the event window and dedupe map before openraft replays tail entries. Large-log,
  large-snapshot and late-join coverage.
- [x] Default snapshot policy: `LogsSinceLast(100_000)` plus a 30-minute timer that only fires
  when newer entries exist; `StorageConfig::snapshot_interval` and `--snapshot-interval-ms` tune
  the timer (`0` disables it).
- [x] Fail-fast gRPC failure: `MatchEngineNode::run()` stops HTTP, joins background tasks and
  shuts Raft down before returning the transport error. A dropped/panicking gRPC task also
  signals failure.
- [x] Storage-path audit: borrowed WAL encoding (no per-entry clone), sequential segment
  recovery, coalesced contiguous log reads, snapshot encode/write outside the state-machine lock,
  and streamed snapshot bodies with incremental CRC (no full body Vec).
- [x] One request logger; the previous duplicate `Logger` middleware is gone.
- [x] `POST /write_batch` applies 1–128 non-nested requests in one Raft entry, with per-item
  sequences/events and continuous event ordinals; empty, oversized, nested and duplicate-id
  batches are rejected at the edge.
- [x] `test_cluster` owns all three nodes in its runtime and shuts them down on success or panic,
  so fixed ports are released.

Transport decisions:

- [x] **P2** Keep TLS at the gateway; do not add per-node TLS to the Raft transport. Keep the
  gRPC listener's failure behavior aligned with the health policy in §3.
- [x] **P2** Use an explicit gRPC `transfer_leader` transport instead of openraft's default:
  `src/network/grpc/client.rs` sends the RPC and `src/network/grpc/server.rs` calls
  `handle_transfer_leader`; the console transfer action uses it.
- [x] **P2** Pre-Vote is enabled: `default_raft_config` sets `enable_pre_vote: Some(true)`, the
  gRPC transport implements `RaftNetworkV2::pre_vote` with a dedicated `PreVote` RPC, and the
  server calls `Raft::pre_vote`. openraft's default grants unconditionally, so the RPC is what
  makes the flag effective.

Remaining:

- [ ] **P2** Move off `openraft` 0.10.0-alpha once a stable 0.10 release exists; latest published
  is still `0.10.0-alpha.37`.
- [ ] **P2** Consider streaming snapshot bytes into the state machine or a temp file instead of
  buffering the whole snapshot. The current API needs a contiguous buffer, but the receiver's
  total allocation still needs a policy (see §3).
- [ ] **P3** Aeron remains gated on a measured bottleneck; no adapter is adopted.

## 5. Matching-engine features and correctness

Done:

- [x] Replaceable `MatchResultSink` (`MatchEvent` with `(log_index, ordinal)` ids;
  `Noop`/`Memory`/`File` sinks; `--match-event-log` debugging sink). Sink failures never affect
  `apply`.
- [x] Durable event delivery: events in `/write` (capped at 512), bounded replicated event log,
  `POST /events`, `GET /events/status`, and `client_order_id` dedupe.
- [x] GTC, IOC, FOK, GTD, Day, post-only, iceberg, market, stop-market and stop-limit.
- [x] Deterministic GTD purge on every admissible mutation; stops elected by replicated
  `last_trade_price`; `triggered`, `expired` and `rejected` events.
- [x] `Amend` for plain resting limit orders; same-price reductions keep priority, price/size
  changes re-insert under a new sequence; crossing amendments are refused.
- [x] `spread`, `mid_price` and `depth` views plus `orderbook_depth`/`orderbook_spread` read keys.
- [x] Randomized differential/property tests against a small reference model: visible book,
  fills, price-time priority, no crossed book, volume conservation, sequence/last-trade state and
  deterministic replay.
- [x] Randomized mixed-order coverage for GTC/IOC/FOK/GTD/Day, market, post-only, iceberg and
  stop orders, with positive, zero and rebate maker/taker rates under Received/Quote/Base fee
  policies.
- [x] Explicit conformance cases for market FOK, empty-book market orders, hidden iceberg
  cancel/expiry, parked-stop GTD/Day expiry and fee reporting on market/IOC/stop-child fills.
- [x] Prototyped per-price-level buckets (`BTreeMap<price, Vec<Order>>` + `head` cursor +
  sequence index + cached BBO) in `benches/level_book.rs`; the benchmark verifies the prototype
  against `OrderBook` before timing. Results are in `docs/PERFORMANCE.md`: match 1.76–2.15x,
  deep cancel 3.34–3.37x, same-price amend 1.70–1.75x, dense re-queue 2.58–2.62x and BBO query
  4.5–6.2x faster; sparse one-order-per-level re-queue 0.64–0.65x and BBO advance 0.87–0.90x
  slower; depth neutral. Full adoption remains P2 and is not justified without a sparse-level
  fast path.

Remaining:

- [ ] **P2** Amend parked stops (trigger price/volume) and icebergs (tranche semantics); consider
  a sequence → location index for cancel/amend by sequence alone, with index maintenance or
  tombstones.
- [ ] **P2** Port the missing OrderBook-rs types: trailing stops, pegged orders, custom reserve
  replenishment, stop-protection collars, market-to-limit and self-trade prevention.
- [ ] **P2** Phase 4 of match-result delivery: long-poll/short-poll `/events` (`wait_ms`), gRPC
  `StreamEvents`, and an optional MQ bridge sidecar. Decide event time and per-instrument cursors
  for multi-symbol deployments.
- [ ] **P3** Consider protobuf/Avro on the public HTTP API. Raft RPCs already use postcard over
  gRPC.

## 6. Market data

Design: `docs/AERON_SOLUTION.md` Part II; implementation plan and progress:
`docs/MARKET_DATA_PLAN.md`.

Done:

- [x] Deterministic feed core: depth journal, `MatchEvent.side`, leader-stamped intake
  timestamps, shadow book, one TRADE per taker, BBO, depth snapshots/deltas, lifecycle, OHLC bars,
  status cadence, gap marker, leader gating with reset snapshots, JSONL sink and wire encoder v6.
- [x] Deterministic `market_seq`/`sub_seq`: identical keys across replicas, failover and restart.
- [x] `marketdata_status` read key.
- [x] The processor runs as a tokio task over a bounded tokio `mpsc` queue, with `watch`-driven
  shutdown and `Notify`-driven snapshot wakeups; `try_send` keeps the matching path non-blocking.
- [x] Processor state and shutdown are covered by unit tests and `test_market_data.rs`.
- [x] **P1** Load-tested the feed at 50k–500k fills/s: at 500k fills/s with an 8,192-frame queue the
  processor keeps up with no drops; a deliberate 2M fills/s overload drops frames and emits `GAP`.
  Results: [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).
- [x] **P2** `epoch` (leader generation) and `leader_id` carry the current Raft term and known
  leader: the leader watcher updates both on every metrics change; headers and reset snapshots carry
  them, covered by the market-data unit test and `test_market_data.rs`.
- [x] **P2** Wire v6 reference decoder (`wire::decode`) with a strict `WireError`: validates the
  header, body length, enum codes and length prefixes; golden round-trip tests cover every body
  type, a checked-in golden frame and malformed-frame rejection.

Remaining:

- [ ] **P2** Archive recording/replay and the snapshot-request stream (2003/2010).
- [ ] **P3** Aeron adapter: `MarketDataSink` with `rusteron-client` plus an embedded driver in
  tests, feature-gated (`market-data-aeron`); see AERON_SOLUTION §20/§23.

## 7. Tests and CI

Current coverage:

- Unit tests cover the order book, fixed-point parsing, Raft-address derivation and snapshot
  framing. `src/order_book/property_tests.rs` runs randomized traces against a reference model,
  including mixed order types and fee-rate styles; `conformance_tests.rs` covers the explicit
  edge cases. `apply` event tests cover market, IOC, FOK, post-only, iceberg, stop-child
  cancellation and automatic GTD expiry. `src/market_data/wire.rs` tests the encoder and
  reference decoder, pins a v6 golden frame and rejects malformed frames.
- `tests/cluster/test_cluster.rs`: 3 nodes over gRPC, init/membership/read/write.
- `test_snapshot.rs`: snapshot build/purge and learner install over gRPC.
- `test_storage.rs`: WAL framing, torn-tail truncation, segment rotation, 20k-record multi-segment
  reopen, purge/truncate, vote/committed persistence, snapshot build/install/reopen.
- `test_restart.rs`: restart via log replay and via snapshot + newer WAL tail (including event
  window and dedupe), 2k-order large-log replay and a multi-chunk (~120 KB) snapshot boot.
- `test_events.rs`: `/write` events, `/events` cursor, retention, submission dedupe.
- `test_match_results.rs`: file-sink rest/fill/cancel ordering, stop trigger, GTD expiry and read
  keys.
- `test_market_data.rs`: feed snapshot, trades, BBO, depth and bars.
- `test_membership.rs`: leader failover preserves the committed book and accepts writes; a learner
  catches up from a live leader.
- `test_late_join.rs`: a stale snapshot/WAL is superseded after a late rejoin.
- `test_exactness.rs`: 18-fractional-digit values survive `/write`, `/read`, `/events`, the JSONL
  feed, snapshot v10 and a restart.
- `test_symbols.rs`: one-symbol config, over-precision rejection, cross-symbol rejection, the
  post-trade config lock and snapshot restart.
- `test_fees.rs`: maker/taker fee rates, `/write`/`/events`/file-sink/feed fees and snapshot restart.
- `test_batch_write.rs`: one Raft entry for multiple requests, per-item sequences and event
  ordinals, and invalid-batch rejection.
- `test_fee_assets.rs`: ETF/fiat symbol metadata, tick/lot enforcement, quote-volume limit
  normalization and quote-asset fee deduction.
- `test_request_limits.rs`: oversized bodies, malformed JSON, invalid/oversized client ids,
  oversized read keys and out-of-range event pages are rejected before Raft.
- `test_feature_matrix.rs`: every advertised order type reaches the engine through `/write`;
  rest/amend/fill/cancel, expiry and stop lifecycles emit explicit events; malformed bodies and
  unknown read keys are covered.
- `test_health.rs`: `/health` reports the gRPC listener, leadership and WAL/state-machine state;
  `/ready` is `503` before initialization and `200` after a leader is elected.
- `test_console.rs`: three-node cluster checks the embedded console assets, node/cluster APIs,
  unsafe-action rejection and leadership transfer through `/console/api/actions/transfer-leader`.
- `tests/replay/`: shell-driven replay matrix for GTC, IOC, FOK, GTD, Day, market, post-only,
  iceberg, stop-market, stop-limit, amend, cancel, default-policy maker/taker fees, quote-asset fees
  and two fee-overflow scenarios (quote policy, and a `Received` fill that keeps an exact base fee).
  Each case runs through `replay_orders` and compares the generated event sequence with a checked-in
  reference; `tests/replay/run-all.sh` runs the whole matrix and `run-all.sh events` replays the same
  cases through `POST /events`.
- `fuzz/`: `cargo-fuzz` targets for `OrderBook::execute_order` (`order_book`), WAL byte decoding
  (`wal_decode`), snapshot byte/content decoding (`snapshot_decode`) and market-data wire decoding
  (`wire_decode`); the scheduled `.github/workflows/fuzz.yml` job builds them and runs a smoke pass.

Completed follow-ups:

- [x] **P1** Randomized differential/property tests for matching invariants (see §5).
- [x] Exactness across snapshot/restart/events/feed is covered by `test_exactness.rs`.
- [x] Per-symbol rejection, precision and lock tests are covered by `test_symbols.rs` (see §2).
- [x] **P0** Hostile-input tests for oversized `client_order_id`, oversized `/write` and `/events`
  payloads, malformed JSON, oversized read keys and oversized snapshots (see §3).
- [x] **P1** End-to-end latency benchmark: submit → commit → apply → response, separated from
  matching and replication costs. See [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).
- [x] **P2** Add negative API tests for unknown `/read` keys and malformed `Place` payloads.
  Covered by `test_feature_matrix.rs`; follower forwarding is already covered.
- [x] **P2** Make `test_cluster.rs` own and shut down its nodes, including on panic.
- [x] **P2** MSRV job for Rust 1.89: the `msrv` job in `.github/workflows/ci.yml` runs
  `cargo check --all-targets --locked`; the main job uses stable.

Remaining:

- [ ] **P2** Parameterize the fixed test ports; `test_cluster.rs` still binds 21001–21003.
- [ ] **P2** Keep `AGENTS.md` and this file in sync after code changes; both are maintained by
  hand.

## 8. Performance and capacity

- [x] Order-book micro-benchmarks: `cargo bench --bench matchengine` (wide/deep sweep, rest,
  rest with the market-data journal, partial fill, IOC cancel, iceberg refill, amend, cancel;
  best-of-5 per round).
- [x] **P1** Record the post-fee baseline in [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md)
  (linked from `docs/DECIMAL.md`): sweep 1k/100 levels 105.6µs, 20k/1 level 2.538ms, rest 97.7µs,
  rest+journal 100.3µs, partial 119.2µs, IOC 201.0µs, iceberg 116.8µs, amend 130.8µs, cancel
  148.1µs. The journal adds only ~2.6µs per 1k rests; no no-fee fast path is justified yet.
- [x] **P1** Add end-to-end latency and throughput benchmarks (`cargo bench --bench e2e`):
  sequential p50 3.99ms / p99 5.17ms, 18,350 writes/s with 1,000 in flight.
- [x] **P1** Load-test the market-data feed (`cargo bench --bench feed_load`): 495k fills/s with no
  drops at an 8,192 queue; 2M overload drops frames and emits `GAP`.
- [x] **P1** Add a storage benchmark (`cargo bench --bench storage`): 20k entries append at
  26,049/s, read at 2,386,089/s, and reopen/recover in 6.33ms.
- [x] **P1** Record measured peak RSS for the microbench, e2e, feed, storage and P0 workloads in
  `docs/PERFORMANCE.md`; the table states that RSS is whole-process, not per-order.
- [x] **P1** Add a book-only replay benchmark (`cargo bench --bench book_replay`): 1k/100-level
  sweep 94.7µs (10.56M fills/s), 20k/1-level sweep 2.209ms (9.06M fills/s), 5k deep cancels
  270.9µs (18.46M cancels/s), 1k-level BBO advance 98.3µs (20.35M advances/s), 1M BBO queries
  2.335ms (428M queries/s).
- [x] **P1** Add an apply-path allocation benchmark (`cargo bench --bench apply_path`) and reduce
  the common event path with inline `SmallVec` buffers: rest 7.17→4.17, cancel 6.00→4.00, fill
  8.00→5.00 allocs/entry; batch rest 6.23→4.19 allocs/entry. Requested bytes/entry fell from
  2,323→595 (rest) and 1,308→156 (cancel); fill 3,308→2,012 and batch rest 2,393→1,693. The
  remaining allocations are the `BTreeMap` node, response `events` `Vec`, legacy `value` `String`
  and fill `MatchResult` `Vec`.
- [ ] **P1** Report percentiles, not only best-of-5: add p50/p99 to `book_replay` and `level_book`
  rounds and label cache warmth per row. Only `e2e` reports percentiles today, and the external
  optimization-notes review (2026-11-04) showed best-of-5 hides exactly the tails a gate needs.
- [ ] **P1** Add one realistic-mix replay benchmark: replay a large fixture (add/cancel/amend with
  stops, icebergs and fees) and report p50/p99. `tests/replay` is correctness-only; this is its
  load analogue.
- [ ] **P2** Adopt per-level buckets or a compact resting record only if the P1 prototype and
  new 3-node or real-sink measurements justify the complexity. The prototype wins populated-level
  match/cancel/amend and cached-BBO queries but loses sparse one-order-per-level re-queue and
  BBO advance; full adoption is not justified yet, and a hybrid or sparse-level fast path is the
  next experiment.
- [ ] **P2** Add a benchmark regression gate (threshold or recorded baseline) to CI.
- [ ] **P2** Run `cargo bench --bench e2e` pinned to one core (`taskset -c <n>`) once, to separate
  scheduler noise from the fsync cost that dominates the current p99.
- [ ] **P2** Reserve the known lengths for the remaining apply-path allocations (response `events`
  `Vec`, fill `MatchResult` `Vec`) where the shape is fixed; they are the `apply_path` leftovers.

## 9. Client library

- [x] **P2** Harden `src/client.rs` before use outside examples: retry/backoff on transient RPC
  errors, configurable timeouts, and a typed "no known leader" error. Done: `ClientConfig` sets
  timeout/transient/forward retries and exponential backoff; `ClientError` distinguishes `Api`,
  `NoLeader`, `Timeout`, `Decode` and Raft transport errors.
- [x] **P2** Add integration coverage for leader forwarding, failover and retry behavior. Done:
  `tests/cluster/test_client.rs` covers typed no-leader/API errors, retry after a dropped
  connection and forwarding to the new leader after failover.
- [x] **P2** Keep the client transport plaintext; gateway authentication/TLS is outside the
  internal client.

## 10. Docs, repo hygiene and naming

Done:

- [x] GitHub Actions runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
  warnings` and `cargo test`.
- [x] `Cargo.lock` is tracked; clippy is clean with `-D warnings`.
- [x] `scripts/` holds every shell helper; scripts resolve the repo root themselves and write
  logs under `logs/`, not the repository root.
- [x] `src/main.rs` is the only binary entry; `examples/sled.rs` is a dev-only playground and
  `sled` is a dev-dependency. `examples/dump_wal.rs` and `examples/dump_snapshot.rs` are read-only
  JSONL dumpers; `examples/replay_orders.rs` is the order-replay/compare harness.
- [x] The unused `match_engine.png` was removed and `logs/` is gitignored.
- [x] README/README_CN, AGENTS, TODO, AERON, MATCH_RESULTS, MARKET_DATA_PLAN and FEE_DEDUCTION are
  synced with the WAL + snapshot + gRPC design and snapshot v10 / wire v6.
- [x] `docs/DECIMAL.md` records the fixed-point research, BTC 18-decimal range analysis,
  microbenchmarks and the completed end-to-end migration.
- [x] **P3** Renamed `src/matchengine/` to `src/order_book/` (2026-10-29); the public module is now
  `order_book`, while the package/binary name `matchengine-raft` is intentionally unchanged.

Remaining:

- [ ] **P2** Keep `AGENTS.md`, READMEs, `docs/DECIMAL.md` and this file in sync after future
  storage/wire changes.
- [ ] **P3** Decide whether to rename the remaining mixed-style modules before 1.0
  (`src/match_result/` → `match_events/`). This is import-breaking and offers no functional
  benefit today.

## 11. Decisions to record

These choices feed the priorities above; write them down as ADRs or doc notes when made.

- [x] `SymbolConfig` lives in replicated state as the single process/cluster symbol and locks
  after the first accepted order; precision changes are versioned by the Raft log/snapshot.
- [x] Wire v6 representation: scale-18 `i128` prices, quantities, quote volume and fees plus
  volume units and fee assets; older consumers must upgrade and frames are never silently
  reinterpreted.
- [x] Keep one canonical 18-decimal `Fixed(i128)` scale. `SymbolConfig` decimals are business
  precision, not a runtime scale; a per-symbol scale would enlarge values or add per-operation
  scaling, and a smaller `i64` scale trades range and a v10/v6 migration for an unmeasured gain.
  See [`docs/DECIMAL.md`](./docs/DECIMAL.md#decision-keep-one-compile-time-18-decimal-scale).
- [x] External exposure model: no TLS/mTLS or HTTP authentication; the supported contract is
  loopback or a trusted private network, with edge request/snapshot limits.
- [x] Authentication and TLS terminate at the gateway; the engine and its Raft transport remain
  internal-only and plaintext for latency.
- [x] gRPC listener failure policy: fail-fast. A dead gRPC server task stops HTTP/background
  work and returns the transport error; it does not leave a half-healthy node serving.
- [x] Multi-symbol isolation: one process/cluster per symbol, never multiple books in one process.
- [x] Equity/ETF sessions, holidays, auctions, T+1/T+2 settlement, corporate actions and
  fixed/regulatory/minimum/tax fees are owned by the external order system; the engine stays
  deterministic and consumes explicit timestamps and notional-proportional maker/taker rates.
- [x] Pre-Vote uses a dedicated gRPC `PreVote` RPC and is enabled by default; mixed-version
  clusters are unsupported because an older peer returns `Unimplemented`.
- [x] No arena allocator on the apply path. The external optimization notes measured `bumpalo`
  79-88% *worse* for single-operation latency and removed it again (their Optimization 7 -> 9);
  inline `SmallVec` buffers are the adopted approach. See
  [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).
- [ ] **P2** Decide the event time source for the clearing feed and per-instrument cursor retention
  (Phase 4 in §5).

## 12. Maker and taker fees — done

A process/cluster trades one symbol and never looks up users or balances. The gateway resolves a
user level to maker/taker rates and places those rates on the order; the engine computes fees
deterministically.

Done:

- [x] `Order.maker_fee_rate` and `Order.taker_fee_rate` are signed `Fixed` fractions with
  `serde(default)` zero. `"0.001"` means 10 bps; negative values are rebates. Rates are validated
  to `-1..=1`.
- [x] The gateway supplies both rates at placement; they travel in the Raft log, snapshot and
  resting/amended/stop/iceberg order state.
- [x] Per fill, notional = maker price × matched quantity through the `i256` path. A quote fee is
  notional × that side's rate; a base fee is matched quantity × that side's rate; the pair's fee
  policy decides which asset each side uses. Fees truncate toward zero at 18 places. Positive =
  charge, negative = rebate.
- [x] A taker that later rests uses its maker rate on future fills. Amendments, iceberg refills and
  stop children preserve the original order's rates.
- [x] `MatchResult` carries `notional`, `taker_fee`, `maker_fee` and `MatchErrors` (the `error`
  set): `notional_overflow`, `taker_fee_overflow` and `maker_fee_overflow`, empty when every
  dependent value is exact. A flagged fill can still carry exact values — a base fee survives a
  quote-notional overflow — so a zero is real only when its code is absent.
  `MatchEvent`/`StoredMatchEvent` carry both fees and the error set.
- [x] `/write` events, `/events`, the JSONL file sink and market-data `TRADE`/fill `LIFECYCLE`
  records expose the fees and their assets. Wire is **v6**.
- [x] Snapshot **v10** persists order rates, fee assets and event fees.
- [x] Tests cover simultaneous maker/taker rates, negative maker rebates, rate validation, overflow
  error codes, 18-digit exactness, a taker that later rests, file/event/feed output and snapshot
  restart. The replay matrix adds two overflow cases: `fee_overflow_orders` (quote policy: the
  notional and both quote fees are unavailable) and `fee_overflow_base_orders` (`Received`: the base
  taker fee stays exact while the quote maker fee is unavailable).

Ownership boundaries (outside the matching engine):

- [x] **P2** Fee accounts/balances and settlement ownership live in the external order/clearing
  system. The engine reports exact fees, their assets and the policy; it does not move funds or
  apply final asset rounding.
- [x] **P2** Fee-tier changes on a live order are handled by the order system as cancel/replace;
  the engine preserves the order's original rates through amendments and resting fills.

## 13. Base/quote volume and asset-aware fee deduction [done]

Design: [`docs/FEE_DEDUCTION.md`](./docs/FEE_DEDUCTION.md).

Done:

- [x] `SymbolConfig` carries base/quote `AssetId`/`AssetClass`, per-asset settlement decimals,
  `price_tick`, `base_lot` and `FeeAssetPolicy`.
- [x] `Order.volume_unit` (`Base`/`Quote`) is replicated; quote limit orders normalize to base at
  the limit price and zero-lot conversions are rejected.
- [x] Quote-aware market sweeps: buy spends a gross quote budget; sell targets gross quote proceeds
  and rounds the final base lot up, overshooting by at most one lot.
- [x] Fee asset policies: received asset (crypto/spot and fiat FX default), quote asset
  (forced for ETF/equity bases) and base asset. Events report the actual fee asset per side.
- [x] Gross base/quote volume, taker/maker fee assets and fees flow through `MatchResult`,
  `MatchEvent`, `/events`, the file sink and the market-data feed.
- [x] An amendment carries no unit: its volume is always base, even for an order placed in quote
  volume, and the client's requested amount stays in `requested_volume`/`requested_volume_unit`.
  Tick/lot increments and per-asset precision are enforced on `Place` and `Amend`.
- [x] Snapshot **v10** and market-data wire **v6** are implemented; old data directories must be
  recreated.
- [x] Tests cover QQQ/USD (ETF/fiat) and BTC/USDT (crypto/stablecoin) tick/lot, quote volume and
  fee-asset deduction in `tests/cluster/test_fee_assets.rs`, plus quote fees, fee overflow, fee
  policies, the file sink, `/events`, the feed and snapshot restart.

Remaining extensions:

- [ ] **P2** Add third-asset fee-token support only if a fee price and balance model are provided.
- [ ] **P2** Decide whether mixed-unit iceberg/FOK orders should normalize tranches; quote volume
  currently supports plain limit and market orders only, and those combinations are rejected at
  admission (`docs/FEE_DEDUCTION.md` §5.1, §13 item 16).
- [ ] **P2** Decide whether the numeric fields named by an error code should become `Option<Fixed>`
  (JSON `null`) instead of zero, so a real zero and an unavailable value cannot be confused
  (`docs/FEE_DEDUCTION.md` §13 item 15).

Resolved decisions: quote limit sell rounds base quantity down to cap notional; quote market sell
rounds up and permits a bounded overshoot; `volume` is always gross; the fee policy is per symbol;
quote market-buy budgets are gross notional with quote fees charged additionally; the engine
reports exact 18-scale fees and clearing applies final asset rounding. Sessions, holidays,
auctions, T+1/T+2 settlement, corporate actions and fixed per-trade/per-share, regulatory, minimum
ticket and tax fees are owned by the external order system; the engine consumes explicit timestamps
and notional-proportional maker/taker rates only.

## 14. Third-party investigation takeaways

Source: [`docs/THIRD_PARTY_INVESTIGATION.md`](./docs/THIRD_PARTY_INVESTIGATION.md), covering
[AsthaMishra/matching-engine](https://github.com/AsthaMishra/matching-engine),
[wasif-exe/wire](https://github.com/wasif-exe/wire) and
[llc-993/matching-core](https://github.com/llc-993/matching-core). All are single-node/reference
projects; our Raft, durability, exactness and deterministic event ids remain non-negotiable.

P0/P1 follow-ups (all completed):

- [x] **P0** Add a measurement-scope table to `docs/PERFORMANCE.md` (what each benchmark
  includes/excludes, closed-loop vs open-loop, timer source and variance), and record measured
  peak RSS rather than estimated per-order memory. Done: see the scope and memory sections.
- [x] **P0** Add `benches/book_replay.rs`: book-only replay with deep sweeps, cancel-at-depth and
  BBO-advance cases, separate from the existing HTTP/Raft `replay_orders` test. Done: results and
  scope are in `docs/PERFORMANCE.md`.
- [x] **P0** Add `cargo-fuzz` targets for `OrderBook::execute_order` and snapshot/WAL decoding;
  keep the targets buildable and runnable in a scheduled job. Done: `fuzz/` contains the
  `order_book`, `wal_decode` and `snapshot_decode` targets; `.github/workflows/fuzz.yml` builds
  them nightly and runs a smoke pass. The order-book target covers the quote-limit
  zero/overflowing-price and FOK preflight overflow cases, which are now fixed and
  regression-tested.
- [x] **P1** Prototype a per-price-level `Vec` + `head_idx` + order index + cached BBO; benchmark
  cancel, match, amend and depth before merging. Done: `benches/level_book.rs` and the results in
  `docs/PERFORMANCE.md`; full adoption remains P2 because the sparse one-order-per-level cases
  still lose. Keep the sparse `BTreeMap` for our 18-decimal range; do not copy a fixed 65k flat
  array.
- [x] **P1** Measure and reduce apply-path allocations. Done: `cargo bench --bench apply_path`
  reports per-entry heap churn; inline `SmallVec` event/publish buffers cut rest/cancel/fill from
  7.17/6.00/8.00 to 4.17/4.00/5.00 allocs/entry and batch rest from 6.23 to 4.19. Response
  `Vec`/`String` sharing and tap-frame copying remain follow-ups because they change a public type
  or need a tap-enabled workload.
- [x] **P1** Add a public-API feature matrix test proving every advertised order type reaches the
  matching engine. Done: `tests/cluster/test_feature_matrix.rs` sends every type through `/write`
  and asserts the event kinds.
- [x] **P1** Add an event-lifecycle test proving cancel, expiry, amend and stop handling emit
  explicit events rather than overloading `Reject`; consumers must reconstruct the full order state.
  Done: `test_feature_matrix.rs` covers rest → amend → partial fill → cancel, GTD/Day expiry and
  stop/stop-limit triggers, including the legitimate `stop_would_trigger` rejection.
- [x] **P1** Keep TPS, events/s and queries/s distinct in benchmark reports; llc-993 labels event
  count as QPS and estimates memory at 200 bytes/order. Done: the measurement-scope table and
  per-benchmark units keep writes/s, orders/s, fills/s, cancels/s, advances/s, queries/s and
  allocs/entry distinct, and memory is whole-process RSS rather than a per-order estimate.
- [x] **P1** Re-read AsthaMishra's `matching-core/optimization_notes.md` (2026-11-04) and record
  what transfers. Done: only the measurement discipline plus the already-prototyped per-level
  buckets apply; the arena was a measured regression there, and the response-slot pool,
  cached-BBO index and symbol sharding are already covered or inapplicable. Verdicts are in the
  "External optimization notes" section of [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).

Open follow-ups:

- [ ] **P1** Add percentiles and cache-warmth labels to the book benchmarks, add a realistic-mix
  replay, and run one pinned-core `e2e` pass (tracked in §8, from the same re-review).
- [ ] **P2** Evaluate a binary order-entry path (gRPC/protobuf or compact codec) and feature-gated
  latency histograms only if a latency target requires them.
- [ ] **P2** Evaluate real ITCH-style replay data and custom MPMC/SIMD/io_uring work only after a
  measured bottleneck and a Linux-only deployment decision. `wire-mpmc` is not async-aware: use
  Tokio `mpsc` for task-to-task communication and keep spin-based queues off Tokio workers.

Guardrails / what not to copy:

- Keep Raft consensus, WAL/snapshot durability, deterministic `(log_index, ordinal)` event ids,
  exact 18-decimal values, fees/assets and the existing property/conformance/integration suites.
- Do not copy one-fill matching loops, fixed 65k price arrays, O(n) cancel, global process-local
  trade ids, SIMD "cancel" that only computes a mask, 192 MiB-per-thread slabs, hard-coded `rdtsc`
  frequency conversions or io_uring as a default.
- Do not put an arena allocator on the apply path (measured 79-88% worse single-operation latency in
  the AsthaMishra notes, later removed by them), and never compare a single-node book
  microbenchmark with this engine's replicated, fsync-bound end-to-end numbers.
- Verify third-party claims before adopting: AsthaMishra and llc-993 have no license file;
  AsthaMishra's fuzz target is stale; Wire's v9 matching kernel is single-fill and its LOB has
  partial-fill/BBO bookkeeping issues; llc-993's advanced order types are not wired into its
  exchange and its WAL/snapshot is not crash-safe.
- Do not advertise a feature that exists only in an example, a separate book type, or an
  unreachable "optimized" implementation.

## 15. Failure behavior

- [x] **P1** Fail-stop when the Raft core stops: `MatchEngineNode::run()` watches
  `running_state` and returns an error instead of leaving HTTP and `/health` serving;
  `tests/cluster/test_failure.rs` covers it.
- [x] **P1** WAL append failure: a debug-only one-shot `LogStore::inject_append_error` drives
  `ENOSPC` and `EIO` through a real client write; `test_failure.rs` asserts the node exits.
- [x] **P1** SIGKILL during a write stream: `test_failure.rs` kills the binary with writes in
  flight and asserts the restarted node's sequence covers every acknowledged batch.
- [x] **P1** Snapshot atomicity: unit tests prove a leftover `current.snap.tmp` (a crash between
  the temp write and the rename) is ignored and never replaces the committed snapshot.
- [x] **P1** SIGKILL during a learner snapshot install: `test_failure.rs` pauses the install after
  the first chunk with a debug-only delay, kills the learner, asserts no `current.snap` was written,
  restarts it, and proves the full snapshot is retried and installed.
