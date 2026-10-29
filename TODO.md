# TODO

Last reviewed: 2026-10-29. This is the working roadmap; see `AGENTS.md` for commands and the
current branch state.

**Current baseline:** the OpenRaft 0.10 port, durable WAL/snapshot storage and exact scale-18
`Fixed` migration are done. Order prices/quantities, last trade, match events, depth/BBO, `/read`,
the market-data feed and wire v3 are exact; snapshot v7 stores the fixed-point state and the
single-symbol precision configuration. One process/cluster trades exactly one symbol; multi-symbol
is isolated by running one process per symbol. The remaining work is fee capture, public-API
hardening, stronger correctness evidence and measured performance.

Priority key: **P0** = correctness/security, do next; **P1** = production readiness; **P2** =
important but not blocking; **P3** = later/optional.

## Top priorities

1. **P0 — Add deterministic maker/taker fees.** Each order must carry its user-level maker and
   taker fee rates from placement; every fill computes exact notional, taker fee and maker fee
   without the engine knowing users or balances. Report fees through `/write`, `/events`, the
   file sink and (if needed) the market-data feed. See §12.
2. **P0 — Harden the public trust boundary.** `client_order_id` is unbounded, the gRPC listener is
   plaintext, and the snapshot receiver has no total-size cap. Bound IDs and request sizes, cap
   or stream snapshots, and decide TLS/mTLS plus HTTP authentication before any non-loopback
   exposure. Add hostile-input tests.
3. **P1 — Build stronger correctness evidence.** Add randomized differential/property tests for
   the book (volume conservation, no crossed book, price-time priority, deterministic replay).
   The 18-digit exactness round trip across API, events, feed, wire and snapshot is now covered by
   `tests/cluster/test_exactness.rs`; keep that guard while adding a reference model.
4. **P1 — Make failure and observability behavior explicit.** A dead gRPC server task currently
   leaves the HTTP API serving a node peers cannot reach; two `Logger` middleware log every
   request twice; `test_cluster` leaks fixed ports and running nodes. Decide fail-fast versus
   degraded mode, collapse logging, and make the test own and stop its nodes.
5. **P1 — Measure before optimizing.** Re-run the microbenchmarks after the `Fixed` migration, add
   submit → commit → apply → response latency and a 50k–500k fills/s feed load test. Only then
   consider per-level buckets or a compact resting record. The numbers in `docs/TRANSPORTS.md`
   are still guidance, not results.

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
  Wire **v3** encodes all prices/quantities as little-endian scale-18 `i128`.
- [x] Snapshot **v7** stores the last trade and match events as `Fixed` plus the single-symbol
  precision configuration; older snapshots are rejected.
- [x] `Fixed::notional` multiplies through an `i256` intermediate and truncates toward zero at 18
  places; unit tests cover an intermediate that overflows `i128`.
- [x] `tests/cluster/test_exactness.rs` round-trips 18 fractional digits through `/write`,
  `/read`, `/events`, the JSONL feed, snapshot v7 and a restart.
- [x] README/README_CN, AGENTS, DECIMAL, MATCH_RESULTS, MARKET_DATA_PLAN and AERON record wire v3,
  snapshot v7 and the new decimal JSON output.

Note: wire v3 and snapshot v7 are breaking changes; existing data directories must be recreated
and v2 feed consumers must implement the scale-18 integer layout.

## 2. One process, one symbol [done]

Decision: one process/cluster supports exactly one symbol. Multi-symbol runs one process/cluster
per symbol, so CPU, memory, logs, snapshots and failure domains stay isolated. The engine never
routes two books or switches symbols after trading starts.

Done:

- [x] Replicated `StateMachineData.symbol_config` + `symbol_locked` in snapshot v7.
  `MatchEngineRequest::ConfigureSymbol` sets the one symbol and its precision through Raft before
  trading.
- [x] The configuration is locked after the first accepted order; changing symbol or precision
  while orders are open, or after trading starts, is refused.
- [x] `Place` and `Amend` validate price/quantity decimals for that symbol; over-precision values
  produce a `rejected` event. `Cancel`, `Amend` and `Place` reject a different `symbol_id`.
- [x] `/read symbol_config` and `/read symbol_id` expose the configuration; a publishing
  market-data feed is reset to the new symbol after a successful configuration change.
- [x] `tests/cluster/test_symbols.rs` covers precision rejection, cross-symbol rejection, the
  symbol lock and snapshot restart.
- [x] Documented the deployment contract: one cluster/process group per symbol, separate data
  directories, ports, service-discovery names and market-data streams.

Remaining:

- [ ] **P1** Add a gateway/deployment-level symbol guard so a client cannot send a symbol-1 order to
  a symbol-0 process (the engine already rejects it; fail earlier and cheaper).
- [ ] **P2** Add a provisioning helper for `ConfigureSymbol` and a pre-trade readiness check.

## 3. Public trust boundary and request safety [P0/P1]

- [ ] **P0** Bound `client_order_id`: define a small byte limit (for example 128 bytes), an
  accepted character set and empty-string behavior. Today a client can grow the 262144-record
  dedupe map with arbitrarily large keys.
- [ ] **P0** Set explicit limits for `/write`, `/events` and `/read` payloads/cursors; reject
  oversized or malformed input before it reaches the Raft proposal.
- [ ] **P0** Cap the total bytes a snapshot stream may allocate, or spool chunks to a temporary
  file. The gRPC receiver currently accepts chunks until the stream ends while extending an
  in-memory buffer.
- [ ] **P0** Decide and document transport security: TLS/mTLS for the gRPC listener and
  authentication/authorization for the HTTP API, or an explicit loopback/private-network-only
  deployment contract. Nothing is configured today.
- [ ] **P1** Add a health/readiness endpoint that reports gRPC listener health, Raft leadership
  and storage state.
- [ ] **P1** Decide whether a died gRPC server task should stop the process (fail fast) or leave
  the node in a degraded, clearly reported state. `src/lib.rs` currently only logs it.
- [ ] **P2** Remove the duplicate `Logger` middleware in `src/lib.rs`; every request is logged
  twice.
- [ ] **P2** Make `Order::sequence` optional in the public deserializer (`#[serde(default)]`) so
  clients need not send a value that `place_order()` overwrites.
- [ ] **P2** Make `impl PartialEq for Order` include `id` (and the other identity fields); today
  two different orders with the same sequence/price/volume compare equal.

## 4. Persistence, Raft and transport

Persistence is done:

- [x] Segmented WAL (`MRFT` header, CRC32-framed postcard records, one `write_all` + one
  `sync_data` per append batch, `pread` reads, torn-tail recovery).
- [x] Atomic snapshots (`MRFSNAP`, temp file → fsync → rename → directory fsync) with validation
  before replacing the durable copy.
- [x] Per-node directories from `StorageConfig`; `--data-dir` / `--instance-prefix` and env
  equivalents.
- [x] Restart recovery via log replay and snapshot; large-log, large-snapshot and late-join
  coverage.

Remaining:

- [ ] **P2** Move off `openraft` 0.10.0-alpha once a stable 0.10 release exists; latest published
  is still `0.10.0-alpha.37`.
- [ ] **P2** Implement `RaftNetworkV2::pre_vote` over gRPC and
  `Raft::pre_vote()`, or explicitly document that Pre-Vote is unsupported and keep it disabled.
- [ ] **P2** Decide whether `transfer_leader` should use openraft's default or get an explicit
  transport implementation; record the decision.
- [ ] **P2** Consider streaming snapshot bytes into the state machine or a temp file instead of
  buffering the whole snapshot. The current API needs a contiguous buffer, but the receiver's
  total allocation still needs a policy (see §3).
- [ ] **P2** Add a benchmark harness for the transport comparison: L1 RPC microbenchmark and L2
  3-node commit latency over HTTP/JSON and gRPC. The current `docs/TRANSPORTS.md` numbers are
  guidance only.
- [ ] **P2** Revisit TLS and keep the gRPC listener's failure behavior aligned with the health
  policy in §3.
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

Remaining:

- [ ] **P0** Carry symbol isolation through the engine (see §2); the exactness migration is done.
- [ ] **P1** Evaluate per-price-level buckets (`BTreeMap<price, VecDeque<Order>>` plus a
  sequence-keyed index) after the post-`Fixed` benchmarks. The current per-order `BTreeMap`
  descends the tree for every fill; the extended `Order` also grew the map value, which shows up
  on copy-heavy benches.
- [ ] **P2** Amend parked stops (trigger price/volume) and icebergs (tranche semantics); consider
  a sequence → location index for cancel/amend by sequence alone, with index maintenance or
  tombstones.
- [ ] **P2** Port the missing OrderBook-rs types: trailing stops, pegged orders, custom reserve
  replenishment, stop-protection collars, market-to-limit and self-trade prevention.
- [ ] **P2** Phase 4 of match-result delivery: long-poll/short-poll `/events` (`wait_ms`), gRPC
  `StreamEvents`, and an optional MQ bridge sidecar. Decide event time and per-instrument cursors
  for multi-symbol deployments.
- [ ] **P2** Add randomized differential/property tests for the book (volume conservation, no
  crossed book, priority, deterministic replay) and keep a small reference model in the test
  suite.
- [ ] **P3** Consider protobuf/Avro on the public HTTP API. Raft RPCs already use postcard over
  gRPC.

## 6. Market data

Design: `docs/AERON_SOLUTION.md` Part II; implementation plan and progress:
`docs/MARKET_DATA_PLAN.md`.

Done:

- [x] Deterministic feed core: depth journal, `MatchEvent.side`, leader-stamped intake
  timestamps, shadow book, one TRADE per taker, BBO, depth snapshots/deltas, lifecycle, OHLC bars,
  status cadence, gap marker, leader gating with reset snapshots, JSONL sink and wire encoder v3.
- [x] Deterministic `market_seq`/`sub_seq`: identical keys across replicas, failover and restart.
- [x] `marketdata_status` read key.

Remaining:

- [ ] **P1** Run a 50k–500k fills/s load test and record feed latency, queue depth and drop/GAP
  behavior. The bounded `SyncSender` drops frames rather than blocking matching; operators need
  to know when that happens.
- [ ] **P1** Route the feed per symbol (see §2); the schema has `symbol_id`, but the engine still
  has one book.
- [ ] **P2** Fill `epoch` (leader generation) and `leader_id` instead of the current zeros; the
  header fields and leader watcher already exist.
- [ ] **P2** Add a consumer SDK/reference decoder for the wire format; the encoder has golden
  tests, the consumer side has none.
- [ ] **P2** Archive recording/replay and the snapshot-request stream (2003/2010).
- [ ] **P3** Aeron adapter: `MarketDataSink` with `rusteron-client` plus an embedded driver in
  tests, feature-gated (`market-data-aeron`); see AERON_SOLUTION §20/§23.

## 7. Tests and CI

Current coverage:

- Unit tests cover the order book, fixed-point parsing, Raft-address derivation and snapshot
  framing.
- `tests/cluster/test_cluster.rs`: 3 nodes, both transports, init/membership/read/write.
- `test_snapshot.rs`: snapshot build/purge and learner install over gRPC.
- `test_storage.rs`: WAL framing, torn-tail truncation, segment rotation, 20k-record multi-segment
  reopen, purge/truncate, vote/committed persistence, snapshot build/install/reopen.
- `test_restart.rs`: restart via log replay and via snapshot, 2k-order large-log replay and a
  multi-chunk (~120 KB) snapshot boot.
- `test_events.rs`: `/write` events, `/events` cursor, retention, submission dedupe.
- `test_match_results.rs`: file-sink rest/fill/cancel ordering, stop trigger, GTD expiry and read
  keys.
- `test_market_data.rs`: feed snapshot, trades, BBO, depth and bars.
- `test_membership.rs`: leader failover preserves the committed book and accepts writes; a learner
  catches up from a live leader.
- `test_late_join.rs`: a stale snapshot/WAL is superseded after a late rejoin.
- `test_exactness.rs`: 18-fractional-digit values survive `/write`, `/read`, `/events`, the JSONL
  feed, snapshot v7 and a restart.
- `test_symbols.rs`: one-symbol config, over-precision rejection, cross-symbol rejection, the
  post-trade config lock and snapshot restart.

Remaining:

- [ ] **P1** Randomized differential/property tests for matching invariants (see §5).
- [x] Exactness across snapshot/restart/events/feed is covered by `test_exactness.rs`.
- [ ] **P1** Per-symbol rejection tests once `SymbolConfig` is enforced (see §2).
- [ ] **P1** Hostile-input tests for oversized `client_order_id`, oversized `/write` and `/events`
  payloads, malformed JSON and oversized snapshots (see §3).
- [ ] **P1** End-to-end latency benchmark: submit → commit → apply → response, separated from
  matching and replication costs.
- [ ] **P2** Add negative API tests for unknown `/read` keys and malformed `Place` payloads.
  Follower forwarding is already covered.
- [ ] **P2** Parameterize ports and shut down the nodes in `test_cluster.rs`; it currently binds
  21001–21003 and leaves them running.
- [ ] **P2** Run the cluster suite over the HTTP transport in CI
  (`MATCHENGINE_TEST_TRANSPORT=http`), not just locally.
- [ ] **P2** Add an MSRV job for Rust 1.89; CI currently uses stable only.
- [ ] **P2** Keep `AGENTS.md` and this file in sync after code changes; both are maintained by
  hand.

## 8. Performance and capacity

- [x] Order-book micro-benchmarks: `cargo bench --bench matchengine` (wide/deep sweep, rest,
  rest with the market-data journal, partial fill, IOC cancel, iceberg refill, amend, cancel;
  best-of-5 per round).
- [ ] **P1** Re-baseline every benchmark after the fixed-point and last-trade changes; record the
  before/after numbers in `docs/DECIMAL.md`.
- [ ] **P1** Add end-to-end latency and throughput benchmarks (submit → commit → apply → response).
- [ ] **P1** Load-test the market-data feed and record queue/drop behavior (see §6).
- [ ] **P2** Consider per-level buckets or a compact resting record only if the post-`Fixed`
  numbers justify the complexity.
- [ ] **P2** Add a benchmark regression gate (threshold or recorded baseline) to CI.
- [ ] **P2** Build the transport benchmark harness (see §4).

## 9. Client library

- [ ] **P2** Harden `src/client.rs` before use outside examples: retry/backoff on transient RPC
  errors, configurable timeouts, and a typed "no known leader" error instead of a bare message.
- [ ] **P2** Add integration coverage for leader forwarding, failover and retry behavior.
- [ ] **P2** Support the chosen TLS/authentication configuration (see §3).

## 10. Docs, repo hygiene and naming

Done:

- [x] GitHub Actions runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
  warnings` and `cargo test`.
- [x] `Cargo.lock` is tracked; clippy is clean with `-D warnings`.
- [x] `scripts/` holds every shell helper; scripts resolve the repo root themselves and write
  logs under `logs/`, not the repository root.
- [x] `src/main.rs` is the only binary entry; `examples/sled.rs` is a dev-only playground and
  `sled` is a dev-dependency.
- [x] The unused `match_engine.png` was removed and `logs/` is gitignored.
- [x] README/README_CN, AGENTS, TODO, AERON and MATCH_RESULTS are synced with the WAL + snapshot
  + gRPC design and snapshot v7 / wire v3.
- [x] `docs/DECIMAL.md` records the fixed-point research, BTC 18-decimal range analysis,
  microbenchmarks and the completed end-to-end migration.

Remaining:

- [ ] **P2** Keep `AGENTS.md`, READMEs, `docs/DECIMAL.md` and this file in sync after future
  storage/wire changes.
- [ ] **P3** Decide whether to rename the remaining mixed-style modules before 1.0
  (`src/matchengine/` → `matching_engine/`, `src/match_result/` → `match_events/`,
  `src/network/raft.rs` → `http_raft.rs`, `raft_network_impl.rs` → `http_network.rs`). This is
  import-breaking and offers no functional benefit today; the package/binary name
  `matchengine-raft` is intentionally unchanged.

## 11. Decisions to record

These choices feed the priorities above; write them down as ADRs or doc notes when made.

- [x] `SymbolConfig` lives in replicated state as the single process/cluster symbol and locks
  after the first accepted order; precision changes are versioned by the Raft log/snapshot.
- [x] Wire v3 representation: scale-18 `i128` mantissas; v2 consumers must upgrade and frames are
  never silently reinterpreted.
- [ ] External exposure model: TLS/mTLS, HTTP authentication, and whether the API is
  loopback/private-network only.
- [ ] gRPC listener failure policy: fail-fast process exit vs degraded health state.
- [x] Multi-symbol isolation: one process/cluster per symbol, never multiple books in one process.
- [ ] Event time source for the clearing feed and per-instrument cursor retention.

## 12. Maker and taker fees [P0]

Goal: report the fee charged or rebated on every fill without teaching the matching engine about
users, tiers or balances. The gateway resolves a user level to a maker/taker rate and places those
rates on the order; the engine stores them in the replicated request and computes fees
deterministically.

Planned implementation:

- [ ] **P0** Add `maker_fee_rate` and `taker_fee_rate` to `Order` as signed `Fixed` fractions with
  `serde(default)` zero. Recommended convention: `"0.001"` means 10 bps; a negative rate is a
  rebate. Validate the accepted range (for example `-1..=1`) in `validate_order`.
- [ ] **P0** The receiving gateway sets both rates from the user level when placing the order.
  They travel in the Raft log and snapshot; the engine never looks up user information.
- [ ] **P0** Compute one notional and two fees per fill:
  - notional = maker price × matched quantity, through the `i256` path;
  - taker fee = notional × `taker.taker_fee_rate`;
  - maker fee = notional × `maker.maker_fee_rate`;
  - positive = charge, negative = rebate. Document the rounding mode (recommended: truncate
    toward zero at 18 places, then let settlement quantize to the quote asset's precision).
- [ ] **P0** A taker pays its taker rate on the current fill. A resting maker pays/rebates its
  maker rate when it is later filled. If a taker leaves a remainder that rests, its maker rate
  applies to its future fills.
- [ ] **P0** Add `notional`, `taker_fee` and `maker_fee` to `MatchResult`; add `taker_fee` and
  `maker_fee` to `MatchEvent` and `StoredMatchEvent`. Preserve them through `/write`, `/events`
  and `--match-event-log`; JSON emits decimal strings.
- [ ] **P0** Decide whether the market-data clearing records also carry fees. If `TradeRecord`
  and fill `LifecycleRecord` do, bump the feed wire format to **v4**; otherwise leave the feed
  untouched.
- [ ] **P0** Bump the snapshot to **v7** for the new order/event fields; older snapshots are
  rejected and existing data directories must be recreated.
- [ ] **P0** Preserve fee rates through amendments, iceberg refills, stop children and partial
  fills. Fee fields belong to the order, not to the book or the user; a tier change requires
  cancel/replace unless a later explicit rate-update request is added.
- [ ] **P1** Define and test extreme-value behavior: notional or fee outside the supported
  `i128` range should be rejected or reported explicitly, never silently charged as zero.
- [ ] **P1** Tests: maker and taker rates simultaneously; negative maker rebate; 18-digit rates
  and notional; partial fills; a taker that later rests; stop and iceberg children; duplicate
  submission replay; `/events` and file-sink JSON; snapshot restart; wire v4 if the feed changes.
- [ ] **P2** Update README/README_CN, AGENTS, MATCH_RESULTS, MARKET_DATA_PLAN, AERON and DECIMAL
  with the fee convention, JSON shape and compatibility versions.

Decisions to settle before coding:

- Rate unit: signed `Fixed` fraction (recommended for consistency with prices/quantities) versus
  integer bps/ppm.
- Rounding mode and settlement precision; fee currency is assumed to be the quote asset.
- Whether maker rebates are enabled by default and how negative fees are represented in reports.
- Whether fees belong in the market-data feed or only in the clearing/event feed.
- Validation range for rates and the handling of notional/fee overflow.
