# TODO

Current snapshot of known gaps. Priorities assume the goal is to finish the openraft 0.10 port and restore the features the README advertises. See `AGENTS.md` for day-to-day commands and branch state.

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

- [ ] Replace `.decompose().unwrap()` in `src/network/api.rs` and `src/network/management.rs` with real error handling. Today a request that must be forwarded to the leader panics the worker instead of returning the forward hint.
- [ ] `src/network/api.rs:96` — `linearizer.await_ready(...).unwrap()` can panic; return the error to the caller.
- [x] Scripts now send the correct `sequence` field.
- [ ] `Order::sequence` is still required by the deserializer even though `place_order()` overwrites
  it; make it optional (`#[serde(default)]`) so clients need not send a value the engine ignores.
- [ ] Collapse the three duplicated `orderbook_sequance` compatibility arms in
  `src/network/api.rs` into one helper used by `/read`, `/linearizable_read` and `/follower_read`.
- [ ] `src/lib.rs` registers two `Logger` middlewares — every request is logged twice. Keep one.

## 4. Dependency cleanup

- [ ] Unused: `uuid`, `thiserror`, `async-trait`, `rand` (no references in `src/` or `tests/`).
- [x] `walkdir` dropped with the deleted `src/store/store.rs`. `sled` is only used by the playground binary `src/bin/sled_test.rs`.
- [ ] Stale version constraints vs. what `Cargo.lock` actually resolves: `actix-web = "4.0.0-rc.2"` (resolved 4.13), `uuid = 0.8.1`, `env_logger = 0.9.0`.

## 5. Tests

Unit tests cover the order book, the Raft-address derivation and the snapshot file format.
Integration coverage is `tests/cluster/test_cluster.rs` (3 nodes, both transports),
`tests/cluster/test_snapshot.rs` (snapshot build, purge, learner install over gRPC),
`tests/cluster/test_storage.rs` (WAL framing, torn-tail truncation, purge/truncate across segments,
vote/committed persistence, snapshot build/install/reopen) and `tests/cluster/test_restart.rs`
(full node restart via log replay and via snapshot).

- [ ] Add unit tests for the order book: partial fills, cancel of an already-filled order, price-time priority (`BidKey`/`AskKey` ordering).
- [ ] Add negative tests: write to a follower, unknown-key read, malformed `Place` payload.
- [ ] `tests/cluster/test_cluster.rs` binds fixed ports 21001-21003 and leaves 3 nodes running after the test finishes; parameterize ports and shut the nodes down.
- [x] Test restart recovery (both log replay and snapshot boot paths).

## 6. Docs

- [x] `README.md` API list corrected to the real routes: `/vote`, `/append`, `/snapshot`,
  `/get_linearizer`, `/linearizable_read`, `/follower_read`, `/read`.
- [x] `README.md` snapshot section now describes `default_raft_config()` and the test-only
  `LogsSinceLast(10)` / `max_in_snapshot_log_to_keep = 0` override.
- [x] `README.md` prerequisites updated to Rust 1.85 (edition 2024) and note that protoc is vendored.
- [x] README now describes the segmented WAL + snapshot storage design instead of Sled.
- [x] `README.md` / `README_CN.md` project structure, transport flags, test suite and future work
  synced with `src/network/grpc/` and `tests/cluster/`.
- [x] `GUIDE_cn.md` rewritten for the current design (WAL + snapshots + gRPC); the
  `ExampleStore`/combined `RaftStorage`/sled description is gone.
- [x] `docs/AERON_SOLUTION.md` and `docs/TRANSPORTS.md` updated: gRPC is adopted, storage is
  durable, and all remaining performance numbers are labelled as guidance.
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

- [ ] No CI exists. Add GitHub Actions running `cargo build`, `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.
- [ ] `Cargo.lock` is in `.gitignore` but the crate produces a binary — usually committed for binaries. Decide one way.
- [ ] `src/lib.rs` starts with `#![allow(clippy::uninlined_format_args)]`; fix the lints and drop the allow.

## 9. Matching engine (from README "Future Work" + found in code)

- [ ] `OrderType::Limit` is hardcoded in `Order::new` (`// TODO: More types` at `src/matchengine/mod.rs:74`); no market orders.
- [ ] `price`/`volume` are `f64` — money should use integer ticks/decimal.
- [ ] `impl PartialEq for Order` ignores `id`, so two different orders at the same price/size compare equal.
- [ ] `Order::sequence` is required by the public deserializer even though `place_order()` overwrites
  it; make it optional (serde default) so callers do not have to send a value the engine ignores.
- [ ] Payloads on the public HTTP API are JSON; consider protobuf/Avro there too. Raft RPCs already use `postcard` over gRPC.

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
