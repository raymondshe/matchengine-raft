# TODO

Current snapshot of known gaps. Priorities assume the goal is to finish the openraft 0.10 port and restore the features the README advertises. This file is the working TODO list and roadmap: the `Future Work` lists that used to live in `README.md` / `README_CN.md` were folded into the sections below. See `AGENTS.md` for day-to-day commands and branch state.

## 1. openraft 0.10 migration — done in code, two loose ends

Done: dependency moved to `openraft` 0.10.0-alpha.37 from crates.io, `openraft-legacy` removed, `src/network/raft_network_impl.rs` implements `RaftNetworkV2` natively, `POST /snapshot` uses `Raft::install_full_snapshot` with the v2 `FullSnapshotRequest`, and `SnapshotData` moved from `RaftTypeConfig` to `RaftStateMachine`/`NetSnapshot`.

- [ ] Move off the alpha once openraft 0.10 ships a stable release (also drop any remaining `0.10.0-alpha` churn in `Cargo.lock`).
- [ ] `RaftNetworkV2::pre_vote` is left at its default (unconditional grant), so openraft's Pre-Vote is a no-op here. Implement it over a gRPC `PreVote` call + `Raft::pre_vote()` if Pre-Vote is ever enabled.
- [ ] `transfer_leader` uses the default. `stream_append` is now a bidirectional gRPC stream on the gRPC transport; the HTTP transport still falls back to sequential unary calls.

## 2. Persistence — done

Storage is now durable: `src/store/log_store.rs` is a hand-written segmented write-ahead log
(`MRFT` segments, CRC32-framed postcard records, one `write_all` + one `sync_data` per openraft
append batch, `pread` reads), and `src/store/state_machine.rs` keeps the state machine in memory
but writes `snapshot/current.snap` atomically (temp file → fsync → rename → dir fsync).

Layout per node is `<data_dir>/<instance_prefix>-<node_id>/{log,snapshot}`; `StorageConfig`
(`src/store/config.rs`) selects it and the binary exposes `--data-dir` / `--instance-prefix`
(`RAFT_DATA_DIR` / `RAFT_INSTANCE_PREFIX`). Recovery = load the latest snapshot, then let openraft
replay the retained log tail on top. Vote and `last_purged` live in an atomically rewritten
`log/state` file; `committed` rides on the WAL tail and is made durable by the next append.
The dead sled file `src/store/store.rs` was deleted.

- [x] Persistent Raft log and vote storage.
- [x] Durable state machine snapshots with atomic replacement.
- [x] Per-node data directories and CLI/env configuration.
- [x] Restart recovery covered by `tests/cluster/test_restart.rs` (log replay and snapshot paths).
- [x] Storage unit/integration tests in `tests/cluster/test_storage.rs`.

## 3. Correctness / robustness in the request path

- [x] Replace `.decompose().unwrap()` in `src/network/api.rs` and `src/network/management.rs` with real error handling. Forwarding already returned the `ForwardToLeader` body; the unwrap panicked on fatal `RaftError`s. Fatal errors now return HTTP 500 instead of killing the worker.
- [x] `linearizer.await_ready(...).unwrap()` is gone: fatal wait errors return HTTP 500, and recoverable `LinearizableReadError`s stay in the JSON result.
- [x] Scripts now send the correct `sequence` field.
- [x] Fix issue #1: `place_order` no longer emits a phantom zero-quantity
  `MatchResult` when the taker is exactly filled, and partially filled makers are
  shrunk in place instead of rebuilt with `Order::new` (which reset `id` to 0).
  Covered by `exact_fill_*`, `partial_fill_preserves_maker_identity` and
  `full_sweep_leaves_book_empty_without_phantom_trades` in `src/matchengine/mod.rs`.
- [ ] `Order::sequence` is still required by the deserializer even though `place_order()` overwrites
  it; make it optional (`#[serde(default)]`) so clients need not send a value the engine ignores.
- [x] Collapse the three duplicated `orderbook_sequance` compatibility arms in
  `src/network/api.rs` into one `read_state` helper used by `/read`, `/linearizable_read` and
  `/follower_read` (done together with removing the KV store).
- [ ] `src/lib.rs` registers two `Logger` middlewares — every request is logged twice. Keep one.

## 4. Dependency cleanup

- [x] Unused dependencies: `uuid` and `rand` were removed earlier; `thiserror` and `async-trait` are now removed too. `walkdir` went with the deleted `src/store/store.rs`; `sled` is only used by the playground binary `src/bin/sled_test.rs`.
- [x] Dependency versions are current (`actix-web 4.18.1`, `env_logger 0.11.11`, etc.) and `Cargo.lock` is committed.

## 5. Tests

Unit tests cover the order book, the Raft-address derivation and the snapshot file format.
Integration coverage is `tests/cluster/test_cluster.rs` (3 nodes, both transports),
`tests/cluster/test_snapshot.rs` (snapshot build, purge, learner install over gRPC),
`tests/cluster/test_storage.rs` (WAL framing, torn-tail truncation, purge/truncate across segments,
vote/committed persistence, snapshot build/install/reopen), `tests/cluster/test_restart.rs`
(full node restart via log replay and via snapshot), `tests/cluster/test_events.rs`
(`/write` events, `/events` cursor, retention, submission dedupe), and
`tests/cluster/test_membership.rs` (leader failover preserves the committed book and accepts writes;
a new learner catches up from a live leader through log replication).

- [x] Order-book unit tests cover partial fills, maker identity, priority, IOC/FOK, icebergs, stops, expiry, amend rules and depth views.
- [ ] Add negative API tests for unknown `/read` keys and malformed `Place` payloads (follower forwarding is covered).
- [ ] `tests/cluster/test_cluster.rs` binds fixed ports 21001-21003 and leaves 3 nodes running after the test finishes; parameterize ports and shut the nodes down.
- [x] Test restart recovery (both log replay and snapshot boot paths).
- [x] Test match-result processing: `apply` -> sink events (rest/fill/cancel), failing-sink
  isolation, file-sink JSONL format, and an end-to-end `--match-event-log` node test.

## 6. Docs

- [x] `README.md` API list corrected to the real routes: `/vote`, `/append`, `/snapshot`,
  `/get_linearizer`, `/linearizable_read`, `/follower_read`, `/read`.
- [x] `README.md` snapshot section now describes `default_raft_config()` and the test-only
  `LogsSinceLast(10)` / `max_in_snapshot_log_to_keep = 0` override.
- [x] `README.md` prerequisites updated to Rust 1.85 (edition 2024) and note that protoc is vendored.
- [x] README now describes the segmented WAL + snapshot storage design instead of Sled.
- [x] `README.md` / `README_CN.md` project structure, transport flags, test suite and future work
  synced with `src/network/grpc/` and `tests/cluster/`.
- [x] `GUIDE_cn.md` folded into `README.md` / `README_CN.md` and removed; its unique WAL, snapshot,
  transport, event-log, performance and roadmap details now live in the READMEs.
- [x] `docs/AERON_SOLUTION.md` and `docs/TRANSPORTS.md` updated: gRPC is adopted, storage is
  durable, and all remaining performance numbers are labelled as guidance.
- [x] `docs/MATCH_RESULTS.md` writes down how fills/cancels reach the order/clearing system.
  Phases 1a–3 are implemented (write-response events, replicated event log, `/events`, dedupe);
  phase 4 (streaming/MQ bridge) remains optional.
- [ ] Keep `AGENTS.md` and this file in sync after code changes; both are maintained by hand.

## 7. Scripts

- [x] `open-cluster.sh` now calls `/init` and forms a cluster.
- [x] **Fixed `/init` in every script.** `test-cluster.sh`, `open-cluster.sh` and `test.sh build-cluster`
  send `'[]'` (a single-node init); previously `'{}'` was rejected by openraft.
- [x] Scripts send `sequence`, not `sequance`, and `test.sh get-seq` reads `orderbook_sequence`.
- [x] `test.sh` `build-cluster`/`metrics`/`place-order`/`get-seq` default to node 1 when no node is
  given; `change-membership` defaults to `[1,2,3]`.
- [x] Order loops use a POSIX arithmetic loop instead of bash-only `JSON+=` / `{1..1500}`.
- [ ] `test.sh` and `test-match.sh` still duplicate the same order-placement loop; `test-matrics.sh`
  is a one-call stub. Consolidate.

## 8. Repo hygiene

- [x] GitHub Actions runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` (`.github/workflows/ci.yml`).
- [x] `Cargo.lock` is no longer ignored and is part of this change (commit it with the source).
- [x] The crate-level `#![allow(clippy::uninlined_format_args)]` is gone; clippy is clean with `-D warnings`.

## 9. Matching engine

- [x] Route match results through a replaceable `MatchResultSink` (`MatchEvent` with
  `(log_index, ordinal)` ids; `Noop`/`Memory`/`File` sinks; `--match-event-log` file log for
  debugging). `apply` publishes best-effort and never lets a sink failure affect the state machine.
- [x] Deliver fills and cancels durably to the order/clearing system: events in the `/write`
  response (capped at 512 with `events_truncated`), a bounded replicated event log snapshotted with
  the state (v4), `POST /events` cursor reads, `GET /events/status`, and `client_order_id` submission
  dedupe. Design: `docs/MATCH_RESULTS.md` (phases 1b–3).
- [ ] Phase 4 of match-result delivery: long-poll/short-poll `/events` (`wait_ms`), gRPC
  `StreamEvents`, and an optional MQ bridge sidecar. Also decide event time in the clearing feed
  and per-instrument cursors once multi-symbol lands.
- [x] Order types (semantics modelled on
  [OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs)): GTC, IOC, FOK, GTD, Day,
  post-only, iceberg (display tranche + hidden reserve), market, stop-market and stop-limit.
  Expiry is deterministic via `PurgeExpired { now_ms, day_close_ms }`; stops park off the book and
  are elected by the replicated last trade price. Events: `triggered`, `expired`, `rejected` join
  `fill`/`rested`/`cancelled`.
- [x] Order amendment (`Amend`): price and/or open volume of a plain resting limit order, keyed by
  side + current price + sequence. Same-price reductions keep time priority; price changes and size
  increases re-insert under a new sequence. Crossing amendments are refused; icebergs, stops and
  market orders are cancel + re-place. `spread`/`mid_price`/`depth` helpers and the
  `orderbook_depth`/`orderbook_spread` read keys are in place.
- [ ] Amend parked stops (change trigger price/volume) and icebergs (tranche semantics); a
  sequence→location index would also allow cancel/amend by sequence alone, at the cost of index
  maintenance in the sweep or tombstone compaction.
- [ ] OrderBook-rs types not ported yet: trailing stops (watermark + trail amount, child on a new
  print), pegged orders (reference price offsets), custom reserve replenishment, stop-protection
  collars, market-to-limit, and self-trade prevention.
- [x] Automatic GTD purge: every admissible `Place`/`Cancel`/`Amend` purges GTD orders due at its
  intake timestamp before applying, using the replicated sequence/timestamp. `Day` still needs the
  explicit `PurgeExpired { now_ms, day_close_ms }` because it requires a market close.
- [ ] `price`/`volume` are `f64` — money should use integer ticks/decimal.
- [ ] The book is a per-order `BTreeMap<BidKey/AskKey, Order>`; every fill descends the tree once
  and sweeps touch one entry per order. Bucketing orders per price level
  (`BTreeMap<price, VecDeque<Order>>` plus a sequence-keyed index so cancels stay O(log n)) would
  make full-level sweeps O(1) per order. Measured headroom and current numbers: `cargo bench
  --bench matchengine`. The extended `Order` (time in force, stop, iceberg fields) also grew the
  map value from 40 to ~88 bytes, which shows up as roughly 1.5–1.7x on the copy-heavy benches; a
  compact resting record or per-level buckets would recover it.
- [ ] `impl PartialEq for Order` ignores `id`, so two different orders at the same price/size compare equal.
- [ ] Bound `client_order_id` length (and decide the accepted character set) before exposing `/write`
  to untrusted clients; today only the HTTP payload limit bounds a single key.
- [ ] `Order::sequence` is required by the public deserializer even though `place_order()` overwrites
  it; make it optional (serde default) so callers do not have to send a value the engine ignores.
- [ ] Payloads on the public HTTP API are JSON; consider protobuf/Avro there too. Raft RPCs already use `postcard` over gRPC.

## 12. Market data

Design: `docs/AERON_SOLUTION.md` Part II; implementation plan and progress:
`docs/MARKET_DATA_PLAN.md`.

- [x] Deterministic feed core: depth journal in `OrderBook`, `MatchEvent.side`, leader-stamped
  intake timestamps, shadow book, deterministic processor (one TRADE per taker, BBO, depth
  snapshots/deltas, lifecycle, OHLC bars, status cadence, gap), leader gating with reset
  snapshots, JSONL sink, binary wire encoder v2 and `marketdata_status` read.
- [ ] Aeron adapter: implement `MarketDataSink` with `rusteron-client` + an embedded driver in
  tests, feature-gated (`market-data-aeron`); see AERON_SOLUTION §20/§23.
- [ ] Archive recording/replay and the snapshot-request stream (2003/2010).
- [ ] Multi-symbol engine: `symbol_id` on orders and per-symbol books; the feed schema already
  carries the field and the shadow/bars are keyed by it.
- [x] Deterministic market-data sequencing (`market_seq`, `sub_seq`): every mutation consumes the
  replicated sequence, boundaries are entry-driven, one TRADE per taker (uncapped), status every
  `status_every`, wire v2. Keys are identical across replicas, failover and restart. Design:
  AERON §16.1; plan U8.
- [ ] Wire `epoch` (leader generation) and `leader_id` instead of the current zeros; the header
  fields exist and the leader watcher already knows the term.
- [ ] Consumer SDK/reference decoder for the wire format (the encoder is covered by golden tests).
- [ ] Load test at the 50k-500k fills/s target and record feed latency/backpressure numbers.


## 10. Transport

gRPC (`tonic`) is implemented and is the default for peer-to-peer Raft RPCs; HTTP/JSON remains
selectable with `--transport http`. Aeron was investigated (`docs/AERON_SOLUTION.md`) and is still not
adopted: it stays gated on a measured bottleneck that has not been demonstrated.

Done:

- [x] Binary payload codec (`postcard`) inside protobuf envelopes; JSON stays on the public HTTP API.
- [x] Bidirectional `stream_append` (`AppendStream`) so replication pipelines instead of alternating send/receive.
- [x] Snapshot transfer as 256 KiB chunks with offsets, a terminal marker and cancellation; covered by `tests/cluster/test_snapshot.rs`.
- [x] Separate replication / heartbeat / snapshot channels per peer, so a snapshot cannot block a vote.

Remaining:

- [ ] Build the benchmark harness described in `docs/TRANSPORTS.md` §5 (L1 RPC microbenchmark + L2 3-node commit latency) and record the HTTP/JSON vs gRPC comparison. All current numbers in `docs/TRANSPORTS.md` are guidance, not results.
- [ ] Decide on TLS/mTLS for the gRPC listener before the order flow crosses a trust boundary; tonic supports it but nothing is configured.
- [ ] Implement `pre_vote` and `transfer_leader` RPCs if openraft enables those paths; both currently use trait defaults.
- [ ] Consider streaming snapshot bytes into the state machine instead of buffering the whole snapshot in memory; the current in-memory state machine requires a contiguous buffer anyway. The gRPC receiver also accepts an unbounded chunk total, so a hostile peer can force allocation up to its own message cap.
- [ ] A gRPC server task that dies is only logged (`src/lib.rs`); the HTTP API keeps serving while peers can no longer reach this node. Decide whether that should stop the process.

## 11. Client library and benchmarks

- [ ] Harden `src/client.rs` before it is used outside the examples: retry/backoff on transient RPC errors, configurable timeouts, and a typed error for "no known leader" instead of a bare message.
- [x] Order-book micro-benchmarks: `cargo bench --bench matchengine` (sweep wide/deep, rest, rest
  with the market-data journal, partial fill, IOC cancel, iceberg refill, amend, cancel; best-of-5
  per round, no framework).
- [ ] Add end-to-end latency benchmarks (submit → commit → apply → response) next to the transport harness in §10, so matching and replication costs can be separated.
