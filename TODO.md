# TODO

Last reviewed: 2026-10-29. This is the working roadmap; see `AGENTS.md` for commands and the
current branch state.

**Current baseline:** the OpenRaft 0.10 port, durable WAL/snapshot storage and exact scale-18
`Fixed` migration are done. Order prices/quantities, last trade, match events, depth/BBO, `/read`,
the market-data feed and wire v5 are exact; snapshot v9 stores fixed-point orders/events, the
single-symbol precision configuration, asset metadata, volume units and maker/taker fee assets.
One process/cluster trades exactly one symbol; multi-symbol is isolated by running one process per
symbol. Base/quote volume and asset-aware fee deduction are done, and the market-data processor
runs as a tokio task. Edge request/snapshot limits and the loopback/private-network contract are in
place; the remaining work is stronger correctness evidence, failure behavior, measured performance
and TLS/authentication for future public exposure.

Priority key: **P0** = correctness/security, do next; **P1** = production readiness; **P2** =
important but not blocking; **P3** = later/optional.

## Top priorities

The P0 public-trust-boundary work and the P1 randomized differential/property tests are complete:
ids, request bodies, events pages, read keys and snapshot streams are bounded at the edge, and the
order book is checked against a reference model for pricing, priority, volume conservation and
deterministic replay, including mixed order types and fee-rate styles.

1. **P1 — Make failure and observability behavior explicit.** A dead gRPC server task currently
   leaves the HTTP API serving a node peers cannot reach; two `Logger` middleware log every
   request twice; `test_cluster` leaks fixed ports and running nodes. Decide fail-fast versus
   degraded mode, collapse logging, and make the test own and stop its nodes.
2. **P1 — Measure before optimizing.** The post-fee microbenchmark baseline is recorded in §8; add
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
  Wire **v5** encodes prices, quantities, quote volume and fees as little-endian scale-18 `i128`.
- [x] Snapshot **v9** stores the last trade, match events, asset metadata, tick/lot rules, fee
  assets and fill fees as `Fixed`, plus the single-symbol precision configuration; older snapshots
  are rejected.
- [x] `Fixed::notional` multiplies through an `i256` intermediate and truncates toward zero at 18
  places; unit tests cover an intermediate that overflows `i128`.
- [x] Fill fees use the taker order's taker rate and the maker order's maker rate; `MatchResult`
  carries notional/fees/`fee_overflow`, events and the JSONL feed carry both fees, and snapshot v9
  preserves the order rates and event fees.
- [x] `tests/cluster/test_exactness.rs` round-trips 18 fractional digits through `/write`,
  `/read`, `/events`, the JSONL feed, snapshot v9 and a restart.
- [x] README/README_CN, AGENTS, DECIMAL, MATCH_RESULTS, MARKET_DATA_PLAN and AERON record wire v5,
  snapshot v9 and the new decimal/fee JSON output.

Note: wire v5 and snapshot v9 are breaking changes; existing data directories must be recreated
and older feed consumers must implement the scale-18 integer layout, volume units and fee assets.

## 2. One process, one symbol [done]

Decision: one process/cluster supports exactly one symbol. Multi-symbol runs one process/cluster
per symbol, so CPU, memory, logs, snapshots and failure domains stay isolated. The engine never
routes two books or switches symbols after trading starts.

Done:

- [x] Replicated `StateMachineData.symbol_config` + `symbol_locked` in snapshot v9.
  `MatchEngineRequest::ConfigureSymbol` sets the one symbol and its precision through Raft before
  trading.
- [x] The configuration is locked after the first accepted order; changing symbol or precision
  while orders are open, or after trading starts, is refused.
- [x] `Place` and `Amend` validate price/quantity decimals, price tick and base lot for that
  symbol; invalid values produce a `rejected` event. `Cancel`, `Amend` and `Place` reject a
  different `symbol_id`.
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

## 3. Public trust boundary and request safety [P1]

- [x] **P0** Bound `client_order_id` to 1–128 printable ASCII bytes from `[A-Za-z0-9._:-]`;
  empty strings are rejected and invalid ids return `400` before the Raft proposal.
- [x] **P0** Cap application JSON bodies at 1 MiB, validate `/read` keys at 1–128 printable ASCII
  bytes, and reject `/events` limits outside 1–1000 with `400`. Malformed JSON and oversized
  bodies fail in the extractor.
- [x] **P0** Cap HTTP snapshot bodies and gRPC snapshot streams at 256 MiB, checking the gRPC
  cumulative total before each chunk is appended.
- [x] **P0** Adopt and document a loopback/trusted-private-network-only deployment contract because
  TLS/mTLS and HTTP authentication are not implemented. The node warns when bound to a
  non-loopback address.
- [ ] **P1** Implement TLS/mTLS and API authentication before allowing public exposure, then
  replace the loopback/private-network warning with an enforced policy.
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
- [x] Default snapshot policy: `LogsSinceLast(100_000)` plus a 30-minute timer that only fires
  when newer entries exist; `StorageConfig::snapshot_interval` and `--snapshot-interval-ms` tune
  the timer (`0` disables it).

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
- [x] Randomized differential/property tests against a small reference model: visible book,
  fills, price-time priority, no crossed book, volume conservation, sequence/last-trade state and
  deterministic replay.
- [x] Randomized mixed-order coverage for GTC/IOC/FOK/GTD/Day, market, post-only, iceberg and
  stop orders, with positive, zero and rebate maker/taker rates under Received/Quote/Base fee
  policies.
- [x] Explicit conformance cases for market FOK, empty-book market orders, hidden iceberg
  cancel/expiry, parked-stop GTD/Day expiry and fee reporting on market/IOC/stop-child fills.

Remaining:

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
- [ ] **P3** Consider protobuf/Avro on the public HTTP API. Raft RPCs already use postcard over
  gRPC.

## 6. Market data

Design: `docs/AERON_SOLUTION.md` Part II; implementation plan and progress:
`docs/MARKET_DATA_PLAN.md`.

Done:

- [x] Deterministic feed core: depth journal, `MatchEvent.side`, leader-stamped intake
  timestamps, shadow book, one TRADE per taker, BBO, depth snapshots/deltas, lifecycle, OHLC bars,
  status cadence, gap marker, leader gating with reset snapshots, JSONL sink and wire encoder v5.
- [x] Deterministic `market_seq`/`sub_seq`: identical keys across replicas, failover and restart.
- [x] `marketdata_status` read key.
- [x] The processor runs as a tokio task over a bounded tokio `mpsc` queue, with `watch`-driven
  shutdown and `Notify`-driven snapshot wakeups; `try_send` keeps the matching path non-blocking.
- [x] Processor state and shutdown are covered by unit tests and `test_market_data.rs`.

Remaining:

- [ ] **P1** Run a 50k–500k fills/s load test and record feed latency, queue depth and drop/GAP
  behavior. The bounded tokio `mpsc` queue drops frames rather than blocking matching; operators
  need to know when that happens.
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
  framing. `src/order_book/property_tests.rs` runs randomized traces against a reference model,
  including mixed order types and fee-rate styles; `conformance_tests.rs` covers the explicit
  edge cases. `apply` event tests cover market, IOC, FOK, post-only, iceberg, stop-child
  cancellation and automatic GTD expiry.
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
  feed, snapshot v9 and a restart.
- `test_symbols.rs`: one-symbol config, over-precision rejection, cross-symbol rejection, the
  post-trade config lock and snapshot restart.
- `test_fees.rs`: maker/taker fee rates, `/write`/`/events`/file-sink/feed fees and snapshot restart.
- `test_fee_assets.rs`: ETF/fiat symbol metadata, tick/lot enforcement, quote-volume limit
  normalization and quote-asset fee deduction.
- `test_request_limits.rs`: oversized bodies, malformed JSON, invalid/oversized client ids,
  oversized read keys and out-of-range event pages are rejected before Raft.

Remaining:

- [x] **P1** Randomized differential/property tests for matching invariants (see §5).
- [x] Exactness across snapshot/restart/events/feed is covered by `test_exactness.rs`.
- [x] Per-symbol rejection, precision and lock tests are covered by `test_symbols.rs` (see §2).
- [x] **P0** Hostile-input tests for oversized `client_order_id`, oversized `/write` and `/events`
  payloads, malformed JSON, oversized read keys and oversized snapshots (see §3).
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
- [ ] **P1** Port the current post-fee baseline into `docs/DECIMAL.md`. The 2026-10-29 run
  measured: sweep 1k/100 levels 84.1µs, 20k/1 level 2.098ms, rest 43.2µs, rest+journal 46.6µs,
  partial 57.2µs, IOC 110.6µs, iceberg 57.8µs, amend 79.3µs, cancel 91.8µs. Fee calculation adds
  i256 per-fill work; decide whether a separate no-fee fast path is worth it.
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
- [x] README/README_CN, AGENTS, TODO, AERON, MATCH_RESULTS, MARKET_DATA_PLAN and FEE_DEDUCTION are
  synced with the WAL + snapshot + gRPC design and snapshot v9 / wire v5.
- [x] `docs/DECIMAL.md` records the fixed-point research, BTC 18-decimal range analysis,
  microbenchmarks and the completed end-to-end migration.

Remaining:

- [ ] **P2** Keep `AGENTS.md`, READMEs, `docs/DECIMAL.md` and this file in sync after future
  storage/wire changes.
- [x] **P3** Rename `src/matchengine/` to `src/order_book/` (2026-10-29); the public module is
  now `order_book`, while the package/binary name `matchengine-raft` is intentionally unchanged.
- [ ] **P3** Decide whether to rename the remaining mixed-style modules before 1.0
  (`src/match_result/` → `match_events/`, `src/network/raft.rs` → `http_raft.rs`,
  `raft_network_impl.rs` → `http_network.rs`). This is import-breaking and offers no functional
  benefit today.

## 11. Decisions to record

These choices feed the priorities above; write them down as ADRs or doc notes when made.

- [x] `SymbolConfig` lives in replicated state as the single process/cluster symbol and locks
  after the first accepted order; precision changes are versioned by the Raft log/snapshot.
- [x] Wire v5 representation: scale-18 `i128` prices, quantities, quote volume and fees plus
  volume units and fee assets; older consumers must upgrade and frames are never silently
  reinterpreted.
- [x] External exposure model: no TLS/mTLS or HTTP authentication; the supported contract is
  loopback or a trusted private network, with edge request/snapshot limits.
- [ ] TLS/mTLS and HTTP authentication for any future non-private exposure.
- [ ] gRPC listener failure policy: fail-fast process exit vs degraded health state.
- [x] Multi-symbol isolation: one process/cluster per symbol, never multiple books in one process.
- [ ] Event time source for the clearing feed and per-instrument cursor retention.

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
- [x] Per fill, notional = maker price × matched quantity through the `i256` path; taker fee =
  notional × taker's taker rate; maker fee = notional × maker's maker rate. Fees truncate toward
  zero at 18 places. Positive = charge, negative = rebate.
- [x] A taker that later rests uses its maker rate on future fills. Amendments, iceberg refills and
  stop children preserve the original order's rates.
- [x] `MatchResult` carries `notional`, `taker_fee`, `maker_fee` and `fee_overflow`.
  `MatchEvent`/`StoredMatchEvent` carry both fees and the flag.
- [x] `/write` events, `/events`, the JSONL file sink and market-data `TRADE`/fill `LIFECYCLE`
  records expose the fees and their assets. Wire is **v5**.
- [x] Snapshot **v9** persists order rates, fee assets and event fees.
- [x] Tests cover simultaneous maker/taker rates, negative maker rebates, rate validation, fee
  overflow, 18-digit exactness, a taker that later rests, file/event/feed output and snapshot
  restart.

Remaining (outside the matching engine):

- [ ] **P2** Define fee accounts/balances and settlement ownership in the clearing system. The
  engine reports exact fees, their assets and the policy; clearing applies final asset rounding and
  moves funds.
- [ ] **P2** Decide whether fee tiers can change on a live order. Today a change requires
  cancel/replace; amendments preserve the original rates.

## 13. Base/quote volume and asset-aware fee deduction [done]

Design: [`docs/FEE_DEDUCTION.md`](./docs/FEE_DEDUCTION.md).

Done:

- [x] `SymbolConfig` carries base/quote `AssetId`/`AssetClass`, per-asset settlement decimals,
  `price_tick`, `base_lot` and `FeeAssetPolicy`.
- [x] `Order.volume_unit` (`Base`/`Quote`) is replicated; quote limit orders normalize to base at
  the limit price and zero-lot conversions are rejected.
- [x] Quote-aware market sweeps: buy spends a gross quote budget; sell targets gross quote proceeds
  and rounds the final base lot up, overshooting by at most one lot.
- [x] Fee asset policies: received asset (default), quote asset and base asset. Events report the
  actual fee asset per side.
- [x] Gross base/quote volume, taker/maker fee assets and fees flow through `MatchResult`,
  `MatchEvent`, `/events`, the file sink and the market-data feed.
- [x] Tick/lot increments and per-asset precision are enforced on `Place` and `Amend`.
- [x] Snapshot **v9** and market-data wire **v5** are implemented; old data directories must be
  recreated.
- [x] Tests cover QQQ/USD (ETF/fiat) tick/lot and quote volume in
  `tests/cluster/test_fee_assets.rs`, plus quote fees, fee overflow, fee policies, the file sink,
  `/events`, the feed and snapshot restart.

Remaining extensions:

- [ ] **P1** Define gateway responsibilities for equity/ETF sessions, holidays, auctions, T+1/T+2
  settlement and corporate actions. The deterministic engine must not read a clock.
- [ ] **P1** Decide whether fixed per-trade fees, per-share commissions, regulatory fees, minimum
  tickets or taxes are in scope; notional-proportional rates do not cover them.
- [ ] **P2** Add third-asset fee-token support only if a fee price and balance model are provided.
- [ ] **P2** Decide whether mixed-unit iceberg/FOK orders should normalize tranches; quote volume
  currently supports plain limit and market orders only.
- [ ] **P2** Add a BTC/USDT (crypto/stablecoin) quote-volume case alongside the QQQ/USD test.

Resolved decisions: quote limit sell rounds base quantity down to cap notional; quote market sell
rounds up and permits a bounded overshoot; `volume` is always gross; the fee policy is per symbol;
quote market-buy budgets are gross notional with quote fees charged additionally; the engine
reports exact 18-scale fees and clearing applies final asset rounding.
