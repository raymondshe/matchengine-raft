# TODO

Last reviewed: 2026-11-10. This is the working roadmap; see `AGENTS.md` for commands and the
current branch state.

**Current baseline:** the OpenRaft 0.10 port, durable WAL/snapshot storage and exact scale-18
`Fixed` migration are done. Order prices/quantities, last trade, match events, depth/BBO, `/read`,
the market-data feed and wire v7 are exact; snapshot v11 stores fixed-point orders/events, the
single-symbol precision configuration, asset metadata, volume units, maker/taker fee assets, the
fill-level `error` code set, owner identity, market-to-limit/trailing/peg state and the STP policy.
One process/cluster trades exactly one symbol; multi-symbol is isolated by running one process per
symbol. Base/quote volume and asset-aware fee deduction are done, and the market-data processor
runs as a tokio task. WAL durability is configurable (`RAFT_WAL_FSYNC_MS`: unset = no fsync with a
four-voter floor, `0` = every append, `N > 0` = periodic), append writes are byte-bounded
(`--max-wal-append-bytes`), OpenRaft can run on a dedicated runtime (`RAFT_CORE_WORKERS`), and the
thread/lock audit leaves only that env-controlled `RaftThread` as an explicit OS thread. Snapshot
bodies are shared as `Arc<[u8]>` and a received snapshot stages to a validated file. Edge
request/snapshot limits and the loopback/private-network contract are in place. The 4-node no-fsync
cluster benchmark and the 2026-11-08 OpenRaft minimal-benchmark comparison are in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md); Raft is below the application path as the current
bottleneck. The remaining work is benchmark-derived correctness and measurement, performance
evidence, OpenRaft utilization follow-ups (§16) and feature extensions (§5, §6, §13);
TLS/authentication stays at the gateway and is out of scope for this process.

Priority key: **P0** = known correctness/security defect, do next; **P1** = correctness coverage,
measurement and production readiness; **P2** = important but not blocking; **P3** = later/optional.

## Top priorities

No P0 or P1 work remains. The benchmark correctness/occupancy envelope and the schema-v11
order-type batch landed on 2026-11-10; the remaining P2 production, feature and validation work is
next, then P3 experiments.

**Recently completed (2026-11-06/10):** configurable WAL fsync with a measured A/B matrix,
best-effort WAL recovery, the four-voter no-fsync floor (`test_min_voters.rs`), the optional
dedicated OpenRaft runtime (`RAFT_CORE_WORKERS`), the thread/lock audit (blocking sinks and startup
recovery on `spawn_blocking`, async JSONL sink/events bridge, Aeron publisher/source blocking
tasks, a Tokio WAL fsync worker; only the env-controlled `RaftThread` remains), the realistic-mix
replay benchmark, the four-node no-fsync cluster benchmark (`cluster_e2e`), the write
concurrency/batching sweep, the read-path benchmark plus lease reads (now the default; `read_e2e`), and the
feature-gated async WAL prototype with crash-recovery tests, the production configuration
decision record (two durability profiles and rejected options), the server-side write admission
control with `/admission/status`, and the pre-connected concurrency sweep that raised the default
from 256 to 1,024 requests. The public matching-engine test-suite inventory and two conformance
batches landed too: FOK atomicity, iceberg conservation/refill priority, stop-cascade conservation,
event-stream reconstruction, event-cursor survival across snapshot/restart and retention eviction,
GTD purge idempotence, post-only/amend interactions, fee conservation and the modify-priority
matrix (§17). OpenRaft observability now exposes `data_metrics`/`server_metrics`/`runtime_stats`
through `/metrics`, uses explicit heartbeat/snapshot channel profiles, and documents flamegraph
and tokio-console use in `cluster_e2e` (§16). The benchmark regression gate now runs in CI:
exact `apply_path` allocation ceilings plus loose `book_replay`/`replay_mix` p50/p99 bounds, with
a manual Linux workflow for the pinned-core `e2e` pass (§8). The four-voter no-fsync contract now
has a real-process leader-kill test: three survivors commit, and the restarted node catches up
after losing a truncated WAL tail (§7). Parked stops now accept direct trigger/limit/volume
amendments and icebergs accept total-open-volume amendments with priority rules (§5). The
market-data service records and replays per-symbol JSONL archives through the normal feed path
(§6). The §13 external contract is settled: zero-plus-`MatchErrors` remains for v11/v7 with exact
accessors, quote iceberg/FOK stay rejected, and fee tokens stay outside the engine. The measured
baselines, write/lease decisions, async-WAL numbers and upstream OpenRaft comparison are in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). The 249-engine FlashOne benchmark survey is
reviewed in [`docs/THIRD_PARTY_INVESTIGATION.md`](./docs/THIRD_PARTY_INVESTIGATION.md) Part IV; it
confirms the 7.20 M/s book-only B-tree result and the phantom-fill issue fixed in `a44d9ae`, and
opened the P1 follow-ups in §17. The benchmark correctness batch is implemented: explicit
conformance cases, `order_not_found` for cancel misses, the identity documentation and the
random-point state audit (§17). The occupancy envelope is also in place:
`cargo bench --bench book_replay -- --matrix` reports the four standing-book shapes and the
worst-case median-of-10 (§8). The schema-v11 order-type batch is implemented: snapshot v11,
market-data wire v7, market-to-limit, trailing stops, pegged orders and self-trade prevention
(§5).

The upstream OpenRaft minimal benchmark (three in-process nodes, in-memory log/state machine,
function-call network, empty requests) reports 33k put/s with one client and 3.5M put/s with 4,096,
while our 4-node no-fsync cluster reports 5,039 sequential and 35,152 concurrent writes/s. Raft is
not the current bottleneck; the application read/write path and benchmark isolation are.

Failure behavior is complete (§15): the node fail-stops on WAL `ENOSPC`/`EIO`, recovers from
`SIGKILL` during writes, keeps snapshot writes atomic, and a learner killed mid-snapshot-install
restarts without a partial snapshot and retries cleanly. Pre-Vote is enabled with a dedicated gRPC
RPC (§4), so an isolated node asks before campaigning. The wire v7 reference decoder and golden
tests are complete (§6).

An OpenRaft utilization review (2026-11-06, [`RAFT_REVIEW.md`](./docs/RAFT_REVIEW.md)) found the
transport already uses `stream_append` and streamed snapshots correctly. Its P1 quick wins are
implemented: the replication lag threshold now sits above the 100k snapshot policy, heartbeat
suppression is enabled, HTTP/2 windows/keepalive are tuned, snapshot chunks follow OpenRaft's
configured size and readiness waits for recovery. The remaining P2/P3 items are tracked in §16.

The next work, in order (re-prioritised 2026-11-10 after the Aeron heartbeat/request batch):

1. **Run the Linux pinned-core `e2e` pass and Linux Aeron validation (P2, §8/§6).**
   Dispatch the manual `Linux benchmark` workflow for the pinned short/sustained rows and run the
   external-driver plus multicast fan-out checklist in
   [`demos/market_data_system/README.md`](./demos/market_data_system/README.md). Both remain blocked
   on a Linux host; `gh` authentication is also needed for the workflow.
2. **Observability measurements and console actions (§16, P2).** Record heartbeat/`AppendStream`
   counts and snapshot-transfer time, and add guarded purge-log/runtime-config console actions.
3. **Benchmark P2 follow-ups (P2, §17).** Add the open-loop latency run (or the coordinated-omission
   caveat), re-pin the benchmark to current `HEAD`, and evaluate a treap/sorted-vector/heap hybrid
   only as a book-only A/B.
4. **Last: apply-path and per-level-bucket experiments (§8, P3).** Reserve known apply-path
   lengths, evaluate apply-thread affinity and adopt per-level buckets only if a sparse-level fast
   path or new measurements justify them; the measured wins are small or split.

The earlier measurement work is done: `book_replay` and `level_book` report warm p50/p99 plus an
interleaved cold pass, `replay_mix` replays a 100,000-operation fixture with fees, the 4-node
no-fsync cluster, the write concurrency/batching sweep, the read-path/lease results, the async-WAL
prototype and the upstream OpenRaft comparison are recorded in `PERFORMANCE.md`, and the `Order`
identity/optional-sequence/BTC-USDT/overflow-case quick wins landed. The earlier third-party
follow-ups are P2/P3 (§14); the new benchmark-derived follow-ups are P1/P2 (§17). Measurement
scope, peak RSS, the fuzz targets, health/readiness and the operational console are in place.
Equity/ETF sessions, holidays, auctions, T+1/T+2 settlement, corporate actions and
fixed/regulatory/minimum/tax fees are owned by the external order system (§13).

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

- [x] **P2** Add a provisioning helper for `ConfigureSymbol` and a pre-trade readiness check.
  Done: `MatchEngineClient::provision_symbol` validates/applies/verifies the configuration and
  refuses a locked or non-empty engine; `wait_for_pretrade_ready` polls `GET /pretrade/status`
  until healthy, recovered, leader-known, unlocked and empty. `/pretrade/status` is diagnostic
  (`200` with reasons); `/ready` remains the operational probe.

## 3. Public trust boundary and request safety [done]

- [x] **P0** Bound `client_order_id` to 1–128 bytes from `[A-Za-z0-9._:-]`;
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

- [x] Segmented WAL (`MRFT` header, CRC32-framed postcard records, one `write_all` per append
  batch plus the configured fsync policy (`RAFT_WAL_FSYNC_MS`: unset = none, `0` = every append,
  `N > 0` = periodic Tokio task), `pread` reads, torn-tail recovery).
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
- [x] Configurable durability: `RAFT_WAL_FSYNC_MS` selects no fsync (unset, the default), every
  append (`0`) or a periodic Tokio `wal-fsync` task (`N > 0`, with the active FD cloned under a
  short lock and `sync_data` on `spawn_blocking`). Non-every-append modes use best-effort WAL
  truncation so Raft can catch up from peers, and the binary requires four voters when fsync is
  disabled. Contract: [`docs/STORAGE.md`](./docs/STORAGE.md); measurements: §8.
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

- [x] **P2** Stream snapshot receive into a staging file instead of buffering the whole body.
  Done: `SnapshotData` carries a staged snapshot received as chunks; the state machine validates
  its length/CRC and writes the durable snapshot atomically. `Arc<[u8]>` carries locally built
  snapshots and `get_current_snapshot` no longer copies the body.
- [ ] **P2** Move off `openraft` 0.10.0-alpha once a stable 0.10 release exists; the latest
  published release is still `0.10.0-alpha.38` (checked 2026-11-10).
- [ ] **P3** The Aeron **Raft transport** remains gated on a measured bottleneck; the market-data
  Aeron egress is implemented (§6).

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
- [x] `Amend` for plain resting limit orders, icebergs and parked stops; same-price reductions keep
  priority, price/size changes re-insert under a new sequence, iceberg amendments preserve the
  display tranche, and stop amendments update trigger/limit/volume while keeping the stop sequence.
  Crossing amendments are refused.
- [x] `spread`, `mid_price` and `depth` views plus `orderbook_depth`/`orderbook_spread` read keys.
- [x] Randomized differential/property tests against a small reference model: visible book,
  fills, price-time priority, no crossed book, volume conservation, sequence/last-trade state and
  deterministic replay.
- [x] Randomized mixed-order coverage for GTC/IOC/FOK/GTD/Day, market, post-only, iceberg and
  stop orders, with positive, zero and rebate maker/taker rates under Received/Quote/Base fee
  policies.
- [x] Explicit conformance cases for market FOK, empty-book market orders, hidden iceberg
  cancel/expiry, parked-stop GTD/Day expiry and fee reporting on market/IOC/stop-child fills.
  The first public-suite batch adds rejected-FOK atomicity, iceberg partial-fill conservation,
  iceberg refill identity/queue priority and stop-cascade conservation (§17).
- [x] Prototyped per-price-level buckets (`BTreeMap<price, Vec<Order>>` + `head` cursor +
  sequence index + cached BBO) in `benches/level_book.rs`; the benchmark verifies the prototype
  against `OrderBook` before timing. Results are in `docs/PERFORMANCE.md`: match 1.76–2.15x,
  deep cancel 3.34–3.37x, same-price amend 1.70–1.75x, dense re-queue 2.58–2.62x and BBO query
  4.5–6.2x faster; sparse one-order-per-level re-queue 0.64–0.65x and BBO advance 0.87–0.90x
  slower; depth neutral. Full adoption remains P2 and is not justified without a sparse-level
  fast path.

Remaining:

- [x] **P2** Plan one schema-v11 order-type batch: market-to-limit, trailing stops, pegged orders
  and self-trade prevention. Done: [`docs/ORDER_TYPES_V11.md`](./docs/ORDER_TYPES_V11.md) fixes the
  data model, semantics, apply order, snapshot v10 -> v11, wire v6 -> v7 and the conformance/replay
  matrix. STP identity is `Order.owner_id: Option<u64>` plus `SymbolConfig.stp`, default `Off`;
  pegs reference non-pegged liquidity to make the reprice pass deterministic.
- [x] **P2** Implement the planned schema-v11 batch in one migration. Done: `Order` gained
  `owner_id`, `market_to_limit`, `trail_offset`/`trail_reference`, `peg`/`peg_offset`;
  `SymbolConfig` gained `stp`; `SNAPSHOT_VERSION` is 11 and `market_data::wire::VERSION` is 7.
  Market-to-limit, trailing stops, deterministic non-pegged peg repricing and all three STP
  modes are implemented, with conformance/property/snapshot/cluster coverage and docs in
  [`docs/ORDER_TYPES_V11.md`](./docs/ORDER_TYPES_V11.md), `docs/API.md`, `docs/MATCHING.md` and
  `docs/MATCH_RESULTS.md`. Custom reserve replenishment and stop-protection collars remain P3.
- [ ] **P3** Add custom reserve replenishment and stop-protection collars only if the product
  requires them; the current iceberg display tranche covers the existing reserve behaviour.
- [x] **P2** Phase 4 of match-result delivery: long-poll `/events` (`wait_ms`), gRPC
  `StreamEvents` and a reference MQ bridge sidecar (`src/bin/events_bridge.rs`) with cursor
  checkpointing; event time and cursor scope decided in `docs/MATCH_RESULTS.md` §4.8.
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
- [x] **P2** Aeron market-data egress and the reference consumer: `src/market_data/aeron.rs`
  (`market-data-aeron`, rusteron 0.2.13, embedded or external driver, one exclusive publication
  per stream) plus the periodic reset snapshot; `demos/market_data_system` ingests Aeron or JSONL
  and serves Binance-style WebSocket streams with depth conflation and subscribe-time snapshots.
  One instance serves multiple symbols behind a shared hub; a connection can hold several symbols'
  streams and switch them with live `UNSUBSCRIBE`/`SUBSCRIBE` (1024-stream cap, Binance parity).

Remaining:

- [x] **P2** Archive recording/replay. Done: `--archive-dir` records every consumed
  message to `<dir>/<symbol>.jsonl` with the engine's JSONL shape, and `--replay-dir` feeds those
  files through the normal assembler/WebSocket path without a media driver. A dedicated archive
  task flushes each line before yielding; `tests/archive.rs` proves record-then-replay equality.
- [x] **P2** Aeron operation hardening — implemented: idle STATUS heartbeat (`--market-data-aeron-heartbeat-ms`),
  `ws_fanout` benchmark with published numbers, and the stream-2010 snapshot-request stream on
  both engine and service. Linux external-driver/multicast fan-out validation remains blocked on
  this macOS host; the runbook and commands are in `demos/market_data_system/README.md`.
- [x] **P2** Add the snapshot-request stream (2003/2010). Done: fixed 24-byte `MSRQ` v1 request,
  engine subscription and state-machine capture, service request on empty/stale depth and gap,
  embedded-driver round-trip tests.

## 7. Tests and CI

Current coverage:

- Unit tests cover the order book, fixed-point parsing, Raft-address derivation and snapshot
  framing. `src/order_book/property_tests.rs` runs randomized traces against a reference model,
  including mixed order types and fee-rate styles; `conformance_tests.rs` covers the explicit
  edge cases (including the public-suite batches: rejected-FOK atomicity, iceberg partial-fill
  conservation/refill priority, stop-cascade conservation, post-only/amend interactions and the
  modify-priority matrix). `apply` event tests cover market, IOC, FOK, post-only, iceberg,
  stop-child cancellation and automatic GTD expiry; `gtd_purge_is_idempotent_and_auto_expiry_keeps_place_ordinals`
  pins repeated-purge idempotence and shared-entry ordinals. `src/market_data/wire.rs` tests the
  encoder and reference decoder, pins a v6 golden frame and rejects malformed frames.
- Fee unit tests cover combined-rate conservation on Base and Quote fees and the `Received`
  split where the base fee stays exact while the quote fee overflows.
- `test_restart.rs` also proves `/events` cursor stability across snapshot/restart and that an
  evicted cursor produces an explicit `oldest_event_id` gap while the retained tail stays
  duplicate-free.
- `tests/cluster/test_cluster.rs`: 3 nodes over gRPC, init/membership/read/write.
- `test_snapshot.rs`: snapshot build/purge and learner install over gRPC.
- `test_storage.rs`: WAL framing, torn-tail truncation, segment rotation, 20k-record multi-segment
  reopen, purge/truncate, vote/committed persistence, snapshot build/install/reopen, and the
  periodic fsync task's dirty-state lifecycle.
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
  rest/amend/fill/cancel, expiry and stop lifecycles emit explicit events; the `/events` log
  replays into a visible-book mirror that matches `/read orderbook_orders`; malformed bodies and
  unknown read keys are covered.
- `test_health.rs`: `/health` reports the gRPC listener, leadership and WAL/state-machine state;
  `/ready` is `503` before initialization and `200` after a leader is elected.
- `test_console.rs`: three-node cluster checks the embedded console assets, node/cluster APIs,
  unsafe-action rejection and leadership transfer through `/console/api/actions/transfer-leader`.
- `test_client.rs`: typed no-leader/API errors, retry/backoff after a dropped connection and
  forwarding to the new leader after failover.
- `test_failure.rs`: fail-stop when the Raft core stops, injected WAL `ENOSPC`/`EIO`, `SIGKILL`
  during a write stream and `SIGKILL` during a learner snapshot install.
- `test_min_voters.rs`: the four-voter no-fsync guard on `/init`, `/change-membership` and writes.
- `test_read_policy.rs`: default lease reads and explicit `read_index` reads serve `/linearizable_read` and `/events`, local `/read`
  bypasses the policy, and `/read-policy/status` reports lease hits and fallbacks.
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

Remaining (the public-suite follow-ups are tracked in §17):

- [x] **P2** Add a four-voter no-fsync failover test. Done:
  `four_voter_no_fsync_survives_leader_kill_and_tail_loss` runs four real binary nodes without
  WAL fsync, kills the leader, truncates the last three WAL records to model an unsynced tail
  loss, proves the remaining three voters commit and replicate three more orders, then restarts
  the killed node and waits for it to catch up and accept a post-restart write.
- [x] **P2** Parameterize the fixed test ports. Done: every integration suite renders HTTP/gRPC
  addresses through `tests/cluster/common.rs`; `MATCHENGINE_TEST_PORT_OFFSET=N` shifts the whole
  suite so a second `cargo test` process can run concurrently. `test_cluster.rs` no longer hardcodes
  21001–21003 in its membership or expected error text.
- [x] **P2** Keep `AGENTS.md` and this file in sync after code changes; both are maintained by
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
  sweep 85.7µs warm p50 (11.66M fills/s), 20k/1-level sweep 2.319ms (8.86M fills/s), 5k deep
  cancels 256.7µs (19.63M cancels/s), 1k-level BBO advance 97.5µs (20.37M advances/s), 1M BBO
  queries 2.492ms (434M queries/s).
- [x] **P1** Add the occupancy scenario matrix (`cargo bench --bench book_replay -- --matrix`):
  four standing-book shapes calibrated to the FlashOne seed-23 averages (static 20.9k/60 down to
  flash-crash 240/210), a 50k-op steady-state mix, occupancy sampled every 64 ops, and 10
  whole-replay trials reporting median/stddev/CV plus the worst-case scenario. The macOS baseline
  is in `docs/PERFORMANCE.md`; static is the worst case at 4.66-5.00 M ops/s.
- [x] **P1** Add an apply-path allocation benchmark (`cargo bench --bench apply_path`) and reduce
  the common event path with inline `SmallVec` buffers: rest 7.17→4.17, cancel 6.00→4.00, fill
  8.00→5.00 allocs/entry; batch rest 6.23→4.19 allocs/entry. Requested bytes/entry fell from
  2,323→595 (rest) and 1,308→156 (cancel); fill 3,308→2,012 and batch rest 2,393→1,693. The
  remaining allocations are the `BTreeMap` node, response `events` `Vec`, legacy `value` `String`
  and fill `MatchResult` `Vec`.
- [x] **P1** Report percentiles, not only best-of-5, and label cache warmth. Done:
  `benches/bench_support` clones before the timer and drops after it, reports warm p50/p99 plus
  best-of-5, and interleaves a cold pass with a 64 MiB eviction sweep. `book_replay` now shows the
  1k-ask sweep at 85.7µs p50 / 171.8µs p99; `level_book` keeps best-of-5 for continuity and uses
  p50 as the headline.
- [x] **P1** Add one realistic-mix replay benchmark (`cargo bench --bench replay_mix`): a
  deterministic 100,000-operation fixture (55,546 limit adds, 9,903 icebergs, 4,858 parked stops,
  9,928 cancels, 9,757 amends, 10,008 market takers) with maker 0.0002 / taker 0.0005 fees.
  Whole replay warm p50 107.252ms / p99 109.363ms (968,709 ops/s at the 103.230ms best round),
  cold p50 106.638ms / p99 108.522ms; per-operation p50/p99: all 791ns/3.041µs, limit add
  1.333µs/2.833µs, iceberg 1.375µs/2.834µs, stop 84ns/292ns, cancel 625ns/2.041µs, amend
  292ns/666ns, taker 1.417µs/6.833µs. `tests/replay` remains correctness-only; this is its load
  analogue.
- [x] **P1** Isolate OpenRaft on a dedicated multi-threaded runtime: set `RAFT_CORE_WORKERS` to
  a positive count to build with `Raft::build` and run on a `raft-worker` runtime; unset or `0`
  keeps `Raft::new` on the shared runtime. RaftCore and the state-machine worker no longer
  compete with HTTP/gRPC/feed tasks when enabled. A/B: the 2026-11-06 pairs were within noise
  (17.0/14.8k/s dedicated vs 17.8/19.2k/s shared); a 2026-11-07 spot rerun measured 9,776 shared
  vs 11,445 dedicated sequential writes/s and 30,204 vs 31,400 concurrent writes/s (both with
  fsync disabled). Apply can still migrate between the dedicated workers.
- [x] **P2** Complete the thread/lock audit: `FileLogSink` and startup WAL/snapshot recovery run on
  `spawn_blocking`, the JSONL sink and `events_bridge` use Tokio I/O, the Aeron publisher/source
  run as blocking tasks with their `!Send` rusteron handles inside the closure, and the periodic
  WAL fsync is a Tokio task. Only the env-controlled `RaftThread` remains an explicit OS thread;
  the demo Aeron poll loop keeps a blocking 100 µs sleep inside its `spawn_blocking` task.
- [x] **P1** Add configurable WAL fsync via `RAFT_WAL_FSYNC_MS` (unset = none, `0` = every
  append, `N` = periodic every N ms) and measure it: 2026-11-07 rerun e2e sequential p50 87.9µs
  (none), 88.2µs (10ms), 101.1µs (100ms), 3.898ms (every append); sequential throughput 9.6k /
  7.4k / 9.5k / 262.5 writes/s. Every-append is ~44x slower at p50; periodic 10ms costs ~24%
  throughput vs none and has a 2.2-3.0ms p99 tail. The periodic task clones the active file handle
  under a short lock and fsyncs the clone on `spawn_blocking`, so appends do not stall on the disk
  lock. Non-every-append modes use best-effort WAL truncation so Raft can catch up from peers, but
  data no peer has is unrecoverable.
- [x] **P1** Require four voters when WAL fsync is disabled: the binary sets `min_voters = 4`,
  `/init` and `/change-membership` reject smaller voter sets, and `/write`/`/write_batch` return
  `503` until four voters exist. Four voters keep a committed entry on at least three nodes, so one
  node's unsynced tail loss leaves at least two copies. Demo scripts default
  `RAFT_WAL_FSYNC_MS=0` for their three-node/single-node setups; `test_min_voters.rs` covers the
  guard.
- [x] **P1** Add a four-node no-fsync cluster benchmark (`cargo bench --bench cluster_e2e`):
  2026-11-07 medians 5,039 sequential writes/s (p50 186.75µs, p99 368.875µs), 35,152
  concurrent writes/s, 227,319 orders/s at 128-order batches and 216,163 orders/s for
  100×32 concurrent batches. Four in-process loopback voters, 3/4 quorum, no fsync.
- [x] **P2** Extend `cluster_e2e` with a client-count sweep (1/8/64/256/1024/4096) and a separate
  client runtime. Done: 2026-11-08 two-round medians peaked at 90,569 writes/s at 256 "in flight",
  but the later `--preconnect` run showed the 1,024/4,096 rows were TCP-setup-bound; the real
  plateau is 512-1,024 in flight at 99-117k writes/s.
  `RAFT_CORE_WORKERS=2` measured 65.7k/42.1k/46.8k, no consistent win.
- [x] **P2** A/B thin LTO + `codegen-units = 1` for release/bench builds. Done: paired control
  74.1k/40.2k/40.8k vs LTO 77.8k/45.9k/45.4k at 256/1,024/4,096; within the host's run-to-run
  spread, so the default profile stays.
- [x] **P2** Add p99-aware concurrency/admission control. Done: per-node
  `--max-inflight-writes` (default 1,024; `512` for a tighter tail, `0` disables) gates `/write`
  and `/write_batch`, returns `429` on overflow and exposes `GET /admission/status` counters.
  The 2026-11-09 pre-connected sweep supersedes the earlier connection-storm numbers: 512 in
  flight reaches 114,484 writes/s (p99 5.76ms) and 1,024 reaches 99,330 (p99 8.74ms); the old
  256 default rejected about a third of a 512-client burst and was raised. At 2,048 clients the
  512 gate cut p99 from 17.46ms to 13.39ms at 91.2k admitted writes/s. See
  `docs/PERFORMANCE.md`.
- [ ] **P3** Evaluate bounded waiting/backpressure as an alternative to immediate `429` when
  clients cannot retry promptly; keep the current fail-fast gate as the default until measured.
- [x] **P2** Add a benchmark regression gate (threshold or recorded baseline) to CI. Done:
  `cargo bench --bench apply_path -- --check` enforces exact allocation ceilings for blank, rest,
  cancel, fill and batch workloads (plus a small requested-bytes allowance);
  `book_replay --check` and `replay_mix --check` enforce loose warm p50/p99 bounds at least an
  order of magnitude above the recorded host baseline. The `bench-regression` CI job runs all
  three. The 4-node/write-sweep/read-path/async-WAL rows are already recorded in
  `docs/PERFORMANCE.md`; the stable fsync/cluster rows stay informational until their variance
  settles.
- [ ] **P2** Run `cargo bench --bench e2e -- --pin <cpu>` on a Linux host once, to separate
  scheduler noise from the fsync cost that dominates the current p99. The flag is implemented;
  this macOS host has no core-pinning API, so the recorded 2026-11-04 approximation is the
  current-thread runtime (p50 3.95ms / p99 5.22ms vs 3.93ms / 5.10ms multi-thread, i.e. no change).
  A manual `Linux benchmark` workflow (`.github/workflows/bench-linux.yml`) now runs the pinned
  short and sustained e2e suites on `workflow_dispatch`; it has not been dispatched yet.
- [x] **P2** Decide the periodic-fsync tail. Decision: production uses either disabled fsync
  with at least four voters (maximum throughput) or every-append fsync (durable writes). The 10 ms
  mode is dominated: 7,374 writes/s and a 2.2-3.0 ms p99. The 100 ms mode is within noise of
  disabled (9,495 vs 9,648 writes/s) but still lossy and has no four-voter guard, so it is not a
  default; use it only with four voters and an accepted 100 ms loss window. The FD-clone fsync
  removed the disk-lock stall; the remaining tail is filesystem-level. See the production
  configuration section in `docs/PERFORMANCE.md`.
- [ ] **P3** Reserve the known lengths for the remaining apply-path allocations (response `events`
  `Vec`, fill `MatchResult` `Vec`) where the shape is fixed; they are the `apply_path` leftovers.
- [ ] **P3** Decide whether strict apply-thread affinity is worth the cost: the single-worker
  dedicated runtime keeps apply on one thread but measured ~7% lower concurrent writes; a batched
  apply actor is the alternative.
- [ ] **P3** Adopt per-level buckets or a compact resting record only if the P1 prototype and
  new 3-node or real-sink measurements justify the complexity. The 2026-11-04 run wins 1.84x
  match, 3.31x cancel, 1.75x/2.59x amend and 4.90x BBO query, but loses 0.75x sparse amend, 0.87x
  BBO advance and 0.96x depth; full adoption is not justified yet, and a hybrid or sparse-level
  fast path is the next experiment.

## 9. Client library [done]

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
- [x] README/README_CN, AGENTS, TODO, PERFORMANCE, AERON, MATCH_RESULTS, MARKET_DATA_PLAN,
  FEE_DEDUCTION, DECIMAL, DEMO_ORDER_SYSTEM and MATCHING_ENGINE_TEST_SUITES are synced with the
  WAL + snapshot + gRPC design, snapshot v11 / wire v7 and the implemented demo crate.
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
- [x] **P2** Decide the event time source for the clearing feed and per-instrument cursor retention
  (Phase 4 in §5): events are ordered by `(log_index, ordinal)` only and carry no wall clock;
  the intake timestamp stays on the order/market data. Cursors are per deployment, which is
  already per instrument (one symbol per process/cluster). See `docs/MATCH_RESULTS.md` §4.8.

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
- [x] **P2** Asset freezing and fee funding are order-system/clearing responsibilities: the order
  system reserves the trading asset using the policy in `docs/FEE_DEDUCTION.md` §6.6, and clearing
  deducts each fill's fee from the freeze or the settlement leg. The engine holds no freeze state.
  The standalone demo desk renders a labelled estimate of the cash/spot freeze and settlement legs
  for demonstration (`demos/order_system/src/freeze.rs`); it does not enforce or persist a freeze. It also offers a
  demo-only linear quote-margined perpetual toggle with leverage, estimating notional, initial margin
  and fee without engine product/leverage fields or margin state.
- [ ] **P3** Engine-level derivative support (instrument kind, leverage/margin fields, contract
  multiplier, funding/PnL/liquidation) is out of scope for the cash/spot matching engine. The demo
  perpetual overlay is a UI/economics estimate only and must not be treated as an engine contract.
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

Resolved extensions:

- [x] **P2** Numeric sentinel policy: keep zero plus `MatchErrors` for snapshot v10/wire v6; a zero
  is authoritative only when its code is absent. `MatchResult` and `MatchEvent` expose
  `exact_notional`/`exact_quote_volume`, `exact_taker_fee` and `exact_maker_fee` accessors, and
  market-data `TradeRecord`/`LifecycleRecord` expose the same helpers. A nullable-field migration
  is explicitly rejected.
- [x] **P2** Mixed-unit iceberg/FOK: quote volume remains limited to plain limit and market orders;
  quote icebergs and FOK are deliberately rejected at admission. Clients submit base volume for
  those shapes; no normalization rule is planned.
- [x] **P3** Fee tokens: third-asset fees remain out of scope; the engine supports only base/quote
  fee assets, and conversion/balances/freeze deduction belong to the external order system.

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

- [x] **P1** Add percentiles and cache-warmth labels to the book benchmarks. Done: shared
  `benches/bench_support` with warm/cold, setup/drop exclusion and p50/p99 (tracked in §8).
- [x] **P1** Add the realistic-mix replay benchmark (tracked in §8, from the same re-review).
  Done: `cargo bench --bench replay_mix` replays a deterministic 100,000-operation
  add/cancel/amend/stop/iceberg/taker fixture with maker/taker fees and reports whole-replay
  warm/cold p50/p99 plus per-operation p50/p99/max; results are in `docs/PERFORMANCE.md`.
- [ ] **P3** Evaluate a binary order-entry path (gRPC/protobuf or compact codec) and feature-gated
  latency histograms only if a latency target requires them.
- [ ] **P3** Evaluate real ITCH-style replay data and custom MPMC/SIMD/io_uring work only after a
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

## 15. Failure behavior [done]

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
- [x] **P1** Four-voter no-fsync leader kill with an unsynced tail:
  `test_failure.rs::four_voter_no_fsync_survives_leader_kill_and_tail_loss` runs four real binary
  nodes without WAL fsync, kills the leader, drops three complete tail records from its last WAL
  segment, and proves the remaining three voters commit and replicate new writes. The killed node
  restarts from its truncated prefix, catches up, and accepts a subsequent write.

## 16. OpenRaft utilization review (2026-11-06)

The full review is in [`RAFT_REVIEW.md`](./docs/RAFT_REVIEW.md). The transport already uses
`RaftNetworkV2::stream_append` for replication and heartbeats and `full_snapshot` for snapshots;
these follow-ups are about aligning OpenRaft 0.10 configuration with this workload and reducing
storage/transport overhead. Re-prioritised 2026-11-08 after the upstream minimal-benchmark
comparison: the read path, payload/purge sweeps, admission control, async-WAL prototype and
observability are complete; next are the before/after measurements, async-WAL productionization
and optional APIs.

**Read path**

- [x] **P2** Add a read benchmark covering local `/read`, `/linearizable_read` and `/events` at
  1/64 readers, read-only and mixed with paced writes. Done: `cargo bench --bench read_e2e`.
  Read-only medians: `/linearizable_read` 8,092 → 17,559 reads/s at 1 reader and 30,232 → 51,777 at
  64; p50 119.7 → 54.9µs and 317.2 → 91.5µs. `/events` 5,100 → 7,461 and 32,979 → 38,856 reads/s.
  Local `/read` stayed at ~20k/50-59k. Mixed cases confirm the direction but are noisier.
- [x] **P2** Add an opt-in `ReadPolicy::LeaseRead` for `/linearizable_read`, `/events` and the
  gRPC `StreamEvents` anchor, with a `ReadIndex` fallback on lease expiry and lease-hit/fallback
  metrics. Done: `--read-policy lease` / `MATCHENGINE_READ_POLICY=lease`, `GET
  /read-policy/status`, per-path counters, `test_read_policy.rs`, and the bounded-clock-drift
  contract in `docs/API.md`. Both benchmark runs had 100% lease hits and no fallbacks on one host.
- [ ] **P3** Upgrade OpenRaft for `enable_local_lease_read`/`get_local_read_linearizer` and measure
  the full local-read path; the API is not in alpha.37. The current LeaseRead still passes through
  RaftCore and delivered 2.17x/1.71x linearizable-read throughput, not the upstream 90.4x/3.9x.
- [ ] **P3** Cache follower-read linearizers for a short TTL bounded by the leader lease, or add a
  lease-based `/get_linearizer` mode; measure the per-read leader hop.

**Configuration, transport and observability**

- [x] **P1** Align `replication_lag_threshold` with the snapshot policy. Done:
  `default_raft_config` sets 150,000, above the 100k `LogsSinceLast` policy; `test_late_join`
  and `test_snapshot` still pass.
- [x] **P1** Enable `heartbeat_min_interval` so sustained replication suppresses redundant
  heartbeat `AppendStream` sessions. Done: 500 ms, with
  `heartbeat_interval = 500` and `election_timeout_min = 1500`.
- [x] **P1** Tune HTTP/2 flow control and keepalive. Done: the server uses 1 MiB stream /
  4 MiB connection windows and 5 s / 3 s keepalive; the client uses explicit replication,
  heartbeat and snapshot profiles (see the P2 item below).
- [x] **P1** Honor `RPCOption::snapshot_chunk_size()` in `full_snapshot`. Done: chunks follow
  OpenRaft's configured size (3 MiB by default) capped at 8 MiB; `install_snapshot_timeout`
  remains intentionally unused by `full_snapshot`, which is bounded by cancellation and HTTP/2
  keepalive.
- [x] **P2** Enable OpenRaft's `runtime-stats` feature in a profiling build and expose
  `raft.runtime_stats()`, `data_metrics()` and `server_metrics()` in `/metrics`. Done:
  `--features profiling` enables the feature and `/metrics` returns `data_metrics`,
  `server_metrics` and the compact `runtime_stats` string (null without the feature); existing
  `RaftMetrics` consumers still deserialize the flattened fields. The optional `MetricsRecorder`
  was not installed: the runtime histograms already expose append/replication/apply batches.
  Flamegraph and tokio-console guidance is in `benches/cluster_e2e.rs` and `--help`.
- [x] **P2** Override `new_heartbeat_client` and `new_snapshot_client` explicitly with
  role-specific channel profiles. Done: `GrpcNetworkFactory` selects replication, heartbeat or
  snapshot profiles; heartbeat uses smaller windows and snapshot uses a longer connect/keepalive
  profile, while each network still owns its lazy channel. The unit test asserts the factory
  returns the expected role for each method.
- [ ] **P2** Record before/after measurements for heartbeat `AppendStream` count under sustained
  writes and snapshot transfer time. The post-change e2e/storage baseline is now in
  [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md); use it to confirm the 1 MiB / 4 MiB HTTP/2
  window choice before tuning further.
- [x] **P2** Benchmark `max_payload_entries` (300 vs 1,024), `purge_batch_size` (1 vs 1,024)
  and `api_batch_linger_ms` under the 4-node cluster and a client sweep. Done: max_payload 1,024
  measured 66.0k/42.5k/49.5k at 256/1,024/4,096 vs the 90.6k/44.8k/51.0k baseline; purge 1,024
  with a snapshot every 1,000 gave no consistent win; linger 1ms regressed sharply. The field is
  milliseconds, so 100/500µs is not representable; 100ms exhausted client ports. Keep 300/1/0.
  The append channel is bounded by count, not bytes, so large payloads can still queue memory;
  keep linger at 0 unless measurements justify the latency tradeoff.
- [ ] **P3** Benchmark `STREAM_APPEND_CHANNEL_SIZE` separately; it is a compile-time transport
  constant, so the runtime sweep above does not cover it.
- [ ] **P2** Add guarded console actions for `raft.trigger().purge_log()` and supported
  `raft.runtime_config()` changes after a snapshot; document the safety constraints.

**Storage**

- [x] **P1** Review `save_committed` durability and recovery readiness. Done: the marker stays
  non-fsynced, but `/ready` now requires `Raft::wait_for_recovery` to confirm the state machine
  applied the cluster commit covering the durable log tail. `/health` exposes the `recovered`
  flag for diagnostics.
- [x] **P2** Bound WAL append size by bytes, not only by OpenRaft's `max_payload_entries`.
  Done: `StorageConfig::max_wal_append_bytes` / `--max-wal-append-bytes` /
  `MATCHENGINE_MAX_WAL_APPEND_BYTES` (default 8 MiB; `0` disables chunking). One apply batch is
  split into size-bounded writes under one disk lock and one fsync; a failed chunk rolls the
  active segment back and removes segments created by the failed batch. Entries are never split.
- [x] **P2** Move snapshot data to an `Arc<[u8]>`-backed representation so
  `get_current_snapshot` and `build_snapshot` do not copy the whole body. Done: `StoredSnapshot`
  and `SnapshotData::Memory` hold `Arc<[u8]>`; the cached snapshot is shared by reference.
- [x] **P2** Stream snapshot receive instead of buffering the whole body. Done: the gRPC server
  writes chunks to a `snapshot-staging-*` file under the snapshot directory with a running CRC and
  size cap; `install_snapshot` verifies length/CRC and postcard content before the atomic durable
  write. Stale staging files are removed on open.
- [x] **P2** Prototype an asynchronous WAL append: insert into a pending-entry cache, write/fsync
  on a background task and call `IOFlushed` only when durable. Done: `async-wal` Cargo feature,
  `--async-wal`/`MATCHENGINE_ASYNC_WAL`, pending-entry reads, `truncate_after`/`purge` flush.
  Storage benchmark median 172,374 entries/s (range 65,176-192,160) vs 26,043 synchronous over
  five runs; `test_storage::async_wal_defers_io_flushed_and_recovers_a_lost_tail` proves the
  callback gate and lost-tail recovery, and `test_async_wal` proves a Raft node commits/reopens.
  Off by default; only every-append mode. A single-node e2e A/B found no consistent request-path
  gain: sequential writes stayed at ~258-260/s because each waits for `IOFlushed`, and concurrent
  writes were within run noise (18.3k sync vs 17.3k async median).
- [ ] **P3** Productionize async WAL only after a Linux multi-node A/B and pipelining work: add
  periodic-mode support if useful, bound the pending cache with backpressure, extend
  crash/failover coverage, and test whether more append concurrency exposes a real gain. The
  single-node A/B did not justify changing the default; keep the synchronous WAL as the default
  until then.
- [ ] **P3** When exposing the snapshot policy, expose `purge_batch_size` and
  `max_in_snapshot_log_to_keep` together; the current 100k snapshot policy makes the defaults
  mostly harmless, but a lower policy would otherwise cause frequent small purges.

**Optional APIs**

- [ ] **P3** Replace metrics polling in the market-data leader watcher with OpenRaft's
  `on_leader_change` callbacks (minor cleanup).
- [ ] **P3** Optional transport work: preconnect peers after startup/membership changes, larger
  HTTP/2 frames and snapshot compression. Persistent `AppendStream` sessions across OpenRaft
  replication sessions are not possible without changing the OpenRaft contract; heartbeat
  suppression is the practical way to reduce stream churn.

## 17. Public matching-engine test suites (2026-11-09, updated 2026-11-10)

Inventory, licence notes, coverage matrix and the full rationale:
[`docs/MATCHING_ENGINE_TEST_SUITES.md`](./docs/MATCHING_ENGINE_TEST_SUITES.md). No public suite
covers every order type, event reconstruction and replicated sequence invariants, so these cases
extend this repository's own property, conformance, replay and cluster suites. Feature work that
the inventory mentions (AON, self-trade prevention, order masks, multi-symbol isolation) stays in
§5/§13/§2 and is not duplicated here.

Done (2026-11-09; first batch in commit `acf8953`):

- [x] **P1** Add the first conformance batch: rejected-FOK atomicity (book, last trade and stops
  unchanged while one sequence is consumed), iceberg partial-fill/cancel conservation, iceberg
  refill identity/queue priority, and stop-cascade child conservation.
- [x] **P1** Reconstruct the visible book from `POST /events` and compare it with
  `/read orderbook_orders` across limit, IOC, FOK-reject, market and stop-cascade flows; assert
  page monotonicity/no duplication and record why parked stops and iceberg reserves are not
  reconstructible from `MatchEvent`.
- [x] **P1** Prove `/events` cursor stability across snapshot/restart and retention eviction:
  `event_cursor_resumes_across_snapshot_and_restart` and
  `event_cursor_reports_retention_gap_across_restart` assert stable `EventId`s, exact tail
  resume, no duplicates, and an explicit `oldest_event_id` gap when the cursor was evicted.
- [x] **P2** Add GTD purge idempotence and sequence-anchor coverage:
  `gtd_purge_is_idempotent_and_auto_expiry_keeps_place_ordinals` proves a repeated purge emits no
  duplicate expiry, and an automatic purge before a place keeps the place's sequence and the
  entry's ordinal order.
- [x] **P2** Add post-only/amend interactions: the flag survives a same-price reduction and a
  price move, a crossing amendment is refused without touching the book, and the refused
  amendment still consumes one sequence.
- [x] **P2** Add fee conservation coverage: Base/Quote fees conserve the combined rate, and the
  `Received` policy keeps the base fee exact when the quote fee overflows in either direction.
- [x] **P2** Add the modify-priority matrix: same-price reduce/increase, price up and price
  down, with the returned sequence and queue position asserted for each case.
- [x] **P2** The independent FIFO oracle/fuzzer already exists in
  `randomized_limit_book_matches_reference_model` (place/cancel/amend against a separate model,
  plus deterministic replay) and `randomized_order_types_and_fee_styles_preserve_invariants`
  (IOC among the mixed types). Added an operation-coverage guard so the generator cannot silently
  stop exercising place/cancel/amend.

Already covered before these batches (not backlog):

- Iceberg hidden reserve counts as reachable depth for FOK
  (`iceberg_reserves_count_as_reachable_depth_for_fill_or_kill`).
- A stop-limit child fills exactly at its limit and cancels the remainder without resting
  (`stop_limit_child_keeps_to_its_limit_and_never_rests`).
- GTD expiry at the deadline and one millisecond before it, Day expiry only when a close is
  supplied, and parked-stop expiry (`conformance_tests.rs`, `test_feature_matrix.rs`).
- Fee assets, maker rebates and exact/overflow fee ordering (`test_fees.rs`,
  `test_fee_assets.rs`, the replay matrix and `conformance_tests.rs`).

### FlashOne benchmark follow-ups (2026-11-10)

The 249-engine survey pinned this repository at `5597532` (before the phantom-fill fix) and
reported 7.20 M/s book-only; the full review and competitor taxonomy are in
[`docs/THIRD_PARTY_INVESTIGATION.md`](./docs/THIRD_PARTY_INVESTIGATION.md) Part IV.

- [x] **P1** Add the explicit benchmark-derived conformance cases. Done:
  `cancel_head_middle_and_scattered_orders_preserve_fifo`,
  `cancel_best_level_updates_bbo_without_a_follow_on_sweep`,
  `fills_print_at_the_maker_price_in_both_directions`,
  `multi_level_ioc_fills_best_price_first_and_cancels_remainder`,
  `marketable_limit_residual_rests_at_its_limit`,
  `deep_same_price_sweep_conserves_every_fill`,
  `near_limit_prices_rest_and_cancel`, and
  `cancel_and_amend_of_a_filled_order_are_rejected` in `src/order_book/conformance_tests.rs`.
- [x] **P1** Decide whether a non-resting cancel emits `Rejected`/4xx or stays a no-op, test it,
  and document `Order.id` vs `sequence` vs `client_order_id`. Done: `apply` now publishes
  `Rejected` with `OrderNotFound` for a cancel miss (and still consumes one sequence);
  `apply_rejects_cancel_and_amend_of_a_fully_filled_order` and the updated GTD/cancel test cover
  it, and `docs/API.md`, `docs/MATCH_RESULTS.md` and `AGENTS.md` document the identity and
  rejection contract. Duplicate client ids are metadata-only
  (`order_id_is_client_metadata_not_a_book_key`); submission-map eviction is already covered by
  `event_log_retains_pages_and_dedupes_submissions`.
- [x] **P1** Add a deterministic random-point book-state audit (best bid/ask/depth) to the
  reference-model tests. Done:
  `random_point_book_state_audit_matches_reference_model` probes 64 deterministic indices per
  seed against the reference model.
- [x] **P1** Add a `book_replay`/`replay_mix` scenario matrix (`static`, `normal`, wide-swing,
  flash-crash), record average/peak resting orders and levels, and report the worst-case
  median-of-10 with variance. Done: `cargo bench --bench book_replay -- --matrix` builds the four
  calibrated shapes, samples occupancy every 64 ops in an untimed pass, and reports median,
  population stddev, CV and the worst-case scenario; the local baseline is in
  [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).
- [ ] **P2** Add a paced/open-loop latency run, or state the coordinated-omission caveat for the
  closed-loop tails.
- [ ] **P2** Re-pin the benchmark to current `HEAD`, record the book-only result and scope in
  `docs/PERFORMANCE.md`, and publish the adapter only if it can be maintained.
- [ ] **P2** Evaluate a treap, sorted vector or heap hybrid only as a book-only A/B; do not switch
  on the external leaderboard alone.

### Remaining feature work

- [ ] **P3** Add AON, self-trade-prevention/order-mask and multi-symbol isolation cases only when
  the product adds those behaviours; multi-symbol remains deployment-level isolation (§2).
