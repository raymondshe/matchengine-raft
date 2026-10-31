# Performance measurements

Measured on **2026-10-30** (runtime flavor A/B and P0 offloads: **2026-10-31**) on the reference development machine:

- Apple M4 (`arm64`)
- Rust `1.93.1`
- Release/bench profile
- Local loopback networking, single process for the end-to-end test

These are engineering baselines, not a capacity guarantee. Run the commands below
on the deployment hardware before using the numbers for sizing.

## Reproduce

```shell
cargo bench --bench matchengine   # order-book microbenchmarks
cargo bench --bench e2e           # submit -> commit -> apply -> response (multi-thread; -- current|both for A/B)
cargo bench --bench feed_load     # market-data feed at 50k/500k fills/s
cargo bench --bench storage       # WAL append/read/recovery
cargo bench --bench p0_bench -- duplicate|read|snapshot  # P0 lock/scan A/B
```

## Order-book microbenchmarks

Best-of-5 per round; each scenario performs the operation 1,000 times unless
noted. These exclude HTTP, Raft, WAL and the feed.

| Scenario | Time per round |
|---|---:|
| Sweep 1k asks / 100 levels | 105.6 µs |
| Sweep 20k asks / 1 level | 2.538 ms |
| Rest 1k bids | 97.7 µs |
| Rest 1k bids + market-data journal | 100.3 µs |
| Partial fills 1k | 119.2 µs |
| IOC cancels 1k | 201.0 µs |
| Iceberg replenish 1k | 116.8 µs |
| Amend 1k | 130.8 µs |
| Cancel 1k | 148.1 µs |

The market-data journal adds about 2.6 µs to 1,000 rests, confirming that the
matching path does not pay for shadow-book derivation, encoding or sink I/O.

## End-to-end write path

Single node, HTTP JSON `/write`, including:

1. HTTP request decode and request validation
2. Raft proposal and commit
3. WAL append and `sync_data`
4. State-machine apply and order-book mutation
5. Response serialization and HTTP response

It excludes cross-node network replication, so a 3-node cluster adds one
majority round trip on top of these numbers.

| Measurement | Result |
|---|---:|
| Sequential writes | 500 |
| Sequential throughput | 253 writes/s |
| Mean latency | 3.96 ms |
| p50 latency | 3.99 ms |
| p95 latency | 4.90 ms |
| p99 latency | 5.17 ms |
| Max latency | 5.36 ms |
| Concurrent writes (1,000 in flight) | 18,350 writes/s |

The gap between sequential and concurrent throughput is the expected batching
effect: sequential clients pay one WAL fsync per request, while concurrent
requests share append batches. Matching itself is measured separately above;
the single-node e2e path is dominated by HTTP plus durable append.

### Batch writes

`POST /write_batch` puts several requests in one Raft entry, so one WAL append
and `sync_data` covers the whole batch. Measured on the same machine with 200
sequential batches per size (orders never cross, so this measures commit/apply,
not matching):

| Batch size | Batches | Orders | Batches/s | Orders/s | Speedup vs size 1 | Batch mean | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 200 | 200 | 236 | 236 | 1.00x | 4.23 ms | 4.17 ms | 5.41 ms |
| 8 | 200 | 1,600 | 229 | 1,829 | 7.73x | 4.37 ms | 4.22 ms | 5.73 ms |
| 32 | 200 | 6,400 | 230 | 7,345 | 31.06x | 4.35 ms | 4.28 ms | 5.34 ms |
| 64 | 200 | 12,800 | 218 | 13,929 | 58.90x | 4.59 ms | 4.72 ms | 5.50 ms |
| 128 | 200 | 25,600 | 197 | 25,183 | 106.48x | 5.08 ms | 5.05 ms | 6.04 ms |

The same run measured 257 single `/write` requests/s. The size-1 `/write_batch`
path (236 orders/s) pays only the array envelope overhead; by size 128 the
per-order cost falls by roughly two orders of magnitude because the fsync is
amortized. Batch latency grows only from 4.23 ms to 5.08 ms as the payload and
apply work grow, so throughput scales close to the batch size.

With 100 concurrent 32-order batches (3,200 orders in flight), the same node
reached **157,035 orders/s** (4,907 batches/s), showing that concurrent batches
also coalesce into Raft append batches. These are single-node loopback numbers;
a 3-node cluster adds one majority round trip per batch, but the WAL fsync
amortization is unchanged. Keep batches within the 128-request cap: `apply`
holds the state-machine lock for the whole batch.

### Runtime flavor A/B (2026-10-31)

The node binary now uses a multi-threaded Tokio runtime; the previous
`#[actix_web::main]` setup used actix-rt's current-thread runtime. To measure the
runtime flavor alone, `cargo bench --bench e2e` was run with `-- current` and
`-- multi` in alternating fresh processes (four runs each, medians). The HTTP
workers are separate threads in both cases; only the runtime hosting RaftCore,
apply, gRPC, the snapshot timer and the feed changes.

| Workload | Current-thread | Multi-thread | Delta |
|---|---:|---:|---:|
| Sequential `/write` | 257 writes/s | 254 writes/s | ~0% |
| Concurrent `/write` (1,000 in flight) | 16,783 writes/s | 16,688 writes/s | ~0% |
| `/write_batch`, 8 orders | 1,912 orders/s | 1,940 orders/s | +1.5% |
| `/write_batch`, 32 orders | 7,622 orders/s | 7,450 orders/s | -2.3% |
| `/write_batch`, 64 orders | 14,426 orders/s | 14,443 orders/s | ~0% |
| `/write_batch`, 128 orders | 27,632 orders/s | 27,324 orders/s | -1.1% |
| Concurrent batches (100 x 32) | 147,356 orders/s | 152,368 orders/s | +3.4% (high variance) |

The current-thread case uses a plain current-thread Tokio runtime; actix-rt's
system runtime has the same scheduler shape. The benchmark driver shares the
runtime with the node, so tail numbers include driver scheduling as well as the
server.

The medians are within run-to-run noise. These workloads are dominated by WAL
`sync_data` and the serialized state-machine apply path, not by background-task
scheduling, so the runtime switch alone does not improve them. It remains the
enabler for isolating gRPC, snapshot and feed work; the next gains require the
snapshot, lock-scope and sink/read offloads below.

### P0 optimization A/B (2026-10-31)

`cargo bench --bench p0_bench -- duplicate|read|snapshot` compares the P0
state-machine changes with the last pre-P0 commit. Both runs use the same
multi-threaded benchmark runtime, so the runtime flavor is not a variable.

| Workload | Before P0 | After P0 | Change |
|---|---:|---:|---:|
| Duplicate retry, 262k-event window, mean | 781 µs | 0.32 µs | ~2,400x faster |
| Duplicate retry, p99 | 1.10 ms | 0.42 µs | ~2,600x faster |
| Large read under writes, write p99 | 12.0 ms | 9.5 ms | -21% |
| Large read under writes, write max | 1.12 s | 20 ms | -98% |
| Large `orderbook_orders` read p99 | 174 ms | 134 ms | -23% |
| Snapshot build+write, 100k orders | 13.9 s | 15.5 s | no gain in this setup |
| Writes during snapshot, p99 | 8.6 ms | 9.1 ms | no gain in this setup |

The duplicate-retry result is the direct effect of replacing the linear event
window scan with a `(log_index, ordinal)` binary search. The large-read case
shows the lock release: the old path serialized the whole book while holding the
state-machine lock, producing the 1.1 s write outlier; the new path clones the
orders under the lock and serializes after releasing it.

The snapshot phase did not improve because its cost is dominated by the
snapshot encode plus fsync, and the multi-threaded runtime already has spare
workers for the writers. The offload still keeps encode/decode off the runtime
worker, but the total snapshot duration is unchanged; snapshot byte sharing and
the `to_content` deep copy remain the P2 opportunities.

## Market-data feed load

Real tokio tap, bounded queue and processor; null sink, so this measures the
feed pipeline without JSONL or network I/O. Each frame carries **10 fills**.

| Target | Offered | Processed | Dropped frames | GAPs | Peak queue | Publish / drain |
|---|---:|---:|---:|---:|---:|---:|
| 50k fills/s, queue 8,192 | 49,480 fills/s | 49,480 fills/s | 0 | 0 | 67 / 8,192 | 1.002 s / 208 ns |
| 500k fills/s, queue 8,192 | 495,072 fills/s | 495,072 fills/s | 0 | 0 | 169 / 8,192 | 1.002 s / 250 ns |
| 2M fills/s overload, queue 128 | 1,978,685 fills/s | 1,876,238 fills/s | 10,261 | 7,956 | 129 / 128 | 1.002 s / 292 ns |

At 500k fills/s the 8,192-frame queue stays far below capacity and the processor
keeps up. The deliberate 2M fills/s overload shows the bounded-queue contract:
frames are dropped rather than blocking `apply`, and the processor emits `GAP`
markers. Peak queue includes one frame handed to the processor but not yet
ingested, so it can show `capacity + 1`.

## Storage benchmark

`cargo bench --bench storage` writes 20,000 real `MatchEngineRequest::place`
entries in batches of 100, with 512 KiB segments:

| Operation | Result |
|---|---:|
| Append 20k entries | 767.8 ms |
| Append throughput | 26,049 entries/s |
| Read 20k entries | 8.38 ms |
| Read throughput | 2,386,089 entries/s |
| Reopen/recover 20k entries | 6.33 ms |
| Recovered entries | 20,000 |

Append throughput is fsync-bound (one durable batch per 100 entries); read and
recovery benefit from sequential segment scanning and coalesced `pread` calls.

## Storage-path improvements

The 2026-10-30 audit removed several avoidable copies and stalls from the storage path:

- **WAL append** serializes a borrowed `WalRecordRef` instead of cloning every entry into an owned
  `WalRecord`; the on-disk format is unchanged.
- **WAL recovery** scans each segment sequentially instead of issuing two `pread` calls per record.
- **Log reads** coalesce contiguous records from one append batch into a single `pread`, while
  still handling commit-marker gaps.
- **Snapshot build** captures a consistent content image under the state-machine lock, then
  encodes and writes it after releasing the lock, so apply can continue during large snapshots.
- **Snapshot writes** stream the postcard body through an incremental CRC writer, so no full
  body `Vec` is allocated; snapshot install moves its buffer into the write task instead of
  cloning it.

The storage integration suite (`test_storage`) still covers framing, torn tails, rotation, purge,
truncate, snapshot install/reopen and 20k-record recovery.

## Thread and task audit: optimization opportunities

This audit looks at where work runs, not just how much work there is. The goal is
to keep the Raft core and the state-machine apply task responsive; matching
itself is already cheap.

### Runtime topology

- `src/main.rs` uses `#[tokio::main(flavor = "multi_thread")]`, so the node's
  background tasks run on a **multi-threaded Tokio runtime**.
- `build_match_engine_node` spawns the OpenRaft core and state-machine worker,
  the tonic gRPC server, the snapshot timer, the market-data watcher and
  processor, and the HTTP server future on that runtime.
- Actix HTTP handlers run on the HTTP worker threads (one current-thread runtime
  per worker), not on the main runtime. They reach the state machine through the
  async mutex and Raft through its command channel.
- WAL append/read/recovery and snapshot file writes already use
  `spawn_blocking`, so their disk I/O does not stall the main runtime.
- The integration tests and benchmarks already call `build_match_engine_node`
  from a multi-threaded Tokio runtime (openraft's test runtime uses 8 worker
  threads), and `actix-server` detects an existing Tokio runtime and starts its
  HTTP workers from it. The production binary now matches that topology.

### Tokio tasks vs OS threads

- A `tokio::spawn` task is a **green thread** scheduled by the runtime. It is not
  a dedicated OS thread, and creating more tasks does not create more CPU
  parallelism by itself.
- On a current-thread runtime, all tasks are cooperatively scheduled on one
  worker. Spawning a task only helps if it yields; a CPU loop or a blocking file
  write still blocks every other task on that runtime. The previous
  `#[actix_web::main]` setup had this shape.
- On a multi-threaded Tokio runtime, spawned tasks are distributed across worker
  threads and can run in parallel. This is the right way to offload independent
  services: RaftCore, apply, gRPC, the snapshot timer and the feed can be
  separate green threads.
- Use `spawn_blocking` only for work that cannot yield: blocking file I/O,
  synchronous `std::fs`, and CPU-heavy encode/decode. It is Tokio's blocking
  pool, not a manually managed OS thread; the runtime bounds and reuses those
  threads.
- Prefer async I/O (`tokio::fs`, `tokio::io::BufWriter`) inside a spawned task
  for sinks, so the task yields instead of holding a worker.

### Work on the state-machine path

| Work | Current location | Cost | Can be offloaded? |
|---|---|---|---|
| Price-time matching | `apply_one`, under the lock | CPU, short | No; keep it on apply |
| Event construction and event-window append | `apply_one`, under the lock | CPU, bounded | No; part of apply |
| Match-result sink (`--match-event-log`) | `publish_pending`, after the lock | JSON + file write + flush | Yes; a dedicated writer task is still open |
| Market-data frame construction | `take_market_frame`, under the lock | event clone + journal drain | Sent after the lock; gate checked first |
| Duplicate `client_order_id` response | `submission_events`, under the lock | binary search + range copy | Implemented |
| Snapshot capture (`to_content`) | snapshot task, under the lock | full book/event copy | Hard (COW) |
| Snapshot encode | `build_snapshot` / `persist_snapshot`, `spawn_blocking` | CPU, tens of MB | Implemented |
| Snapshot decode | `install_snapshot`, `spawn_blocking` | CPU, tens of MB | Implemented |
| Snapshot bytes copy | `build_snapshot` / `get_current_snapshot`, under the lock | full-body memcpy | Yes (`Arc<[u8]>`) |
| Read JSON (`/read orderbook_orders`) | `read_state_owned`, lock; `into_json` after | clone + JSON of the whole book | Implemented |
| Market-data processing (`--market-data-log`) | `MarketDataProcessor` task, runtime | shadow book + wire encode + JSONL flush | Yes, Tokio task + async/blocking sink |
| gRPC decode/encode and snapshot assembly | tonic tasks, runtime | message-size dependent | Yes, on the multi-thread runtime |
| WAL append/read | `LogStore`, `spawn_blocking` | disk I/O | Already offloaded |

### Prioritized opportunities

1. **Run the node on a multi-threaded Tokio runtime (P0, implemented).**
   `src/main.rs` now uses `#[tokio::main(flavor = "multi_thread")]` instead of
   `#[actix_web::main]`. `actix-server` detects the Tokio runtime and starts its
   HTTP workers from it; the integration tests already exercised this path.
   RaftCore, the state-machine worker, gRPC, the snapshot timer and the feed are
   now green threads distributed across workers instead of sharing one thread.
   The full test suite and a binary init/write/read/shutdown smoke test pass.
   The A/B above shows the switch is neutral for the fsync-bound write path; it
   is the enabler for the isolation/offload items below, not a standalone win.
   Items 2-7 are implemented; items 8-11 remain open.

2. **Move snapshot encode/decode to the blocking pool (P0, implemented).**
   `build_snapshot`, `persist_snapshot` and `install_snapshot` now encode/decode
   inside the same `spawn_blocking` closure that writes the file. The
   state-machine lock is still released before encoding, and a bad snapshot is
   decoded before the durable copy is touched. `spawn_blocking` is Tokio's
   blocking pool, not a new OS thread per call.

3. **Serialize reads outside the state-machine lock (P0, implemented).**
   `read_state_owned` clones orders/stops (and builds small scalar views) under
   the lock; `ReadState::into_json` serializes after the guard is dropped in
   `/read`, `/linearizable_read` and `/follower_read`. Large order-list JSON no
   longer holds the state machine while it runs.

4. **Publish match results after releasing the lock (P0, implemented).**
   `apply_one` collects `PendingPublish` (events plus an optional market frame);
   `apply` releases the lock and calls `publish_pending`, which invokes the sink
   and `try_send`s the tap frame. The sink still runs on the apply task, so a
   slow sink delays the next entry; moving it to a dedicated writer task remains
   a P1 follow-up.

5. **Lock per entry, not per Apply command (P0, implemented).**
   `apply` now acquires `inner` inside the loop, updates `last_applied_log` with
   that entry, releases it before publishing, and waits for the next entry
   without holding the lock. Reads and snapshot capture can interleave between
   entries, and the log reader is never called under the lock.

6. **Index duplicate retry events (P0, implemented).**
   `submission_events` uses `VecDeque::partition_point` on the ordered
   `(log_index, ordinal)` ids and copies only the submission's ordinal range.
   An evicted range still returns the original sequence with
   `events_truncated: true`; covered by a unit test.

7. **Check the market-data gate before building a frame (P1, implemented).**
   `take_market_frame` drains the depth journal, then checks
   `MarketDataTap::is_enabled()` before cloning the entry's events. Followers and
   deposed leaders no longer pay for a frame that the tap would drop. Longer
   term, share the entry's events as `Arc<[MatchEvent]>` between the response,
   event window and tap.

8. **Make the market-data task yield (P1, medium risk).**
   `MarketDataHandle::start` already uses `tokio::spawn`; once the node runs on
   a multi-threaded runtime it can execute on another worker. The remaining
   problem is that `JsonlSink::publish` performs blocking `BufWriter` writes and
   flushes inside the task. Switch the sink to `tokio::fs::File` +
   `tokio::io::BufWriter` and await the writes, or wrap the existing synchronous
   sink in `spawn_blocking`. No dedicated OS thread is needed.

9. **Keep gRPC on the shared Tokio runtime (P1, satisfied by item 1).**
   The tonic server is already a set of green threads on the multi-thread
   runtime; no separate runtime was added. Snapshot decode/validation moved to
   `spawn_blocking` in item 2, so large append and snapshot messages compete for
   workers rather than blocking RaftCore.

10. **Share snapshot bytes (P2, medium effort).**
    `build_snapshot` clones the encoded body into `current_snapshot` and returns
    the original; `get_current_snapshot` clones the whole body under the lock.
    Store `Arc<[u8]>` (or `bytes::Bytes`) in `StoredSnapshot` and return a cheap
    cursor clone. This removes full-body copies proportional to snapshot size.

11. **Avoid the snapshot deep copy (P2, high effort).**
    `to_content` clones every order, stop, event and submission record under the
    lock. A COW/immutable view or an incremental/dirty-page snapshot would cut
    the lock hold time proportional to book and event-window size. Until then,
    keep the capture short and do encode/write off-runtime.

### What not to do

- Do not spawn a dedicated OS thread per service. Use `tokio::spawn` on the
  multi-threaded runtime and reserve `spawn_blocking` for blocking or CPU-heavy
  calls.
- Do not move matching to another task. It is short, deterministic and needs the
  state-machine lock; a channel hop would add latency and complicate ordering.
- Do not replace the market-data `try_send` with an unbounded queue. Dropping
  frames and emitting `GAP` is the designed overload contract; unbounded memory
  is worse.
- Do not add a second fsync or a synchronous sink to the apply path. The WAL's
  one-append/one-fsync batch is the reason the batch-write path scales.

### Validation plan

- The P0 changes (items 2-6) are covered by the existing unit/integration suite
  plus `duplicate_response_reports_evicted_events`. The dedicated A/B above uses
  `cargo bench --bench p0_bench -- duplicate|read|snapshot`.
- The binary now uses the multi-threaded runtime; compare
  `cargo bench --bench e2e` p50/p99 and `cargo bench --bench feed_load` with the
  previous single-thread baseline when a direct before/after is needed. Actix
  shutdown and `ServerHandle::stop` are covered by the integration tests.
- Add a snapshot benchmark: 100k resting orders and a full 262k-event window;
  measure `persist_snapshot`, `install_snapshot` and write p99 while a snapshot
  builds.
- Add a `/read orderbook_orders` benchmark with 100k orders and measure the
  apply-task stall (or lock wait) before and after serializing outside the lock.
- Re-run `cargo bench --bench e2e`, `cargo bench --bench feed_load` and
  `cargo bench --bench storage`; compare p50/p99, not just throughput.
- During snapshot install, watch Raft metrics for heartbeat/leader changes; a
  healthy result is no election and no write-timeout spike.

## Interpretation

- Matching and feed derivation are not the current bottleneck.
- Sequential write latency is fsync-dominated; batching concurrent submissions
  improves throughput substantially.
- The feed has ample headroom at 500k fills/s with a null sink. A real sink
  (JSONL or Aeron) must be measured separately because it adds encoding and I/O.
- No per-level bucket or compact resting-order change is justified by these
  measurements yet. Revisit after a 3-node e2e run and a real feed sink.
- The runtime flavor A/B is within noise for the write path: WAL fsync and the
  serialized apply path dominate. P0 items 2-6 are implemented: snapshot
  encode/decode and read serialization are off the lock, apply locks per entry
  and publishes after release, and duplicate retries binary-search the event
  window. Snapshot byte sharing and optional sink offload remain open.
