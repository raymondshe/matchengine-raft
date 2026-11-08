# OpenRaft utilization review

Date: 2026-11-06
Scope: OpenRaft storage and transport usage, plus adjacent OpenRaft 0.10 APIs that are
currently unused.
Version reviewed: `openraft 0.10.0-alpha.37`.

This document records the review and its priorities. The P1 quick wins were implemented on
2026-11-06; the remaining follow-ups are tracked in [`TODO.md`](../TODO.md) §16.

## Executive summary

The transport is already on OpenRaft's modern replication path:

- `RaftNetworkV2::stream_append` is implemented on both sides and is used by OpenRaft's
  replication and heartbeat workers.
- `full_snapshot` is a custom, cancellable gRPC stream with chunk validation.
- Pre-Vote is enabled and backed by a dedicated RPC.
- Replication, heartbeat and snapshot clients each get their own channel.

The storage implementation is also solid: a segmented WAL with one fsync per append batch,
atomic snapshots, torn-tail recovery, and snapshot building outside the state-machine lock.

The main opportunities are therefore not "replace `stream_append`". They are:

1. aligning OpenRaft configuration defaults with this workload;
2. reducing snapshot copying and snapshot-transfer overhead;
3. evaluating a non-blocking WAL append path;
4. tuning HTTP/2 flow control and heartbeat behavior;
5. exposing the OpenRaft metrics and runtime controls the project does not yet use.

## Implementation status (2026-11-06)

The P1 quick wins are implemented:

- `default_raft_config` now sets `replication_lag_threshold = 150_000` (above the 100k
  `LogsSinceLast` policy) and `heartbeat_min_interval = Some(500)`.
- The gRPC client and server set HTTP/2 stream/connection windows and keepalive, and the
  server now has keepalive enabled.
- `full_snapshot` uses `RPCOption::snapshot_chunk_size()` (capped at 8 MiB) instead of the
  fixed 256 KiB constant.
- `/ready` and `/health` expose `recovered`; readiness requires
  `Raft::wait_for_recovery` to confirm the state machine applied the cluster commit covering
  the durable log tail. This covers the non-fsynced `save_committed` marker without adding a
  second fsync per commit.

The P2/P3 items below remain open.

## Method

The review covered:

- `src/lib.rs` (Raft configuration and node composition);
- `src/store/log_store.rs` (WAL and `RaftLogStorage`);
- `src/store/state_machine.rs` (state machine and `RaftSnapshotBuilder`);
- `src/network/grpc/{client,server,mod}.rs` (transport);
- `src/network/{api,management,health}.rs` (reads, metrics and readiness);
- the `openraft 0.10.0-alpha.37` sources for replication, heartbeat, snapshot transfer and
  configuration defaults.

The transport claim was also checked at runtime: a 3-node `test_cluster` run with
`RUST_LOG=matchengine_raft=debug` repeatedly logged
`gRPC AppendEntries stream opened`, confirming that `stream_append` is the active path.

## Current usage that should be kept

- **Bidirectional pipelined replication.** `src/network/grpc/client.rs` implements
  `stream_append` over `AppendStream`; `src/network/grpc/server.rs` feeds the request stream
  into `Raft::stream_append`. OpenRaft calls this from
  `replication/mod.rs` and `core/heartbeat/worker.rs`.
- **Streamed snapshots.** `full_snapshot` streams fixed-size chunks and honors the
  replication cancellation future; the receiver validates offsets and the total size before
  installing.
- **Separate role channels.** `GrpcNetworkFactory::new_client` returns a fresh
  `GrpcNetwork`, so replication, heartbeat and snapshot clients do not share an HTTP/2
  connection. This is desirable: a snapshot cannot head-of-line-block a heartbeat.
- **Pre-Vote.** `default_raft_config` sets `enable_pre_vote: Some(true)` and the transport
  implements the dedicated `PreVote` RPC.
- **Snapshot build outside the lock.** `build_snapshot` captures a consistent image under
  the lock, then encodes and writes it on the blocking pool.
- **One fsync per append batch.** `LogStore::append` writes and syncs the batch before
  invoking `IOFlushed`.
- **Existing observability.** `/metrics`, `/health`, `/ready`, snapshot trigger, election
  and leadership transfer are already wired.

## Storage findings

### S1. `replication_lag_threshold` does not match the snapshot policy

**Current:** `default_raft_config` (`src/lib.rs:198`) sets
`snapshot_policy = LogsSinceLast(100_000)` but leaves OpenRaft's default
`replication_lag_threshold` at **5,000**.

**Why it matters:** OpenRaft's configuration docs state that
`replication_lag_threshold` should be greater than the snapshot policy threshold,
otherwise sending a snapshot may not actually fix the lagging follower. A follower only
6,000 entries behind can trigger a snapshot transfer even though the leader still has a
large retained log window.

**Recommendation:** set the threshold above `LogsSinceLast`, for example 120,000–150,000,
and verify follower catch-up with the existing late-join/snapshot tests. This is a
configuration-only change.

**Effort / impact:** small / high. Avoids unnecessary snapshot churn and the latency spike
that comes with snapshot installation.

### S2. WAL append blocks RaftCore on fsync

**Current:** `RaftLogStorage::append` (`src/store/log_store.rs:803`) calls
`append_entries`, which awaits `append_blocking` and `sync_data` before returning.
`IOFlushed` is only completed after the fsync.

**Why it matters:** OpenRaft's `RaftLogStorage::append` contract allows returning after
the entries are readable in memory and calling `IOFlushed` later, once they are durable.
Blocking RaftCore on the fsync prevents it from processing and batching more writes while
the disk flushes, limiting the benefit of OpenRaft's `max_append_entries` and
`api_batch_*` batching controls.

**Recommendation:** evaluate a background WAL writer:

- append entries to an in-memory pending buffer and update the index;
- return immediately so RaftCore can continue;
- write and fsync on a dedicated blocking task;
- call `IOFlushed` when the batch is durable;
- serve `try_get_log_entries` from the pending buffer until the on-disk copy is visible.

This is the highest-effort storage change because reads must remain correct immediately
after `append` returns. It should be designed with the same recovery/fault-injection tests
as the current WAL.

**Effort / impact:** large / high. Potentially higher write throughput and lower tail
latency under concurrent writes.

### S3. Snapshot data is copied on read and build

**Current:**

- `StateMachineStore::get_current_snapshot` clones `snapshot.data` while holding the
  state-machine lock (`src/store/state_machine.rs:1427`).
- `build_snapshot` stores `data.clone()` in `current_snapshot` and returns
  `Cursor::new(data)` (`src/store/state_machine.rs:1005`).

**Why it matters:** a large order book plus the retained 262k-event window produces a
large snapshot. Every lagging follower triggers a full `Vec<u8>` copy, and the copy holds
the state-machine lock, delaying applies.

**Recommendation:** change `SnapshotData` from `Cursor<Vec<u8>>` to a cheap-to-clone
representation such as `Cursor<Arc<[u8]>>` (or a small `Bytes`-backed reader). Then
`get_current_snapshot` is an `Arc` clone and `build_snapshot` does not duplicate the
encoded body.

**Effort / impact:** medium / medium-high. Less memory, shorter lock holds, cheaper
snapshot fan-out.

### S4. WAL append size is bounded by entry count, not bytes

**Current:** OpenRaft's `max_append_entries` defaults to **4,096**. A
`MatchEngineRequest::Batch` can contain up to 128 orders, so one merged append can encode
a very large buffer. `append_blocking` builds the entire encoded batch in memory before
writing it.

**Why it matters:** a large merged append can create a memory/IO spike and can exceed a
64 MiB WAL segment or the 64 MiB gRPC message limit.

**Recommendation:** either lower `max_append_entries` (for example 512) and measure
throughput, or add a byte-size cap in the WAL append path. A byte cap could split one
logical append into multiple writes while still using one final fsync.

**Effort / impact:** medium / medium. Better bounded memory and more predictable latency.

### S5. `save_committed` is not durable until the next append

**Current:** `save_committed_blocking` writes a commit record but deliberately does not
fsync it; the next append makes it durable.

**Why it matters:** OpenRaft's trait documentation warns that if `committed` is not saved
durably, a restarted node may serve a read from a state older than one already observed
before the restart, until it perceives a newer cluster commit. This is a consistency
tradeoff, not just a performance choice.

**Recommendation:** either fsync the commit marker, or keep the optimization and gate
readiness/reads on `Raft::wait_for_recovery(...)` after restart. The current `/ready`
handler only requires a known leader (`src/network/health.rs`), so it does not cover this
case.

**Effort / impact:** small / high for consistency. Decide explicitly and document the
choice.

### S6. Purge tuning is coupled to the snapshot policy

**Current:** OpenRaft defaults are `max_in_snapshot_log_to_keep = 1,000` and
`purge_batch_size = 1`, while snapshots are taken every 100,000 logs.

**Why it matters:** with the current 100k policy, purge happens in large jumps and the
default batch size is probably harmless. If the snapshot policy is lowered or exposed as a
CLI option, frequent small purges would rewrite the `state` file and fsync the directory
more often than necessary.

**Recommendation:** when exposing the snapshot policy, expose `purge_batch_size` too and
choose it relative to the expected snapshot interval.

**Effort / impact:** small / low-medium.

## Transport findings

### T1. Heartbeat suppression is disabled

**Current:** `default_raft_config` leaves `heartbeat_min_interval` at `0`, so every
heartbeat tick opens a new `AppendStream` to every follower.

**Why it matters:** OpenRaft 0.10 can suppress a periodic heartbeat when a replication or
heartbeat response was received within `heartbeat_min_interval`. Under sustained writes,
replication already proves liveness; the extra heartbeat stream is redundant. Suppression
resumes automatically when replication idles.

**Recommendation:** set `heartbeat_min_interval` to roughly 500 ms. With the current
`heartbeat_interval = 500` and `election_timeout_min = 1500`, the maximum gap is
500 + 500 + ~101 ms tick, safely below 1500 ms.

**Effort / impact:** small / high. Fewer streams, fewer RPCs, less wakeup pressure.

### T2. HTTP/2 flow-control windows are left at defaults

**Current:** `make_channel` (`src/network/grpc/client.rs`) sets `tcp_nodelay` and keepalive
but not `initial_stream_window_size` or `initial_connection_window_size`. The tonic
server builder in `src/lib.rs` also leaves the defaults.

**Why it matters:** replication payloads and snapshot chunks can be much larger than the
default HTTP/2 window. Small windows force frequent window updates and can cap throughput
on higher-latency links.

**Recommendation:** benchmark larger windows, for example 1–4 MiB stream and 4–8 MiB
connection windows for small clusters. Apply the largest windows only to the dedicated
snapshot client if connection memory matters. Also set server-side HTTP/2 keepalive so
half-open peer connections are detected.

**Effort / impact:** small-medium / medium-high for snapshot and catch-up throughput.

### T3. `RPCOption::snapshot_chunk_size` is ignored

**Current:** `full_snapshot` hardcodes `SNAPSHOT_CHUNK_SIZE = 256 KiB`
(`src/network/grpc/mod.rs`). OpenRaft passes `snapshot_max_chunk_size` (default 3 MiB) in
`RPCOption::snapshot_chunk_size()`.

**Why it matters:** the configured value and the actual transport behavior disagree. A
100 MB snapshot uses roughly 400 messages at 256 KiB versus roughly 34 at 3 MiB.

**Recommendation:** use `option.snapshot_chunk_size()` and clamp it to a safe maximum, or
set `snapshot_max_chunk_size` to 256 KiB so the configuration matches the transport. Test
cancellation latency with larger chunks.

`install_snapshot_timeout` is also still at OpenRaft's 200 ms default. The current
`full_snapshot` intentionally ignores `soft_ttl` and is bounded by the cancellation future and
HTTP/2 keepalive, so this does not fail today. If a future transport starts honoring the
timeout, raise it to a realistic snapshot-transfer budget (for example 30–60 s) or document
that the transport deliberately opts out.

**Effort / impact:** small / medium.

### T4. Role-specific network clients are implicit

**Current:** `GrpcNetworkFactory` only implements `new_client`. OpenRaft's default
`new_heartbeat_client` and `new_snapshot_client` delegate to it, which happens to create a
new `GrpcNetwork` and channel for each role.

**Why it matters:** the isolation is real but accidental. If connection pooling or a shared
channel is added later, the defaults could silently put snapshots and heartbeats on the
replication path.

**Recommendation:** override `new_heartbeat_client` and `new_snapshot_client` explicitly
and give each role its own channel profile (small windows for heartbeats, large windows for
snapshots). Keep `connect_lazy` in the factory and optionally warm up connections in the
background after membership changes.

**Effort / impact:** small-medium / medium. Makes an important isolation property explicit
and enables role-specific tuning.

### T5. Payload and in-flight limits are OpenRaft defaults

**Current:** `max_payload_entries = 300`, `STREAM_APPEND_CHANNEL_SIZE = 64`,
`api_batch_linger_ms = 0`. These values were not chosen for this workload.

**Why it matters:** the client stream channel is bounded by count, not bytes. With large
append payloads, 64 queued requests can consume significant memory. Conversely, a very
small channel can limit replication throughput.

**Recommendation:** benchmark combinations of `max_payload_entries` (for example 128 vs
300 vs 1,024), `STREAM_APPEND_CHANNEL_SIZE` (16/32/64) and `api_batch_linger_ms`
(0/1/2 ms). Keep latency-sensitive defaults at 0 linger unless measurements justify the
tradeoff.

**Effort / impact:** small / medium. Requires benchmarks, not new architecture.

### T6. Optional transport improvements

Lower-priority items:

- **Preconnect peers** after startup/membership changes to remove first-RPC TCP/HTTP2
  setup latency.
- **Server HTTP/2 keepalive** to detect dead peers and reclaim stale streams.
- **Larger HTTP/2 max frame size** (for example 64 KiB) for large append/snapshot
  messages; both endpoints are ours.
- **gRPC compression** for snapshot transfer if network bandwidth, not CPU, is the
  bottleneck.
- **Persistent `AppendStream` sessions** across OpenRaft replication sessions are not
  possible without changing the OpenRaft contract; heartbeat suppression is the practical
  way to reduce stream churn.

## OpenRaft APIs that are not yet used

These are adjacent to storage/transport and improve operability:

- **`Raft::data_metrics()` and `Raft::server_metrics()`** — expose replication progress,
  lag and leader-contact information alongside `RaftMetrics` in `/metrics`.
- **`runtime-stats` feature / `MetricsRecorder`** — optional histograms for append batches
  and replication latency; useful in benchmarks and production dashboards.
- **`Raft::wait_for_recovery()`** — gate `/ready` or startup on recovery, especially if
  `save_committed` remains non-fsynced.
- **`Raft::runtime_config()`** — expose supported timing/snapshot knobs at runtime through
  the console.
- **`Raft::trigger().purge_log()`** — add a guarded console action for explicit log purge
  after a snapshot.
- **`ReadPolicy::LeaseRead`** — lower-latency linearizable reads; requires bounded clock drift.
  It is now the default with a `ReadIndex` fallback; `--read-policy read_index` forces the
  conservative path.
- **`on_leader_change` callbacks** — replace metrics polling in the market-data leader
  watcher, a minor cleanup.
- **`client_write_many`** — not recommended for `/write_batch`: the current
  `MatchEngineRequest::Batch` is one Raft entry, one WAL record and one apply, which is
  more efficient than multiple entries in one API message.

## Priority summary

| Priority | Item | Area | Effort | Expected effect |
|---|---|---|---|---|
| P1 | Align `replication_lag_threshold` with `LogsSinceLast(100_000)` | Storage/config | S | Avoid snapshot churn and stale snapshot transfers |
| P1 | Enable `heartbeat_min_interval` (~500 ms) | Transport/config | S | Fewer heartbeat streams and RPCs |
| P1 | HTTP/2 windows + server keepalive | Transport | S/M | Better snapshot and replication throughput |
| P1 | Honor `RPCOption::snapshot_chunk_size` | Transport | S | Fewer snapshot messages |
| P1 | Review `save_committed` durability and recovery readiness | Storage/consistency | S | Avoid reverted reads after restart |
| P2 | Snapshot `Arc` data | Storage | M | Less copying and shorter lock holds |
| P2 | WAL append byte cap / payload tuning | Storage/transport | M | Bounded memory and predictable latency |
| P2 | Role-specific network clients + preconnect | Transport | S/M | Explicit isolation and faster first RPC |
| P2 | Expose OpenRaft data/server metrics | Observability | S/M | Better lag and replication visibility |
| P3 | Async WAL append | Storage | L | Higher write throughput and better batching |
| P3 | Runtime config, purge action, `LeaseRead` | OpenRaft APIs | M | Operational flexibility and read latency |
| P3 | Snapshot compression, larger frames | Transport | M | Optional bandwidth/CPU tradeoff |

The P1 rows are implemented as of 2026-11-06; see [Implementation status](#implementation-status-2026-11-06).

## Suggested implementation order

### Phase 1 — configuration and transport quick wins (low risk)

1. Set `replication_lag_threshold` above the snapshot threshold.
2. Set `heartbeat_min_interval = Some(500)`.
3. Add server HTTP/2 keepalive and benchmark larger client/server windows.
4. Use `RPCOption::snapshot_chunk_size()` or align the config to 256 KiB.
5. Add tests/benchmarks:
   - existing `test_cluster`, `test_snapshot`, `test_late_join`;
   - `cargo bench --bench storage`;
   - `cargo bench --bench e2e`;
   - a debug-log check that heartbeat stream count drops under sustained writes.

### Phase 2 — storage memory and snapshot transfer

1. Move snapshot data to `Arc<[u8]>` / `Cursor<Arc<[u8]>>`.
2. Add a WAL append byte cap or tune `max_append_entries`.
3. Add a large-snapshot transfer test that asserts memory/copy behavior and cancellation.

### Phase 3 — async WAL append

Design the pending-entry read cache and background writer first. Keep the existing WAL
format and fault-injection tests. This phase should not start until Phases 1–2 are measured.

### Phase 4 — observability and read policy

Add OpenRaft data/server metrics, optional runtime config and purge actions, and evaluate
`LeaseRead` as an opt-in read policy.

## Verification notes

Any change in this area should be measured with the existing discipline:

- `cargo test` (including the cluster integration target);
- `cargo bench --bench storage` for WAL append/read/reopen;
- `cargo bench --bench e2e` for write latency and throughput;
- `test_snapshot` and `test_late_join` for snapshot and learner catch-up;
- a 3-node run with `RUST_LOG=matchengine_raft=debug` to inspect
  `gRPC AppendEntries stream opened` counts before/after heartbeat suppression.

## Verification performed (2026-11-06)

The P1 quick wins were validated with:

- `cargo test` — 55 cluster/integration tests passed;
- `test_health`, `test_snapshot`, `test_late_join`, `test_cluster` — passed with the new
  readiness gate and heartbeat configuration;
- `cargo test --manifest-path demos/market_data_system/Cargo.toml` — 6 WebSocket tests passed;
- `cargo clippy --all-targets -- -D warnings` — clean;
- `cargo fmt --all -- --check` — clean.

The post-change end-to-end and storage medians are recorded in
[`PERFORMANCE.md`](./PERFORMANCE.md) (2026-11-06): sequential `/write` 262 writes/s,
p50 3.96 ms, p99 5.17 ms, concurrent 17,245 writes/s; storage append
25,547 entries/s. A dedicated before/after heartbeat-stream-count and
snapshot-transfer-time measurement remains a P2 follow-up in
[`TODO.md`](../TODO.md) §16; the P2/P3 storage and observability items remain open.
