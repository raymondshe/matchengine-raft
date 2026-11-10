# Performance measurements

Measured on **2026-10-30** (runtime flavor A/B and P0 offloads: **2026-10-31**; measurement scope and RSS: **2026-11-01**; book p50/p99 and the pinned-core approximation: **2026-11-04**; end-to-end/storage rerun with the OpenRaft quick wins: **2026-11-06**; realistic-mix replay: **2026-11-06**; dedicated Raft runtime: **2026-11-06**; WAL fsync Tokio rerun: **2026-11-07**; four-node no-fsync cluster: **2026-11-07**; write concurrency/batching sweep: **2026-11-08**; async WAL prototype: **2026-11-08**; production configuration decisions: **2026-11-08**; write admission control: **2026-11-09**; pre-connected write concurrency: **2026-11-09**; sustained single-node CPU profile: **2026-11-09**; benchmark regression gate: **2026-11-09**; read-path benchmark: **2026-11-08**; OpenRaft minimal-benchmark comparison: **2026-11-08**; WebSocket fan-out: **2026-11-10**; open-loop e2e, current-HEAD book re-pin and sorted-vector A/B: **2026-11-10**; apply-path inline buffers, affinity A/B and per-level rerun: **2026-11-10**; local lease-read experiment and dependency upgrade: **2026-11-10**) on the reference development machine:

- Apple M4 (`arm64`)
- Rust `1.93.1`
- Release/bench profile
- Local loopback networking, single process for the end-to-end test

These are engineering baselines, not a capacity guarantee. Run the commands below
on the deployment hardware before using the numbers for sizing.

## Reproduce

```shell
cargo bench --bench matchengine   # order-book microbenchmarks
cargo bench --bench e2e           # submit -> commit -> apply -> response (multi-thread; -- current|both for A/B, --pin <cpu> on Linux)
cargo bench --bench e2e -- --duration 60 --clients 500  # sustained closed-loop load with process CPU and one-second samples
cargo bench --bench e2e -- --duration 10 --clients 64 --rate 50000  # fixed-rate open-loop: scheduled, dispatch and service latency
RAFT_WAL_FSYNC_MS=0 MATCHENGINE_ASYNC_WAL=1 cargo bench --features async-wal --bench e2e -- both  # prototype async WAL A/B
cargo bench --bench cluster_e2e -- --rounds 3  # four in-process no-fsync voters, leader-direct writes and batches
cargo bench --bench cluster_e2e -- --rounds 2 --concurrency-only  # 1/8/64/256/1024/4096-client sweep
cargo bench --bench cluster_e2e -- --rounds 4 --concurrency-only --clients 256,512,1024,2048 --max-inflight-writes 1024 --preconnect  # pre-connected admission sweep
cargo bench --bench read_e2e -- --policy both  # local /read, /linearizable_read and /events; ReadIndex vs local_lease (or --policy local_lease)
cargo bench --bench feed_load     # market-data feed at 50k/500k fills/s
cargo bench --manifest-path demos/market_data_system/Cargo.toml --bench ws_fanout -- --clients 64 --updates 10000 --rate 20000  # WebSocket fan-out
cargo bench --bench storage       # WAL append/read/recovery
RAFT_WAL_FSYNC_MS=0 MATCHENGINE_ASYNC_WAL=1 cargo bench --features async-wal --bench storage  # prototype async WAL
cargo bench --bench p0_bench -- duplicate|read|snapshot  # P0 lock/scan A/B
cargo bench --bench book_replay   # book-only sweeps, deep cancels and BBO advances
cargo bench --bench book_replay -- --matrix  # occupancy scenario matrix: static/normal/wide-swing/flash-crash
cargo bench --bench replay_mix    # realistic add/cancel/amend/stop/iceberg replay with per-op percentiles
cargo bench --bench level_book    # per-level bucket prototype vs the current book
cargo bench --bench sorted_book   # sorted-vector price ladder vs the current book (book-only)
cargo bench --bench apply_path    # apply-path heap allocations per applied entry
cargo bench --bench apply_path -- --check   # exact allocation regression gate
cargo bench --bench book_replay -- --check  # loose warm p50/p99 gate
cargo bench --bench replay_mix -- --check   # loose whole-replay p50/p99 gate
```

Set `RAFT_CORE_WORKERS` to a positive worker count to run Raft on a dedicated runtime; unset or `0` uses the shared runtime.
Set `RAFT_WAL_FSYNC_MS` to control WAL fsync: unset = no fsync, `0` = every append, `N > 0` = every N ms.

The OpenRaft minimal comparison below is reproduced from the upstream
[`openraft/benchmarks/minimal`](https://github.com/databendlabs/openraft/tree/main/benchmarks/minimal)
crate; it is not a local benchmark.

## Measurement scope

Each number below answers a different question. Do not compare one row to another without checking
its scope.

| Benchmark | Includes | Excludes | Load model | Timer / sampling | Variance |
|---|---|---|---|---|---|
| `matchengine` | Order-book operations: sweep, rest, partial fill, IOC cancel, iceberg refill, amend, cancel; optional depth journal | HTTP, Raft, WAL, snapshots, market-data encoding/sink | Closed-loop, in-process | `Instant`, best-of-5 per round | Best-of-5 only; not a tail study |
| `book_replay` | `OrderBook` only: 100-level and single-level sweeps, deep cancels, BBO advancement and static BBO queries; clone/eviction outside the timed window | HTTP, Raft, WAL/snapshots, market-data feed, event construction, fees | Closed-loop, in-process; one pristine book per round | `Instant` per round; best-of-5 repeats, warm p50/p99, interleaved cold p50/p99 after a 64 MiB eviction sweep | 5-14% run-to-run; cold adds ~8-22%; 1k/20k cases differ in tree depth, not just order count |
| `book_replay --matrix` | Four calibrated standing-book shapes (static 20.9k orders/60 levels to flash-crash 240/210) with a 50k-op steady-state mix: 45% add, 43% cancel, 10% amend, 2% taker | HTTP, Raft, WAL/snapshots, market-data feed, event construction, fees, occupancy sampling in the timed pass | Closed-loop, in-process; 10 whole-replay trials per shape; initial book and fixture cloned outside the timer | `Instant` per whole replay; median, population stddev and CV; occupancy sampled every 64 ops in a separate untimed pass | Static CV 2-7% across runs; the other shapes are below 3%; not the external 2M-message flow |
| `replay_mix` | Deterministic 100,000-operation `OrderBook` stream: limit adds, icebergs, parked stops, cancels, amendments and market takers, with maker/taker fees on every order; whole-replay and per-operation timings | HTTP, Raft, WAL/snapshots, market-data feed, event construction, symbol/tick validation | Closed-loop, in-process; fixture generated at startup, book rebuilt per round | `Instant` per operation for the per-kind table; 20 whole-replay warm rounds plus 20 interleaved cold rounds after a 64 MiB eviction sweep | Four runs; whole-replay warm p50 within 5%, per-operation p50 within ~30%; p99 and max are host-noise sensitive |
| `level_book` | Prototype per-level buckets plus the current `OrderBook`: match sweeps, deep cancels, sparse/dense amends, depth, BBO queries and BBO advancement; plain limit orders only; clone/eviction outside the timed window | Stops, icebergs, GTD expiry, validation, events, depth journal, market-data feed, fees beyond `MatchResult` construction | Closed-loop, in-process; paired current/prototype books | `Instant` per round; best-of-5 repeats, warm p50/p99, interleaved cold p50/p99 after a 64 MiB eviction sweep | 5-10% run-to-run, BBO-query ratio 4.9-5.6x; sparse one-order-per-level cases are the prototype's worst case |
| `sorted_book` | Bench-only sorted-vector price ladder plus the current `OrderBook`: match sweeps, deep cancels, same-price/price-move amends, depth, BBO queries and BBO advancement; plain limit orders only; clone/eviction outside the timed window | Stops, icebergs, GTD expiry, validation, events, depth journal, market-data feed, fees beyond `MatchResult` construction; no production adoption | Closed-loop, in-process; paired current/sorted books | `Instant` per round; best-of-5 repeats, warm p50/p99, interleaved cold p50/p99 after a 64 MiB eviction sweep | Four runs: match/cancel/amend-volume wins stable in direction but 2.24-2.46x on cancel/amend; price-move amend and BBO advance regress consistently; BBO-query win reflects the prototype's cached BBO |
| `apply_path` | `StateMachineStore::apply` only: blank, rest, cancel, fill and 128-request batch workloads; a counting global allocator reports heap calls and requested bytes | HTTP, Raft, WAL, snapshots, market-data tap, event-log reads, response serialization | Closed-loop, direct apply; 50k entries or 1k batches per workload | Counting allocator; per-entry averages | Requested bytes include realloc churn and are not a live-memory high-water mark |
| `e2e` `/write` | HTTP decode/validation, Raft proposal/commit, WAL append + `sync_data`, apply, response serialization | Cross-node replication, external client network beyond loopback | Sequential and concurrent bursts are closed-loop; `--duration --clients` is closed-loop; `--rate` is fixed-rate open-loop with an absolute schedule | `Instant` per request; p50/p95/p99/max; open-loop also reports dispatch lag and service latency | Medians/percentiles; runtime A/B uses four alternating runs, the 2026-11-06 rerun uses three; open-loop tails include generator scheduling |
| `e2e` `/write_batch` | Same as `/write`, with 1–128 requests per Raft entry and one WAL fsync per entry | Same as `/write` | 200 sequential batches per size; 100 concurrent 32-order batches | `Instant` per batch | Medians of three runs per size; batch latency includes payload/apply growth |
| `cluster_e2e` | Four in-process loopback voters with a separate client runtime; HTTP `/write`/`/write_batch`, Raft 3/4 quorum replication over gRPC, WAL append without fsync, apply and response; optional client-count, write-admission and OpenRaft config sweeps | Cross-host network latency, independent machines, disk contention between nodes | 50 warm-up writes; 500 sequential, 200 batches/size, 100×32 concurrent batches, and a 1/8/64/256/1024/4096-client sweep; fresh rounds | `Instant` per request/batch; p50/p95/p99 and medians | Two or three fresh rounds; high-concurrency tail is host-sensitive |
| `read_e2e` | Four in-process loopback voters with a separate client runtime; local `/read`, `/linearizable_read` and `/events`; ReadIndex, core lease and local-lease policies; 1/64 readers; read-only and mixed with an offered 2,000 writes/s | Cross-host network and clock skew; independent machines | 100 warm-up reads per endpoint; 2,000 reads/reader per case; 100-event pages; depth-20 local reads; two runs per policy | `Instant` per request; p50/p95/p99 and throughput | Two runs; read-only rows are stable, mixed rows depend on writer pacing |
| `feed_load` | Tokio tap, bounded queue, shadow book, wire encoding and processor drain with a null sink | JSONL/network sink I/O, Raft, HTTP | Offered-load generator at 50k/500k/2M fills/s | `Instant`; queue/drain counters | One run per target; no p99 table |
| `storage` | WAL append/read/reopen with 20k real order entries and 512 KiB segments; optional `async-wal` prototype | State machine, Raft, HTTP | Closed-loop, 100-entry batches | `Instant` | Five runs per mode; no tail study; async variance is high |
| `p0_bench duplicate` | State-machine duplicate-retry lookup over a 262k-event window | HTTP, Raft, WAL | 200 retries after one setup | `Instant`; mean/p50/p99/max | Single run; a single scheduling outlier moves the mean |
| `p0_bench read` | In-process node `/read orderbook_orders` with 100k orders and 8 concurrent writers | Cross-node replication; client-side parsing beyond bytes received | 2 readers x 10 large reads, writers in flight | `Instant`; read/write p50/p99/max | Single run; allocator/HTTP buffers affect peak RSS |
| `p0_bench snapshot` | `persist_snapshot` encode/write/fsync for 100k orders while 8 writers run | Raft snapshot shipping; cross-node replication | 8 writers, snapshot on the main benchmark task | `Instant`; writer p50/p99/max and snapshot duration | Single run; encode/fsync dominated |

Timer source is `std::time::Instant` throughout; there is no `rdtsc` or assumed CPU frequency.
Loopback HTTP and local disk are part of the measured path where stated. Run-to-run variance is
not a confidence interval; the e2e runtime A/B uses four alternating runs, and the four-node
cluster section reports three fresh rounds.

The book benches share `benches/bench_support`: each round clones the state before
`Instant::now()` and drops the mutated state after the duration is recorded, so setup and teardown
are outside the timer. "Warm" means the clone immediately precedes the timed operation; "cold"
means a 64 MiB read-modify-write sweep runs between the clone and the timed operation. Warm and
cold rounds are interleaved in one time window: running all warm rounds first biased the warm pass
(early samples carried clock ramp-up and the cold pass inherited a boosted clock), which showed up
as cold p50 beating warm p50 on this host.

## Production configuration decisions (2026-11-08)

The measurements support two production profiles. Choose one; do not combine a lossy WAL mode
with a single-voter deployment. The four-voter floor is part of the no-fsync contract.

### Profile A: maximum throughput (recommended for performance)

| Setting | Production choice |
|---|---|
| WAL durability | leave `RAFT_WAL_FSYNC_MS` unset (no fsync) |
| Voters | at least 4; the binary enforces this when fsync is disabled |
| Read policy | `local_lease` (the default) uses OpenRaft's caller-side lease cache; keep it when NTP/PTP keeps clock drift bounded and `/read-policy/status` shows lease hits, otherwise set `--read-policy read_index`. The core `lease` path remains available for comparison |
| Raft runtime | `RAFT_CORE_WORKERS` unset or `0` (shared runtime) |
| OpenRaft batching | defaults: `max_payload_entries=300`, `purge_batch_size=1`, `api_batch_linger_ms=0` |
| Write admission | 1,024 concurrent `/write` or `/write_batch` requests per node; `429` overflow; `512` is the tighter-tail option and `--max-inflight-writes 0` disables |
| Snapshots | defaults: every 100,000 log entries or 30 minutes |
| Async WAL | off (no `--async-wal`, no `async-wal` feature needed) |
| Build | default release profile; no LTO requirement |
| Market data | 8,192-frame queue, leader-only default, external sink for production |

The binary defaults implement this profile, including local-lease reads. Set `--read-policy read_index`
when the deployment cannot guarantee bounded clock drift.

Profile A is **lossy on an unclean crash**. The four-voter floor protects against one node losing
its unsynced tail; it does not protect against simultaneous failures or host power loss. Use
Profile B when acknowledged writes must survive those cases.
`test_failure.rs::four_voter_no_fsync_survives_leader_kill_and_tail_loss` exercises that contract
with real processes: it kills the leader, removes three tail records from its WAL, proves the
remaining three voters still commit, and waits for the restarted node to catch up.

Measured envelope: the four-node no-fsync cluster does **5,039 sequential writes/s**,
**114,484 writes/s at 512 pre-connected in-flight** (p99 5.76 ms), **99,330 writes/s at 1,024**
(p99 8.74 ms), **227,319 orders/s** for sequential 128-order batches and **216,163 orders/s** for
100 x 32 concurrent batches. The single-node no-fsync control is higher for sequential writes
(9,648/s) because there is no 3/4 quorum round trip; use the cluster rows for sizing.

Client shape: use `/write_batch` with 32-128 orders per entry. 128 maximizes throughput
(227k orders/s in the four-node cluster); 32-64 keeps batch latency lower. The server admits at
most **1,024** concurrent `/write` or `/write_batch` requests per node by default; excess requests
receive `429` and should be retried with backoff. Lower `--max-inflight-writes` to `512` for a
tighter tail at roughly 1,024 concurrent clients, or set it to `0` to disable rejection; tune the
value against the deployment load. The measured batch shape is 100 concurrent 32-order batches,
well below the gate. `apply` holds the state-machine lock for the whole batch, so do not exceed
the 128-request cap.

### Profile B: durable writes

Set `RAFT_WAL_FSYNC_MS=0`. Keep the same runtime, OpenRaft, snapshot, feed and build choices,
and use `/write_batch` for throughput. Single-node measurements: 262 sequential writes/s,
17.9k concurrent writes/s, 180.2k orders/s for 100 x 32 concurrent batches, sequential
p50/p99 3.9/5.0 ms. A four-node every-append run has not been measured; expect the quorum
round trip plus follower fsyncs to reduce these numbers.

### Rejected options

| Option | Measurement | Decision |
|---|---|---|
| `max_payload_entries=1,024` | 66.0k vs 90.6k writes/s at 256 | keep 300 |
| `purge_batch_size=1,024` with snapshots every 1,000 | 58.3k writes/s; snapshot-heavy control 60.3k | keep 1 and the 100k snapshot policy |
| `api_batch_linger_ms=1` | 37.4k writes/s, p99 19.75 ms | keep 0 |
| `RAFT_CORE_WORKERS=2` | 65.7k vs 90.6k at 256; paired A/B within noise; the 2026-11-10 single-worker A/B was also within noise | keep shared; use 2 only for isolation |
| Thin LTO + `codegen-units=1` | 77.8k vs 74.1k paired control, within host spread | keep the default profile |
| Async WAL | storage 6.6x, but e2e 18.3k vs 17.3k writes/s (noise) | keep off; not a request-path win |
| Fsync every 10 ms | 7,374 writes/s and 2.2-3.0 ms p99 | reject: dominated by both alternatives |
| Fsync every 100 ms | 9,495 writes/s vs 9,648 disabled; still lossy and no four-voter guard | not selected; use only with four voters and an accepted 100 ms loss window |
| Per-level bucket book | 2.00x sweep/3.27x cancel, but 0.88x sparse amend and 0.94x depth; BBO advance 1.04x | keep the current book; prototype only, no sparse fast path |
| Arena allocator (`bumpalo`) | 79-88% worse single-operation latency in the external review | keep inline buffers; do not adopt |
| Full-book `/read orderbook_orders` under load | p99 134 ms, 1.49 GiB peak RSS | use `orderbook_depth:N`; keep full-book reads off the hot path |
| Unbounded market-data queue | the 2M fills/s overload test relies on drops + `GAP` | keep the bounded 8,192 queue |

### Rules of thumb

- Local `/read` is the fast path and may be stale; `/linearizable_read` and `/events` use the
  read policy. Lease reads measured 2.17x/1.71x for `/linearizable_read` at 1/64 readers and
  1.46x/1.18x for `/events`; confirm fallback counters before relying on them.
- Do not use `--match-event-log` or `--market-data-log` as production feeds; they are node-local
  debug sinks. Use the replicated `/events` cursor or the Aeron market-data service.
- Keep `--max-events` at the default 262,144 unless the event window is deliberately sized; a
  larger window increases memory and snapshot cost. External consumers should follow `/events`.
- Re-run `cargo bench --bench cluster_e2e`, `read_e2e` and `e2e` on deployment hardware before
  treating these as capacity numbers; all cluster rows are four in-process loopback voters.

## Memory footprint (measured peak RSS)

Peak RSS measured with `/usr/bin/time -l` on the release bench binaries on 2026-11-01, running
the binaries directly so Cargo's own peak is not included. These are **whole-process** peaks,
including the benchmark driver, runtime, HTTP buffers and allocator retention; they are not
per-order estimates and should not be compared directly across processes.

| Workload | Peak RSS | What dominates the peak |
|---|---:|---|
| `matchengine` microbench | 603 MiB | 20k-order deep book plus per-round clones/allocations |
| `book_replay` | 604 MiB | 20k-order single-level book, 1k-level BBO book and per-round clones; allocator retention dominates |
| `e2e` write path | 176 MiB | Node + driver, ~51k resting orders/events, HTTP/JSON buffers |
| `feed_load` | 7 MiB | Queue and null-sink processor |
| `storage` | 15 MiB | 20k WAL entries and segment buffers |
| `p0_bench duplicate` | 163 MiB | 262k retained events and the state-machine book |
| `p0_bench read` | 1.49 GiB | 100k-order book plus 20 large `orderbook_orders` JSON responses; allocator retention and client buffers dominate |
| `p0_bench snapshot` | 171 MiB | 100k-order book plus snapshot capture/encode/write buffers |

The `p0_bench read` peak is a warning about large `/read` responses, not a per-order book cost.
`level_book` runs the current book and the prototype in one process, so its RSS is not a
per-design A/B; measure each design in a dedicated process before using memory as an adoption
criterion. For per-order sizing, use the workload's own measured RSS delta or a dedicated
allocation profile; do not infer it from these whole-process peaks.

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

## Book-only replay benchmark

`cargo bench --bench book_replay` exercises `OrderBook` directly: no HTTP, Raft, WAL, snapshot
or market-data feed. Each case builds a pristine book once, clones it outside the timer, and
reports three views of one run: the historical best-of-5 mean-of-rounds, warm p50/p99 over all
per-round samples, and a cold p50/p99 with the caches evicted between clone and operation.

| Case | Workload per round | Warm best | Warm p50 | Warm p99 | Cold p50 | Best-case throughput |
|---|---|---:|---:|---:|---:|---:|
| Deep sweep, 100 levels | 1,000 asks resting 10 per level; one taker crosses all | 85.8 µs | 85.7 µs | 171.8 µs | 104.4 µs | 11.66M fills/s |
| Deep sweep, 1 level | 20,000 asks at one price; one taker crosses all | 2.258 ms | 2.319 ms | 2.856 ms | 2.512 ms | 8.86M fills/s |
| Cancel at depth | 5,000 bids across 50 levels, cancelled deepest-first | 254.7 µs | 256.7 µs | 322.6 µs | 311.8 µs | 19.63M cancels/s |
| BBO advance | 1,000 levels x 2 sides; cancel the best, then read bid/ask/spread | 98.2 µs | 97.5 µs | 153.9 µs | 114.7 µs | 20.37M advances/s |
| BBO query | 1,000,000 `spread()` calls against a 1,000-level book | 2.301 ms | 2.492 ms | 2.820 ms | 2.542 ms | 434.5M queries/s |

Warm p50 tracks best-of-5 within 9%, so the historical number is a fair summary of the typical
round; the p99 column is where the tail shows. The 1k-ask sweep has the widest tail (p99 2.0x p50)
because a 100-level tree is small enough that one preemption is a large fraction of the round; the
20k single-level sweep spreads the same work over 2.3 ms and its p99 is 1.2x p50. Cold p50 is
8-22% above warm for the destructive cases and ~2% above warm for the static BBO query, whose
working set is the tree edge rather than every node.

The two sweeps both cross every resting ask but differ in tree shape: the 100-level book keeps
each price subtree small, while the single-level book is one 20,000-entry sequence-ordered
tree, so per-fill cost rises. Cancels and BBO advancement pay `BTreeMap` removal plus best-level
lookups; static BBO queries repeatedly call `spread()`, which reads `first_key_value()` on each
side, so the 434M/s figure shows lookup is cheap relative to mutation. These are whole-round
times, not per-fill latency percentiles, and the benchmark is closed-loop.

Two consecutive runs on the same machine produced 84.7-85.8 µs best / 85.7-89.2 µs warm p50 for
the 100-level sweep, 2.26-2.50 ms / 2.32-2.49 ms for the single-level sweep, 255-267 µs /
257-271 µs for the deep cancels, 98-108 µs / 98-107 µs for BBO advancement and 2.30-2.59 ms /
2.49-2.85 ms for the 1M-query loop. Treat these as order-of-magnitude baselines, not exact
constants.

## Book occupancy scenario matrix (2026-11-10)

`cargo bench --bench book_replay -- --matrix` is the measurement envelope inspired by the 249-engine
FlashOne survey. It builds one `OrderBook` per regime from the survey's canonical seed-23 average
standing book, then replays a deterministic 50,000-operation steady-state mix over it:

| Scenario | Initial orders/levels | Mix | Purpose |
|---|---:|---|---|
| `static` | 20,900 / 60 | 45% add, 43% cancel, 10% amend, 2% taker | Deep book with many orders per level; nothing sweeps the tail |
| `normal` | 2,700 / 290 | same | Routine book with moderate depth |
| `wide-swing` | 1,300 / 830 | same | Many levels, one or two orders per level |
| `flash-crash` | 240 / 210 | same | Sparse book after a dislocation |

The mix is a book-only stress mix, not the survey's 2M-message flow: it keeps the order count
roughly constant so the four shapes stay comparable. Occupancy is sampled every 64 operations in a
separate untimed pass; the timed pass excludes fixture generation, the initial-book clone, occupancy
sampling and teardown. Each scenario runs 10 whole-replay trials and reports the median, population
standard deviation and coefficient of variation; the lowest median throughput is the worst case.

One representative run on this macOS host gave:

| Scenario | Avg orders/levels | Peak orders/levels | Median | Stddev | CV | Median M ops/s |
|---|---:|---:|---:|---:|---:|---:|
| `static` | 20,917 / 59.4 | 21,226 / 60 | 10.73 ms | 658 µs | 6.1% | 4.66 |
| `normal` | 3,090 / 282.1 | 3,558 / 290 | 7.33 ms | 123 µs | 1.7% | 6.82 |
| `wide-swing` | 1,771 / 711.5 | 2,180 / 830 | 7.07 ms | 108 µs | 1.5% | 7.07 |
| `flash-crash` | 432 / 176.1 | 621 / 210 | 6.34 ms | 75 µs | 1.2% | 7.88 |

The worst case is `static` at **4.66-5.00 M ops/s** across three runs (normal 6.82-7.01,
wide-swing 7.07-7.21, flash-crash 7.87-7.88). That is the expected direction:
the 20.9k-order book pays for the deep, dense level set on every cancel, amend and match, while the
sparse flash-crash shape runs about 1.7x faster. The external survey's 7.20 M/s result for this
repository is not directly comparable: it is a different workload, hardware and operation mix, and
this matrix deliberately isolates the price ladder rather than reproducing the full message flow.
The central number to compare over time is the worst-case median and its CV, not the leaderboard.

The manual Linux benchmark workflow (`.github/workflows/bench-linux.yml`) runs the pinned short and
sustained e2e passes and then the occupancy matrix, so a pinned-host envelope is captured without a
separate command. The regular `--check` mode still runs only the five case-level gates above.

### Current-HEAD book-only re-pin (2026-11-10)

The production code under test is `ee07ba0`; the later lock-file and benchmark/documentation
commits add no `OrderBook` changes. Three clean `book_replay --matrix` runs with no other
local nodes running gave:

| Scenario | Median | M ops/s (median) | Range M ops/s | CV |
|---|---:|---:|---:|---:|
| `static` | 13.58 ms | **3.68** | 3.62-4.23 | 2.0-5.5% |
| `normal` | 8.46 ms | 5.91 | 5.91-6.02 | 0.8-1.2% |
| `wide-swing` | 8.10 ms | 6.18 | 6.14-6.28 | 0.7-0.9% |
| `flash-crash` | 7.38 ms | 6.78 | 6.77-6.85 | 0.8-2.6% |

The worst-case `static` shape is **3.62-4.23 M ops/s** across runs (median 3.68), which is the
current-HEAD local re-pin. The earlier 4.66-5.00 M ops/s run above remains historical; the spread
is host/run variance, not a code change. The scope is a local 50k-operation occupancy envelope,
not the external 2M-message flow. The FlashOne survey still pins this repository at `5597532` and
reports 7.20 M ops/s; that number is not comparable to this matrix (different hardware, workload
and operation mix).

**Adapter decision: not published.** A FlashOne adapter would need to translate their client-id
cancel+reinsert `modify` (not our `Amend`), their ABI and their book-query semantics, and it would
need external maintenance in another repository. The maintained local baseline stays
`book_replay --matrix`, which reads the real `OrderBook` and is already run by the manual Linux
workflow. A third-party re-pin is the benchmark maintainer's call.

## Realistic-mix replay benchmark (2026-11-06)

`cargo bench --bench replay_mix` is the load analogue of `tests/replay`: it replays a deterministic
100,000-operation fixture through `OrderBook` directly (no HTTP, Raft, WAL, snapshot or
market-data feed) and reports whole-replay percentiles plus per-operation p50/p99/max by operation
kind. The fixture is generated at startup from a fixed-seed LCG rather than checked in, so the
operation stream is identical across runs and machines.

| Operation | Count | Share | Warm p50 | Warm p99 | Max |
|---|---:|---:|---:|---:|---:|
| Limit add | 55,546 | 55.5% | 1.333 µs | 2.833 µs | 775.500 µs |
| Iceberg add | 9,903 | 9.9% | 1.375 µs | 2.834 µs | 15.583 µs |
| Stop add (parked) | 4,858 | 4.9% | 84 ns | 292 ns | 3.291 µs |
| Cancel | 9,928 | 9.9% | 625 ns | 2.041 µs | 7.667 µs |
| Amend | 9,757 | 9.8% | 292 ns | 666 ns | 11.125 µs |
| Market taker | 10,008 | 10.0% | 1.417 µs | 6.833 µs | 17.875 µs |
| **All operations** | **100,000** | **100%** | **791 ns** | **3.041 µs** | **775.500 µs** |

Whole replay: warm best 103.230 ms, warm p50 107.252 ms, warm p99 109.363 ms (**968,709 ops/s**
at the best round), cold p50 106.638 ms, cold p99 108.522 ms (n=20). Warm and cold rounds are
interleaved; at n=20 the nearest-rank p99 is the slowest round, so the per-operation table is the
stronger tail signal. Across four runs the whole-replay warm p50 was 107.25-112.32 ms (within 5%)
and per-operation p50 varied by up to ~30%; one noisy run produced a 182.6 ms warm p99 and a
10.1 µs per-operation p99, so treat p99 and max as host-noise sensitive.

Every order carries maker `0.0002` / taker `0.0005` fee rates, so fee computation is on every fill
path. The fixture is built to exercise the full lifecycle rather than to hit a target reject rate:
all 55,546 limit adds and 9,903 icebergs rest, all 4,858 stops park, and there are no rejections.
The takers produce 12,212 fills and elect 2,546 parked stops; 9,526 of the 9,928 cancel attempts
find a live order (the rest target an order already consumed by a fill or stop trigger), and all
9,757 amendments succeed.

The per-operation max is host-noise sensitive (a single 0.8-2.3 ms outlier moved between
operation kinds across runs); p99 is the useful tail. Market takers have the widest p99/p50 ratio
(~4.8x) because each taker may sweep several price levels and elect stop children, while parked
stops and amendments are single tree insertions and stay sub-microsecond at p99.

## Per-level bucket prototype (2026-11-04, rerun 2026-11-10)

`cargo bench --bench level_book` compares a bench-only prototype with `OrderBook` on the same
workloads. The prototype keeps one `Vec<Option<Order>>` per price level, a `head` cursor for the
consumed prefix, a sequence-keyed location index for cancel/amend and cached best bid/ask prices.
The index uses an identity hasher for the monotonic `u64` sequence keys; the standard SipHash
variant was also tried and made the sparse cases below slower.

This is a **prototype, not production code**. It covers plain limit orders only and skips stops,
icebergs, GTD expiry, validation, events, the depth journal and the market-data tap, so the numbers
isolate the book data structure. Before timing, the benchmark runs a deterministic mixed workload
that compares BBO, depth, fills, cancels and amendments against `OrderBook`; it prints
`prototype verification: OK`.

| Case | Current warm p50 | Prototype warm p50 | Speedup (p50) | Speedup (best-of-5) |
|---|---:|---:|---:|---:|
| Match 1k asks / 100 levels | 108.6 µs | 54.3 µs | 2.00x | 2.01x |
| Match 20k asks / 1 level | 3.332 ms | 1.849 ms | 1.80x | 1.91x |
| Cancel 5k at depth / 50 levels | 324.8 µs | 99.5 µs | 3.27x | 3.17x |
| Amend volume 5k (same price) | 264.8 µs | 116.2 µs | 2.28x | 2.33x |
| Amend price 1k sparse (1 order/level) | 97.0 µs | 110.4 µs | 0.88x | 0.88x |
| Amend price 5k dense (50 orders/level) | 785.8 µs | 286.4 µs | 2.74x | 2.70x |
| Depth 10 levels, 100k queries | 6.631 ms | 7.075 ms | 0.94x | 0.94x |
| BBO query 1k levels, 1M calls | 2.971 ms | 0.488 ms | 6.09x | 5.62x |
| BBO advance 1k levels x 2 sides | 129.6 µs | 125.0 µs | 1.04x | 1.04x |

This is the 2026-11-10 rerun at current `HEAD`; the 2026-11-04 table had smaller wins (1.84x match,
3.31x cancel, 1.75x/2.59x amend, 4.90x BBO query) and a BBO-advance loss, so the direction is
stable but the host spread is material. The prototype wins where a level is populated: a sweep
consumes a contiguous vector and advances `head` instead of removing every order from the tree,
deep cancels amortize the level lookup, a same-price reduction is an in-place update, and a dense
re-queue leaves the old level alive. The losses are the sparse case: with one order per level, the
sequence lookup, level indirection and level creation/removal cost more than a single `BTreeMap`
remove/insert, and depth has to walk and sum level vectors.

The cold pass does not change the ranking: cancel is 382.6 µs cold for the current book vs
103.1 µs for the prototype, sparse amend is 120.8 µs vs 201.0 µs, and BBO query is 3.000 ms vs
0.486 ms. BBO advancement is a small warm/cold win on this run (151.0 µs vs 132.0 µs cold), so the
cached best index pays off at both temperatures.

Full adoption is still **not** justified: the prototype is plain-limit only and excludes stops,
icebergs, GTD, pegged orders, the depth journal and event construction, while the sparse amend
path remains 12% slower and depth 6% slower. The next experiment would have to be a sparse-level
fast path (or a hybrid that keeps tree keys for one-order levels) plus a 3-node/real-sink run;
neither exists, so the production `BTreeMap` stays. The sorted-vector A/B above reinforces that
every price-ladder layout trades wins for structural losses; do not switch on the external
leaderboard alone.

## Sorted-vector price ladder A/B (2026-11-10)

`cargo bench --bench sorted_book` is a second **book-only prototype** experiment. It keeps price
levels in a sorted `Vec<(price, Level)>`, uses binary search for lookup and `Vec::insert` /
`Vec::remove` for level creation/removal, and caches the BBO. The per-level order vector, sequence
index and verification workload match the per-level bucket prototype, so this isolates the price
ladder. It covers plain limit orders only and is not production code.

| Case | Current warm p50 | Sorted-vector warm p50 | Speedup (p50) |
|---|---:|---:|---:|
| Match 1k asks / 100 levels | 105-109 µs | 52-56 µs | 1.93-2.07x |
| Match 20k asks / 1 level | 3.11-3.33 ms | 1.77-1.85 ms | 1.73-1.84x |
| Cancel 5k at depth / 50 levels | 316-339 µs | 137-151 µs | 2.24-2.36x |
| Amend volume 5k (same price) | 218-265 µs | 105-116 µs | 2.07-2.46x |
| Amend price 1k sparse (1 order/level) | 90.4-97.0 µs | 642-645 µs | **0.14-0.15x** |
| Depth 10 levels, 100k queries | 6.55-6.75 ms | 5.42-5.60 ms | 1.20-1.23x |
| BBO query 1k levels, 1M calls | 2.43-2.97 ms | 0.234-0.247 ms | 10.1-12.0x |
| BBO advance 1k levels x 2 sides | 129.4-129.8 µs | 373-375 µs | **0.34-0.35x** |

The wins are real for this workload: grouping orders by level cuts the number of tree entries, a
sweep/cancel walks a contiguous level vector, and a same-price amendment updates in place. The
losses are structural: a price move removes and re-inserts a level in the sorted `Vec` (O(n)
shifts), and BBO advancement removes the first/last level on every cancellation, so a wide ladder
pays O(n) per advance. The BBO-query win is not a pure price-ladder result; the prototype caches
best bid/ask while the current `OrderBook::spread` reads the tree edge. Across four runs the
cancel ratio moved 2.24-2.36x and same-price amend 2.07-2.46x, so treat the ratios as directional.

Conclusion: **do not adopt a pure sorted vector.** It is a useful small-book/level-grouping
signal, not a universal replacement. A tree/heap hybrid that keeps sorted levels with O(log n)
level removal is the only follow-up worth measuring, and only if a sparse-level fast path is
needed; the external survey's treap/heap leaderboard numbers are not sufficient evidence on
their own. The cold pass preserves the ranking.

## Apply-path allocation benchmark (2026-11-10)

`cargo bench --bench apply_path` drives `StateMachineStore::apply` directly with a counting global
allocator. It covers blank entries, one-event rest/cancel/fill entries, and 128-request batches;
the blank and batch-blank cases isolate the harness and response allocations. Counts are heap
allocation calls per applied entry; bytes are the sum of requested sizes **including realloc
churn**, not live memory. The timer is not the gate here: the per-entry async apply harness
dominates it, so read the allocation columns.

The event path previously wrote fills/rests into an inline `SmallVec<[MatchEvent; 4]>`, but the
response then copied one event into a `Vec`. The 2026-11-10 change keeps one response event
(`SmallVec<[MatchEvent; 1]>`) inline; larger responses spill exactly like a `Vec`. The retained
event window, the per-request batch wrappers and the fill `Vec` still allocate.

We also tried `FillBuffer = SmallVec<[MatchResult; 1]>` for the sweep. It removed the fill
allocation, but enlarging `Outcome` and every `place_order` return by one `MatchResult` slowed the
book-only `replay_mix` whole replay from ~107 ms to ~185 ms (and was not worth it for apply_path).
It was reverted; the fill `Vec` stays.

| Workload | Before allocs/entry | After allocs/entry | Before bytes/entry | After bytes/entry |
|---|---:|---:|---:|---:|
| Blank | 0.00 | 0.00 | 0.0 | 0.0 |
| Rest, 1 event | 4.17 | 3.17 | 595 | 568.5 |
| Cancel, 1 event | 4.00 | 3.00 | 156 | 12.1 |
| Fill, 1 event | 5.00 | 4.00 | 2,012 | 2,380.1 |
| Batch blank, 128 requests | 1.02 | 1.02 | 381 | 597.1 |
| Batch rest, 128 x 1 event | 4.19 | 3.19 | 1,693 | 1,882.4 |

The single-entry rest/cancel rows drop one allocation per entry and the response bytes fall; the
fill row drops one allocation but its bytes rise slightly because the fill `Vec` still allocates
and the response header is larger. The batch rows keep the same shape but pay a larger
per-response header: each `MatchEngineResponse` now reserves room for one inline event even in an
all-blank batch, so the requested bytes rise. The allocation calls still fall for batch rest
(536,336 to 408,336), and the larger header is the deliberate trade for the cheaper common
single-event response.

Remaining allocations are the `BTreeMap` node on a rest, the fill `Vec` on a match, the
retained-event window growth, the legacy `value` sequence `String`, and harness/book-keeping
allocations. The market-data tap path still copies events into `TapFrame.events` when the tap is
enabled, and is not covered by this no-tap benchmark.

## Benchmark regression gate (2026-11-09)

CI job `bench-regression` runs three no-framework benchmarks in check mode. It is a regression
gate, not a capacity target: CI uses loose upper bounds so a shared runner does not fail on
scheduling noise, while a real algorithmic regression or an allocation-count increase fails.

`cargo bench --bench apply_path -- --check` enforces the measured 2026-11-10 allocation ceilings
for 50,000 single-entry applications or 1,000 128-request batches:

| Workload | Max allocation calls | Max requested bytes |
|---|---:|---:|
| Blank | 0 | 0 |
| Rest, 1 event | 158,335 | 30,000,000 |
| Cancel, 1 event | 150,000 | 1,000,000 |
| Fill, 1 event | 200,000 | 120,000,000 |
| Batch blank, 128 requests | 130,000 | 80,000,000 |
| Batch rest, 128 x 1 event | 408,336 | 250,000,000 |

The call ceilings are exact totals: fewer allocations pass, more fail. Requested bytes get a
small allowance for capacity choices. `book_replay --check` enforces warm p50/p99 bounds from
1 ms/5 ms (1k sweep) to 50 ms/100 ms (1M BBO queries), and `replay_mix --check` enforces
1 s/2 s for the whole warm replay. Those are roughly an order of magnitude above the recorded
M4 baseline. Cold passes remain reported but are not gated.

The pinned-core `e2e` pass is a separate, manual Linux run. `.github/workflows/bench-linux.yml`
(`Linux benchmark`, `workflow_dispatch`) selects the last available CPU, runs the short e2e suite
with `--pin`, then the 60 s / 500-client sustained pass. It has not been dispatched on this macOS
development host.

## End-to-end write path

Single node, HTTP JSON `/write`, including:

1. HTTP request decode and request validation
2. Raft proposal and commit
3. WAL append (and `sync_data` when fsync is enabled)
4. State-machine apply and order-book mutation
5. Response serialization and HTTP response

The table below uses the historical durable baseline `RAFT_WAL_FSYNC_MS=0` (fsync every
append). The default is now unset (no fsync); see [WAL fsync A/B](#wal-fsync-ab-2026-11-07)
for the current default and periodic modes.

It excludes cross-node network replication. The
[four-node no-fsync cluster](#four-node-no-fsync-cluster-2026-11-07) section adds
the 3-of-4 quorum round trip over gRPC.

| Measurement | Result |
|---|---:|
| Sequential writes | 500 |
| Sequential throughput | 262 writes/s |
| Mean latency | 3.81 ms |
| p50 latency | 3.96 ms |
| p95 latency | 4.66 ms |
| p99 latency | 5.17 ms |
| Max latency | 7.22 ms |
| Concurrent writes (1,000 in flight) | 17,245 writes/s |

The 2026-11-06 rerun reports the median of three runs. The per-run maxima varied
from 5.55 to 12.40 ms, so the max above is the median of those per-run maxima and
is not a stable tail estimate.

The gap between sequential and concurrent throughput is the expected batching
effect: sequential clients pay one WAL fsync per request, while concurrent
requests share append batches. Matching itself is measured separately above;
the single-node e2e path is dominated by HTTP plus durable append in every-append mode.

### Batch writes

`POST /write_batch` puts several requests in one Raft entry, so one WAL append
and `sync_data` covers the whole batch. Measured on the same machine with 200
sequential batches per size (orders never cross, so this measures commit/apply,
not matching):

| Batch size | Batches | Orders | Batches/s | Orders/s | Speedup vs size 1 | Batch mean | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 200 | 200 | 258 | 258 | 1.00x | 3.87 ms | 3.99 ms | 4.86 ms |
| 8 | 200 | 1,600 | 253 | 2,020 | 7.81x | 3.96 ms | 4.00 ms | 5.12 ms |
| 32 | 200 | 6,400 | 216 | 6,918 | 26.80x | 4.62 ms | 4.88 ms | 5.54 ms |
| 64 | 200 | 12,800 | 206 | 13,208 | 51.45x | 4.84 ms | 4.97 ms | 5.96 ms |
| 128 | 200 | 25,600 | 182 | 23,249 | 91.06x | 5.50 ms | 5.47 ms | 6.76 ms |

The same rerun measured 262 single `/write` requests/s. The size-1 `/write_batch`
path (258 orders/s) pays only the array envelope overhead; by size 128 the
per-order cost falls by roughly two orders of magnitude because the fsync is
amortized. Batch latency grows only from 3.87 ms to 5.50 ms as the payload and
apply work grow, so throughput scales close to the batch size.

With 100 concurrent 32-order batches (3,200 orders in flight), the same node
reached **139,323 orders/s** (4,354 batches/s), showing that concurrent batches
also coalesce into Raft append batches. These are single-node loopback numbers;
the [four-node no-fsync cluster](#four-node-no-fsync-cluster-2026-11-07) section
measures the quorum cost, while the WAL fsync amortization is unchanged. Keep
batches within the 128-request cap: `apply` holds the state-machine lock for the
whole batch.

### Sustained CPU profile (2026-11-09)

The short suite above lasts about 0.4 s warm, so instantaneous CPU bursts are easy
to miss in a dashboard or a lifetime-average `ps %cpu`, and a cold run averages
under 3% of a 10-core host. `--duration <seconds>` replaces the short suite with a
closed-loop sustained write load. `--clients <n>` sets the number of clients
(default 64) and requires `--duration`:

```text
cargo bench --bench e2e -- --duration 60 --clients 500
```

The mode pre-connects one keep-alive connection per client, then has every client
send one `/write` at a time until the deadline. It reports accepted/rejected/failed
counts, throughput, p50/p95/p99 and **process** CPU from `getrusage` (in-process
node plus load generator), both as a whole-run average and as one-second interval
samples. The workload places IOC Buy orders at 100.0 against an empty book, so no
order rests or fills and the book stays empty; it profiles the HTTP decode, Raft
propose/commit, WAL append, apply and event/response path, not matching sweeps.
WAL fsync is disabled, admission is unbounded and the snapshot timer is disabled,
but the default `LogsSinceLast(100_000)` policy still snapshots about every 100k
entries.

Apple M4 (10 logical cores: 4 performance + 6 efficiency), 25 GB RAM, macOS,
bench profile, one fresh run:

| Measurement | Result |
|---|---:|
| Clients | 500 pre-connected keep-alive connections |
| Duration | 60.01 s |
| Accepted / rejected / failed | 6,647,163 / 0 / 0 |
| Throughput | **110,760 writes/s** |
| p50 / p95 / p99 | 4.33 ms / 7.03 ms / 8.83 ms |
| Process CPU | **502.89 s = 8.38 cores** |
| Share of 10 logical cores | **83.8%** |

The one-second samples stayed between 7.48 and 8.71 cores after the first second,
with cumulative CPU settling at 8.38 cores and no downward trend over the run.
The dips (t=1 s, 7 s, 48 s, 53 s) are scheduler noise and snapshot/compaction
work, not a throughput collapse; the completed-run average is 110.8k writes/s.

An independent check against a **standalone release server** (port 24972, driven
by `ab -k -n 7000000 -c 500`) measured **80,466 writes/s**, p50 6 ms / p99 11 ms,
and **3.99 cores in the server process alone** (39.9% of 10 logical cores). The
`ab` process runs separately and is not included in that figure, so this is the
closest node-only measurement here. It used `RAFT_WAL_FSYNC_MS=1000` because the
binary sets `min_voters = 4` when the variable is unset; the single-node `/init`
therefore needs a non-default fsync mode to form a one-voter cluster. The
in-process run above additionally includes its own load generator, which explains
the higher 8.38-core figure.

CPU accounting for this benchmark:

- The node runs **inside the bench process** on `127.0.0.1:24002`. A separately
  started `matchengine-raft` binary receives no requests; monitoring it shows only
  idle CPU. Monitor the `e2e-*` bench process (or run the standalone server and
  drive it with an external load generator).
- `getrusage(RUSAGE_SELF)` covers every thread of the bench process, so the 8.38
  cores include the HTTP clients as well as the node. It is not an isolated
  node-only figure; compare it with the four-node `cluster_e2e` CPU runs, which
  also include all in-process nodes and clients.
- A percentage is only meaningful with its denominator: 8.38 cores is 83.8% of
  this 10-core host, but 13% of a 64-core host. Check whether the monitoring tool
  reports percent-of-one-core or percent-of-all-cores.
- `-- current` still selects the single-thread runtime. Sustained CPU on that path
  is bounded by one worker and is not comparable with the default multi-thread
  runtime.

### Open-loop write latency (2026-11-10)

`--rate <requests/s>` switches the sustained suite from closed-loop clients to a fixed-rate
open-loop schedule:

```text
cargo bench --bench e2e -- --duration 10 --clients 64 --rate 50000
```

Each client keeps an absolute per-client send schedule (`clients / rate` interval) and measures
latency from the **scheduled** send time, so queueing and generator lateness are not hidden. The
output separates three views:

- **open-loop**: completion minus scheduled send (the client-observed latency);
- **dispatch lag**: actual send minus scheduled send (the generator's scheduling error);
- **service**: completion minus actual send (the request path itself).

64 pre-connected clients, 10 s per row, WAL fsync disabled, one fresh node per row:

| Offered writes/s | Achieved writes/s | Open-loop p50 / p99 | Dispatch lag p99 | Service p50 / p99 | Process CPU |
|---:|---:|---:|---:|---:|---:|
| 10,000 | 9,993 | 3.49 / 5.87 ms | 2.93 ms | 1.82 / 3.70 ms | 1.44 cores |
| 30,000 | 29,987 | 1.63 / 3.25 ms | 2.31 ms | 0.55 / 1.24 ms | 3.01 cores |
| 50,000 | 49,975 | 1.51 / 3.02 ms | 2.33 ms | 0.57 / 0.97 ms | 3.98 cores |
| 80,000 | 79,952 | 1.39 / 11.73 ms | 10.93 ms | 0.63 / 1.12 ms | 6.58 cores |

The same 64-client **closed-loop** run for 10 s reached 89,714 writes/s at p50 693 µs / p99
1.33 ms and 7.33 cores. It offers more load than the 80k open-loop row because the server can
sustain it.

Repeat runs move the generator tail: one earlier 30k run showed a 29 ms scheduled p99 before the
next settled at 3.25 ms, and two 80k runs measured 11.73/11.21 ms, while the service p99 stayed
near 1.1 ms. Treat the open-loop tail as a load-generator observation unless a separate generator
process reproduces it.

**Coordinated-omission caveat.** Closed-loop tails hide queueing: when the server slows down, a
closed-loop client also sends less, so slow periods contribute fewer samples. The open-loop rows
fix the offered rate, but at 80k/s the p99 is dominated by **dispatch lag** (10.93 ms) while the
service p99 stays at 1.12 ms; the load generator, not the server, misses its schedule. macOS timer
granularity and the per-client catch-up loop contribute to that lag. Treat the open-loop number as
a client-observed envelope and the service columns as the server-path view; do not compare a
closed-loop p99 with an open-loop p99 without stating which one it is. A sustained tail study
should use a separate load-generator process (or host) before drawing server-capacity conclusions.

### Four-node no-fsync cluster (2026-11-07)

`cargo bench --bench cluster_e2e -- --rounds 3` starts four in-process voters on loopback
(HTTP 25101–25104, gRPC 26101–26104), initializes all four in one `/init`, disables WAL fsync and
runs three fresh clusters. Every node has its own storage, HTTP and gRPC listener; all four share
one process and the host CPU. Each round runs 50 excluded warm-up writes, then the same write and
batch workloads as the single-node e2e benchmark. The measured path is client → leader HTTP
`/write` → Raft 3-of-4 quorum replication over gRPC → WAL append without fsync → apply → response.

| Workload | 4-node no-fsync median | Range |
|---|---:|---:|
| Sequential `/write` | 5,039 writes/s | 4,637–5,378 |
| Sequential `/write` p50 | 186.75 µs | 182.67–206.42 µs |
| Sequential `/write` p95 | 288.33 µs | 233.96–304.92 µs |
| Sequential `/write` p99 | 368.88 µs | 260.92–457.71 µs |
| Concurrent `/write` (1,000 in flight) | 35,152 writes/s | 32,851–36,055 |
| Sequential `/write_batch`, 128 orders | 227,319 orders/s | 226,486–233,395 |
| Batch 128 p50 / p99 | 563.42 µs / 622.46 µs | 545.46–563.96 / 616.08–626.17 µs |
| Concurrent `/write_batch` (100 × 32) | 216,163 orders/s | 206,883–217,136 |

The single-node no-fsync control on the same host (`cargo bench --bench e2e`, three fresh runs)
measured 10,165 writes/s sequential (p50 87.46 µs, p99 166.96 µs), 33,831 concurrent writes/s,
310,338 orders/s at batch size 128 (p50 411.75 µs, p99 441.63 µs) and 223,406 orders/s for
100×32 concurrent batches.

| Workload | Single node, no fsync | 4-node no-fsync | Cluster change |
|---|---:|---:|---:|
| Sequential `/write` | 10,165 writes/s | 5,039 writes/s | -50% |
| Sequential p50 / p99 | 87.5 / 167.0 µs | 186.8 / 368.9 µs | 2.14x / 2.21x |
| Concurrent `/write` (1,000) | 33,831 writes/s | 35,152 writes/s | +3.9% (ranges overlap) |
| 128-order batch | 310,338 orders/s | 227,319 orders/s | -27% |
| 128-order batch p50 / p99 | 411.8 / 441.6 µs | 563.4 / 622.5 µs | 1.37x / 1.41x |
| 100×32 concurrent batches | 223,406 orders/s | 216,163 orders/s | -3.2% |

The sequential path pays one quorum round trip per write, so throughput is about half and the
p50/p99 are about 2.2x higher. Concurrent writes and batches pipeline enough replication to stay
within a few percent of the single-node control (the concurrent-write median was slightly higher,
but the ranges overlap). A 128-order batch still amortizes one quorum round trip across 128 orders,
so it retains about 73% of the single-node throughput. This is an in-process loopback cluster, not
four independent hosts: it covers the Raft quorum and gRPC transport cost, but not cross-host
network latency, independent machine failure or disk contention between nodes.

### Write concurrency and batching sweep (2026-11-08)

`cargo bench --bench cluster_e2e -- --rounds 2 --concurrency-only` starts the four-node no-fsync
topology on a server runtime and runs the write clients on a separate runtime. The sweep sets
`--max-inflight-writes 0`, so these rows measure the unbounded path; the default gate is
evaluated in [Write admission control](#write-admission-control-2026-11-09). Each client task
sends one request at a time; 1/8/64 clients send about 2,000 requests per case, 256 sends 1,792,
1,024 sends 1,024 and 4,096 sends 4,096. The table is the median of two fresh rounds.

| Clients offered | Writes/s | p50 | p95 | p99 |
|---:|---:|---:|---:|---:|
| 1 | 5,778 | 169.3 µs | 216.8 µs | 293.9 µs |
| 8 | 26,958 | 309.8 µs | 413.1 µs | 455.7 µs |
| 64 | 79,191 | 792.5 µs | 1.064 ms | 1.662 ms |
| 256 | **90,569** | 2.256 ms | 5.487 ms | 6.467 ms |
| 1,024 | 44,829 | 14.199 ms | 20.040 ms | 21.789 ms |
| 4,096 | 50,996 | 19.707 ms | 64.566 ms | 76.465 ms |

These rows start the timer before TCP connections are established, so the 1,024/4,096 cases are
connection-setup-bound rather than pure in-flight measurements. With `--preconnect`, the engine
sustains **116.9k writes/s at 512** in flight and **99.3k at 1,024** (p99 5.9/8.7 ms); see
[Write admission control](#write-admission-control-2026-11-09). Read the 1/8/64/256 rows as a
connection-storm baseline and use the pre-connected rows for capacity and tail sizing.

One-factor sweeps at the same topology. The rows are separate runs on the same host; compare
the shape and the p99 direction, not exact percentages.

| Variant | 256 writes/s (p99) | 1,024 writes/s (p99) | 4,096 writes/s (p99) | Decision |
|---|---:|---:|---:|---|
| Default: 300 / 1 / 0 / shared | 90.6k (6.47 ms) | 44.8k (21.79 ms) | 51.0k (76.47 ms) | baseline |
| `max_payload_entries = 1,024` | 66.0k (11.83 ms) | 42.5k (23.34 ms) | 49.5k (78.52 ms) | no win; keep 300 |
| `purge_batch_size = 1,024`, snapshot every 1,000 | 58.3k (12.74 ms) | 30.5k (34.77 ms) | 34.5k (125.04 ms) | no consistent win; keep 1 |
| `purge_batch_size = 1`, snapshot every 1,000 | 60.3k (12.23 ms) | 33.6k (29.03 ms) | 34.2k (136.02 ms) | snapshot-heavy control |
| `api_batch_linger_ms = 1` | 37.4k (19.75 ms) | 31.3k (31.05 ms) | 45.1k (82.11 ms) | severe regression; keep 0 |
| `RAFT_CORE_WORKERS = 2` | 65.7k (11.75 ms) | 42.1k (29.86 ms) | 46.8k (77.14 ms) | no consistent win; keep shared |
| Thin LTO + `codegen-units = 1` | 77.8k (12.33 ms) | 45.9k (21.36 ms) | 45.4k (93.90 ms) | within host spread; keep default |
| Paired default control for LTO | 74.1k (12.61 ms) | 40.2k (28.14 ms) | 40.8k (93.83 ms) | comparison control |

`api_batch_linger_ms` is a millisecond field, so the requested 100/500 µs values are not
representable; the smallest nonzero value is 1 ms, which already regressed throughput and tail.
A 100 ms run also exhausted ephemeral client ports, so it is not a usable configuration. The
default stays 0.

`purge_batch_size` only matters when snapshots run; the sweep used a deliberately aggressive
`LogsSinceLast(1,000)` policy so each round purged several times. That policy itself costs roughly
a third of the 256-client throughput; changing only the purge batch did not recover it. Keep the
production defaults 300/1/0 and the shared Raft runtime. The LTO A/B was within the host's
run-to-run spread and is not adopted.

Caveat: all four voters share one process and host. The 1,024/4,096 rows in this section include
TCP connection setup; the pre-connected sweep sets the real plateau at 512–1,024 in flight.

### Write admission control (2026-11-09)

Each node has a bounded in-flight gate for `/write` and `/write_batch`:
`--max-inflight-writes` (env `MATCHENGINE_MAX_INFLIGHT_WRITES`) defaults to **1,024**; `0`
disables rejection while still collecting counters. Excess requests return `429 Too Many
Requests` instead of queueing behind Raft/apply. `GET /admission/status` reports the limit,
current in-flight, peak, admitted and rejected totals. The limit counts requests, not orders: a
128-order `/write_batch` consumes one slot.

#### Pre-connected concurrency sweep

The earlier sweep started the timer before TCP setup and used one shared client, so a run offering
1,024 clients often had only a few hundred requests truly in flight and the 1,024/4,096 rows were
dominated by connection establishment. The `--preconnect` mode establishes one keep-alive
connection per client before the barrier and consumes response bodies, so latency measures request
queueing and apply work rather than handshakes.

Four-node no-fsync cluster, median of 3-4 fresh rounds, 256-2,048 pre-connected clients. Cells are
**admitted writes/s / p99**; a number in parentheses is the median rejected count, and bold
marks the highest admitted throughput in the row. Rejected requests are fast `429`s and are
excluded from the admitted latency. This is an open-loop burst: rejected requests are not
retried, so production clients should back off and retry.

| Pre-connected clients | Limit 0 | Limit 256 | Limit 512 | Limit 1,024 |
|---:|---:|---:|---:|---:|
| 256 | 97.9k / 3.65 ms | 91.9k / 3.89 ms | 97.5k / 4.00 ms | 96.3k / 3.79 ms |
| 512 | 116.9k / 5.92 ms | 85.6k / 5.99 ms (499) | **114.5k / 5.76 ms** | 111.2k / 5.96 ms |
| 1,024 | 96.5k / 9.42 ms | 73.2k / 5.66 ms (512) | 78.6k / 6.87 ms (452) | **99.3k / 8.74 ms** |
| 2,048 | **107.4k / 17.46 ms** | 80.8k / 14.36 ms (884) | 91.2k / 13.39 ms (704) | 83.3k / 14.12 ms (637) |

At 256 clients every limit is within run-to-run noise. At 512 clients the 256 gate rejects about a
third of the burst and drops to 85.6k admitted writes/s; the 512 and 1,024 gates admit the whole
burst at 111-117k with p99 near 6 ms. At 1,024 clients the 1,024 gate is effectively unbounded
(99.3k, p99 8.74 ms), while the 512 gate rejects about 44% and falls to 78.6k. At 2,048 clients
the unbounded path is fastest (107.4k) but has the worst tail (17.46 ms); the 512 gate cuts p99 to
13.39 ms at 91.2k admitted writes/s, and the 1,024 gate gives 83.3k at 14.12 ms.

**Default 1,024.** It admits the measured 512-1,024 throughput plateau and still bounds runaway
concurrency. Use **512** when clients expect roughly 1,024 concurrent requests and a p99 near 7 ms
is worth the open-loop goodput loss; use **0** only when clients back off and retry and the tail
target is loose. The old 256 default was based on the connection-storm sweep and is too low for
512+ real clients.

At 4,096 pre-connected clients the client runtime (four workers) becomes the bottleneck, so those
tail numbers are not server-sizing data: one round each measured 108.3k/27.2 ms unbounded,
96.9k/27.5 ms at 1,024, 87.9k/28.2 ms at 512 and 78.7k/24.6 ms at 256. Use the 256-2,048 rows
above and re-run on deployment hardware.

`/write_batch` stayed below the gate: 100 concurrent 32-order batches (3,200 orders) measured
**309,180 orders/s** with p99 **10.31 ms** unbounded versus **303,609 orders/s** with p99
**10.49 ms** at limit 256. At the 1,024 default the same shape is admitted without rejection. The
gate counts requests, not orders, so batch-heavy deployments should size the limit against the
orders they are willing to have in flight (1,024 x 128 orders is a large cap). A rejected request
is a fast `429`, not a queued request; clients should retry with backoff (the Rust client treats
429 as retryable).

### Read-path benchmark: ReadIndex vs lease (2026-11-08)

`cargo bench --bench read_e2e -- --policy both` starts the same four-node no-fsync topology on a
server runtime and runs the read clients on a separate runtime. The cluster is seeded with 2,000
resting orders; the local read is `orderbook_depth:20`, `/events` returns a 100-event page, and the
mixed cases offer 2,000 writes/s. Each case runs 2,000 reads per reader after 100 warm-up reads.
The table reports the median of two runs per policy; the read-only rows were stable, while the
mixed rows depend on how many paced writes actually completed.

| Read path | Readers | ReadIndex reads/s | Lease reads/s | Throughput gain | ReadIndex p50 / p99 | Lease p50 / p99 | p50 / p99 gain |
|---|---:|---:|---:|---:|---:|---:|---:|
| `/linearizable_read` | 1 | 8,092 | 17,559 | 2.17x | 119.7 / 243.6 µs | 54.9 / 83.9 µs | 2.18x / 2.90x |
| `/linearizable_read` | 64 | 30,232 | 51,777 | 1.71x | 317.2 µs / 3.847 ms | 91.5 µs / 1.155 ms | 3.46x / 3.33x |
| `/events` | 1 | 5,100 | 7,461 | 1.46x | 191.7 / 352.1 µs | 131.7 / 194.3 µs | 1.46x / 1.81x |
| `/events` | 64 | 32,979 | 38,856 | 1.18x | 1.768 / 4.366 ms | 1.584 / 3.552 ms | 1.12x / 1.23x |

The local `/read` baseline was unchanged, as expected: about 20k reads/s at one reader and
50-59k at 64 readers, with run-to-run noise larger than any policy difference.

The mixed cases show the same direction, but with more variance because the writer competes with
the readers. With one linearizable reader, throughput rose from 7,469 to 17,973 reads/s (2.41x)
and p50 from 122.1 to 48.0 µs (2.54x). At 64 readers, the mixed linearizable p50 fell from 338.6
to 91.1 µs (3.72x) while throughput gain ranged from 1.55x to 2.28x. Mixed `/events` improved
1.52x at one reader and 1.17x at 64; p50 improved 1.56x and 1.13x.

Lease hits were **100%** in both runs: 2,000/2,000 at one reader and 128,000/128,000 at 64 readers
for both `/linearizable_read` and `/events`; there were no fallbacks. This cluster has all four
nodes on one host, so the lease remained fresh and the fallback branch was not exercised. The
counters are implemented and unit-tested; a real deployment with clock drift should exercise the
fallback and report it through `GET /read-policy/status`.

These gains are smaller than the upstream 90.4x/3.9x local-lease result because the original
alpha.37 `ReadPolicy::LeaseRead` baseline still went through RaftCore; the benchmark's
`enable_local_lease_read` local-cache path is newer. Stable `0.10.0` provides that API, and the
`local_lease` experiment below measured 11-36% higher linearizable-read throughput with
~40% lower 64-reader p50/p99. The original lease gain is still material: **2.17x** for one
linearizable reader, **1.71x** at 64 readers, with p50/p99 reduced by 2-3x. Local-lease reads are
now the default; set `--read-policy read_index` to force a fresh quorum round. The contract and
fallback are documented in `docs/API.md`.

### Local lease-read experiment (2026-11-10)

Stable OpenRaft 0.10 adds `Config::enable_local_lease_read` and
`Raft::get_local_read_linearizer`. With the config enabled, a valid caller-side lease snapshot
authorizes the read without entering RaftCore's API queue; a cache miss or stale lease falls back
to the core lease path. `--read-policy local_lease` is now the default; the core `lease` mode
remains available for comparison.

`cargo bench --bench read_e2e -- --policy lease|local_lease` on the same four-node no-fsync
cluster (2,000 reads/reader, 100 warm-up, depth-20 key, events page 100), two runs per policy:

| Read-only case | Core `lease` | `local_lease` | Change |
|---|---:|---:|---:|
| `/linearizable_read`, 1 reader | 17.5-20.1k reads/s | 21.1-27.4k | +20-36% |
| `/linearizable_read`, 1 reader p50 / p99 | 47.8-55.8 / 72.7-74.8 µs | 34.8-47.3 / 61.5-65.9 µs | p50 down 15-38% |
| `/linearizable_read`, 64 readers | 49.4-51.1k reads/s | 56.6-56.9k | +11-15% |
| `/linearizable_read`, 64 readers p50 / p99 | 88.4-92.1 µs / 1.17-1.19 ms | 53.5-53.8 µs / 0.64-0.69 ms | p50 ~40%, p99 ~42-45% lower |
| `/events`, 1 reader | 7.3-7.4k reads/s, p50 134-138 µs | 8.1-8.2k, p50 119-120 µs | +11%, p50 down ~13% |
| `/events`, 64 readers | 38.9-40.8k reads/s | 44.0-45.2k | +8-16% |
| `/events`, 64 readers p99 | 3.67-3.73 ms | 3.21-3.39 ms | down 8-14% |

The mixed read/write rows point the same way: linearizable reads at 64 readers measured
41.3-42.6k/s local vs 33.2-33.6k/s core, and `/events` at 64 readers 39.3-40.3k vs 35.0-37.6k.
Local `/read` is not routed through the policy and stayed in its normal range, as expected.

Write-side cost was within noise. A single-node 64-client sustained `/write` run measured
87.5/87.7k writes/s on core lease vs 86.3/87.6k on local lease (p50 ~709/710 µs; process CPU
7.30 vs 7.27-7.33 cores). A 4-node 256-preconnected-client burst measured 77.5/82.7k core vs
84.6/81.3k local, also within the host's round-to-round spread.

Caveats: the cache rebuilds the leader snapshot after notifications/command batches, so a
read-heavy leader may pay CPU that these mixed rows do not isolate; a local lease hit can overtake
an unfinished write, so clients must await the write response before a read that must observe it.
Client retries are at-least-once and should carry `client_order_id` for replicated dedupe;
downstream event consumers should resume from `(log_index, ordinal)` and dedupe on `event_id`.
The same bounded-clock-drift contract applies. The local path is now the default; the core
`lease` mode remains available for comparison and for deployments that prefer the core queue order.

### OpenRaft minimal benchmark comparison (2026-11-08)

The upstream [`openraft/benchmarks/minimal`](https://github.com/databendlabs/openraft/tree/main/benchmarks/minimal)
harness isolates OpenRaft: three nodes in one process, an in-memory log store and empty state
machine, a function-call network and empty requests. It is an overhead floor, not a capacity
number for this engine. The upstream README reports M1 Max/M3 Max numbers; ours are M4, so compare
directions rather than exact ratios.

| Upstream measurement | Result |
|---|---:|
| 1 client, single writes | 33,000 put/s |
| 64 clients | 912,000 put/s |
| 256 clients | 1,808,000 put/s |
| 1,024 clients | 3,006,000 put/s |
| 4,096 clients | 3,548,000 put/s |
| 4,096 clients, batch 4 | 5,615,000 put/s |
| Lease read, 1 reader | 100,805 → 9,110,300 reads/s (90.4x) |
| Lease read, 64 readers | 2,347,080 → 9,139,761 reads/s (3.9x) |

The one-client result is about 30µs per write; our 4-node no-fsync cluster measures 186.75µs p50
(5,039 writes/s), and the single-node no-fsync control 87.46µs (10,165 writes/s). The upstream
harness has no HTTP JSON, WAL, order book, event construction or real gRPC, so the gap is the
application path, not Raft itself. The 3.5M/s and 5.6M/s figures are not capacity targets.

What transfers:

- **Lease reads are the largest read-side lever.** Our `/linearizable_read` and `/events` use
  the configured read policy (`src/network/api.rs`); the gRPC `StreamEvents` stream anchors once
  per stream (`src/network/grpc/events.rs`), and follower reads fetch a leader linearizer through
  `src/network/management.rs`. Lease reads (the caller-side local cache by default) avoid the
  heartbeat round while the lease is valid; a stale lease falls back to `ReadIndex`, which pays the
  quorum round trip.
  The upstream benchmark reports 90.4x/3.9x for its local lease path. Stable
  `openraft 0.10.0` has `enable_local_lease_read`/`get_local_read_linearizer`, and the default
  `local_lease` mode measured 11-36% higher linearizable-read throughput with ~40% lower 64-reader
  p50/p99; the core `lease` path remains available. Use `--read-policy read_index` when clock
  drift is not bounded.
- **Batch and concurrency tuning still has headroom.** Our `/write_batch` 128-order path reaches
  227,319 orders/s versus 5,039 sequential writes/s, so application batching is already the main
  throughput lever. For independent clients, sweep client count and the server-side batch knobs:
  the upstream benchmark sets `max_payload_entries = 1,024` and `purge_batch_size = 1,024`; our
  defaults are 300 and 1, with `api_batch_linger_ms = 0`. The 2026-11-08 sweep tested those knobs
  and found no setting beat the defaults; the later pre-connected sweep put the write plateau at
  512-1,024 in flight, not 256. See the write-control section above.
- **Separate client and server runtimes.** The upstream harness runs clients on one runtime and
  servers on 16 workers. Our `cluster_e2e` currently runs the load generator and all four nodes on
  one runtime, so the published 5,039/35,152 numbers include driver contention. A separate client
  runtime or process is the next measurement fix; a four-process/four-host run is the deployment
  check.
- **Asynchronous append is the storage-side analogue.** The upstream `LogStore::append` inserts
  into memory, returns, and signals `IOFlushed` after a delayed flush; its test asserts entries are
  readable before the flush completes. The async-WAL prototype now implements the same contract
  behind the `async-wal` feature: a pending-entry cache, a background fsync, `IOFlushed` only
  when durable, and crash tests. It improved storage throughput 6.6x but did not improve Raft
  request throughput, so it remains off by default.
- **Instrumentation and build settings.** The upstream harness uses `runtime-stats`, flamegraph,
  tokio-console and a profile binary that measures startup memory, write allocations, latency and
  snapshot cost; it also builds with thin LTO and `codegen-units = 1`. The local thin-LTO A/B was
  within host spread and did not beat the default profile. `/metrics` now exposes
  `data_metrics`, `server_metrics` and (with `--features profiling`) the compact `runtime_stats`
  histograms; `cluster_e2e --help` documents the flamegraph and tokio-console setup. The
  remaining step is to record the before/after benchmark rows with those histograms enabled
  (`TODO.md` §16).
- **Follower-read cache is a later step.** The leader's lease read makes `/get_linearizer`
  cheaper, but a follower still pays one leader HTTP hop per read. A short-TTL linearizer cache
  bounded by the leader lease could remove it; it needs a safety analysis and is not a first move.

Do not copy the upstream in-memory store, function-call network, disabled heartbeats/elections,
200ms election timeout or empty-request capacity numbers into production. The harness is for
attribution, not deployment sizing.

### OpenRaft quick wins (2026-11-06)

The 2026-11-06 rerun includes the P1 OpenRaft utilization changes:
`replication_lag_threshold = 150_000`, `heartbeat_min_interval = 500 ms`,
server-side 1 MiB HTTP/2 stream / 4 MiB connection windows with 5 s / 3 s keepalive,
snapshot chunks from `RPCOption::snapshot_chunk_size()` (3 MiB default,
8 MiB cap), and `/ready` gated on `Raft::wait_for_recovery`. The client factory now
selects explicit replication (1 MiB / 4 MiB, 5 s / 3 s), heartbeat (256 KiB / 1 MiB,
5 s / 3 s) and snapshot (1 MiB / 4 MiB, 5 s connect, 10 s / 5 s keepalive) profiles.
The e2e and storage tables above are the post-change baseline. A dedicated
heartbeat-stream-count and snapshot-transfer-time measurement is still a P2
follow-up (`TODO.md` §16).

### Runtime flavor A/B (2026-10-31)

The node binary now uses a multi-threaded Tokio runtime; the previous
`#[actix_web::main]` setup used actix-rt's current-thread runtime. To measure the
runtime flavor alone, `cargo bench --bench e2e` was run with `-- current` and with
the default multi-thread mode in alternating fresh processes (four runs each, medians). The HTTP
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

### Pinned-core pass (2026-11-04)

The external optimization notes (below) reduce scheduler noise by pinning the benchmark to one
core. The e2e bench now accepts `--pin <cpu>`: on Linux it calls `sched_setaffinity` before the
runtimes start, so threads created afterwards inherit the mask (equivalent to
`taskset -c <cpu> cargo bench --bench e2e`). On macOS and the BSDs it fails loudly, because those
hosts expose no process core-pinning API.

This host is macOS, so the recorded pass uses the closest available approximation,
`cargo bench --bench e2e -- current`: one Tokio worker hosts RaftCore, apply, gRPC and the snapshot
timer, while Actix keeps its separate HTTP worker threads.

| Workload | Multi-thread (unpinned) | Current-thread (approximation) |
|---|---:|---:|
| Sequential `/write` | 256 writes/s | 258 writes/s |
| Sequential `/write` p50 / p99 | 3.93 ms / 5.10 ms | 3.95 ms / 5.22 ms |
| Concurrent `/write` (1,000 in flight) | 19,651 writes/s | 18,186 writes/s |
| Concurrent batches (100 x 32) | 156,034 orders/s | 140,068 orders/s |

The two runs are within the 5-10% run-to-run noise, so removing Tokio's multi-thread scheduler did
not move the tail: p99 stays ~1.3x p50 and the p50 stays at the WAL `sync_data` cost. A true
pinned single-core run remains a Linux-only measurement; run
`cargo bench --bench e2e -- --pin 0` there when a Linux host is available.

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

The idle heartbeat adds at most one diagnostic `STATUS` per second when no entry
is applied (`--market-data-aeron-heartbeat-ms`, default 1000; `0` disables). It
runs on the off-thread processor and does not touch `apply`.

## WebSocket fan-out (2026-11-10)

`cargo bench --manifest-path demos/market_data_system/Cargo.toml --bench ws_fanout --
--clients N --updates M --rate R` starts the reference service on a channel source,
connects N loopback WebSocket clients to `@bookTicker`, and publishes M BBO updates at R/s.
The hub serializes each payload once and fans the same `Bytes` out to every connection.

| Clients | Updates | Offered | Delivered frames | Per client | Aggregate |
|---:|---:|---:|---:|---:|---:|
| 16 | 2,000 | 19,790/s | 15,973 | 998.3 | 158,052 frames/s |
| 64 | 5,000 | 4,996/s | 110,649 | 1,728.9 | 110,571 frames/s |
| 64 | 5,000 | 19,997/s | 46,560 | 727.5 | 186,217 frames/s |

`@bookTicker` is a latest-value stream, so a slow connection intentionally drops older
updates in its forward task; delivered frames are the effective fan-out, not a promise that
every offered update reaches every client. The service remains bounded by the per-connection
queue and closes a consumer with `1013` when it falls too far behind. These are local
macOS/loopback numbers and exclude Aeron ingestion and JSON encoding changes under load.

### Transport observability (2026-11-10)

`GET /metrics` now includes `transport_metrics` (also embedded in
`GET /console/api/node` as `transport`):

- `heartbeat_streams` / `heartbeat_requests` and `replication_streams` /
  `replication_requests` count the gRPC `AppendStream` sessions and requests opened by this process.
- `snapshot_sent` / `snapshot_received`, bytes, average and max microseconds count every completed
  snapshot send and receive/install.

Measured on the reference host:

| Scenario | Heartbeat streams / reqs | Replication streams / reqs | Snapshot |
|---|---:|---:|---:|
| 4-node startup + membership | 2 / 2 | 6 / 12 | — |
| 64-client sustained write sweep, `--snapshot-logs 200` | 0 / 0 | 8 / 640 | — |
| Snapshot install test (tiny 4,104-byte snapshot) | — | — | leader sent 2, avg 34.4 ms, max 39.9 ms; learner received 2, avg 33.7 ms, max 39.3 ms |

The zero heartbeat streams under sustained replication are the intended
suppression: `heartbeat_min_interval` avoids opening a heartbeat stream while a
replication response was just acknowledged. Startup/idle periods still show the
heartbeat streams. Counts are process-local and reset on restart; the snapshot
numbers use a deliberately tiny book and are a measurement sanity check, not a
capacity result.

## Storage benchmark

`cargo bench --bench storage` writes 20,000 real `MatchEngineRequest::place`
entries in batches of 100, with 512 KiB segments:

| Operation | Result |
|---|---:|
| Append 20k entries | 768.0 ms |
| Append throughput | 26,043 entries/s |
| Read 20k entries | 7.65 ms |
| Read throughput | 2,614,579 entries/s |
| Reopen/recover 20k entries | 6.78 ms |
| Recovered entries | 20,000 |

The 2026-11-08 rerun reports the median of five runs with fsync every append.
Append throughput is fsync-bound in that mode (one durable batch per 100 entries); read and
recovery benefit from sequential segment scanning and coalesced `pread` calls. The A/B below
covers no-fsync and periodic modes.

### WAL fsync A/B (2026-11-07)

WAL fsync is controlled by `RAFT_WAL_FSYNC_MS`:

- unset: no fsync (the default); a crash can lose acknowledged writes.
- `0`: fsync every append batch.
- `N > 0`: a Tokio `wal-fsync` task fsyncs the active segment every `N` ms. It clones the active
  file handle under a short lock and fsyncs the clone on `spawn_blocking`, so appends do not wait
  on the disk lock during the fsync. This bounds the loss window to roughly `N` ms; it does not
  make every acknowledged write durable.

The numbers below use the default shared runtime (`RAFT_CORE_WORKERS` unset). The single-node
bench uses `StorageConfig::new` (`min_voters = 1`), so it measures raw storage performance without
the production four-voter guard. When `RAFT_WAL_FSYNC_MS` is unset, the binary requires at least
four voters; see [`docs/STORAGE.md`](./STORAGE.md) for the durability and peer-recovery contract.
The table reports the median of two runs (2026-11-07); ranges that matter are called out below.

| Workload | every append (`0`) | every 10 ms | every 100 ms | disabled (unset) |
|---|---:|---:|---:|---:|
| e2e sequential `/write` p50 | 3.898 ms | 88.2 µs | 101.1 µs | 87.9 µs |
| e2e sequential `/write` p99 | 4.979 ms | 2.639 ms | 219.1 µs | 270.8 µs |
| e2e sequential throughput | 262.5 writes/s | 7,374 writes/s | 9,495 writes/s | 9,648 writes/s |
| e2e concurrent 1,000 writes | 17.9k writes/s | 34.4k writes/s | 32.8k writes/s | 33.5k writes/s |
| e2e concurrent 100×32 batch | 180.2k orders/s | 221.7k orders/s | 218.7k orders/s | 230.5k orders/s |
| WAL append, 20k entries | 25.9k entries/s | 742k entries/s | 1.43M entries/s | 2.15M entries/s |
| WAL read, 20k entries | 2.84M entries/s | 6.98M entries/s | 7.36M entries/s | 5.66M entries/s |
| WAL reopen, 20k entries | 6.87 ms | 2.94 ms | 2.86 ms | 3.64 ms |

The every-append concurrent-batch row is the noisiest (two runs: 143.7k and 216.7k orders/s);
the 10 ms and 100 ms rows were 216.8-226.7k and 217.9-219.5k. The storage bench is short enough
that a 100 ms interval may not tick before its append phase ends, and the disabled-mode storage
read/reopen numbers are page-cache sensitive.

#### Compared with the 2026-11-06 matrix

| Metric | Previous | 2026-11-07 | Change |
|---|---:|---:|---:|
| every-append sequential p50 | 3.93 ms | 3.898 ms | -0.8% |
| every-append sequential throughput | 267 writes/s | 262.5 writes/s | -1.7% |
| every-append concurrent writes | 18.3k writes/s | 17.9k writes/s | -2.2% |
| 10 ms sequential p50 | 98.5 µs | 88.2 µs | -10.5% |
| 10 ms sequential throughput | 7,459 writes/s | 7,374 writes/s | -1.1% |
| 10 ms concurrent writes | 32.5k writes/s | 34.4k writes/s | +5.8% |
| 100 ms sequential p50 | 84.0 µs | 101.1 µs | +20.4% |
| 100 ms sequential throughput | 9,938 writes/s | 9,495 writes/s | -4.5% |
| disabled sequential p50 | 90.9 µs | 87.9 µs | -3.3% |
| disabled sequential throughput | 8,586 writes/s | 9,648 writes/s | +12.4% |
| WAL append, every append | 26.2k entries/s | 25.9k entries/s | -1.1% |
| WAL append, 10 ms | 815k entries/s | 742k entries/s | -9.0% |
| WAL append, 100 ms | 1.33M entries/s | 1.43M entries/s | +7.5% |
| WAL append, disabled | 1.66M entries/s | 2.15M entries/s | noisy (runs 1.20-3.10M) |

Every-append throughput/latency moved by about 2%, and disabled throughput was 8.0-11.2k
writes/s versus 8.6k previously, so the Tokio fsync worker did not regress the normal paths. The
100 ms p50 moved from 84 to 101 µs, but the previous value sat below both new runs (100.7 and
101.5 µs) and is within this host's run-to-run spread. The one clear change is the **periodic 10 ms tail**: p99 is 2.24-3.04 ms in the new runs.
The previous matrix did not record p99, but an earlier single run measured ~0.30 ms. The first
Tokio implementation held the disk lock during `sync_data` and produced ~4.0 ms p99; cloning the
FD under a short lock reduced that to 2.2-3.0 ms, but filesystem-level write contention remains.
If a multi-millisecond periodic tail matters, use a larger interval or accept the trade-off.

Every-append fsync accounts for about **97%** of sequential write latency; 10 ms periodic fsync
costs about 24% sequential throughput versus disabled in the median (7,374 vs 9,648 writes/s),
while keeping p50 essentially flat. Concurrent writes improve ~1.9x from every-append to the
non-blocking modes because fsyncs overlap across concurrent requests. Read and reopen gains are
partly page-cache residency; the append comparison is the direct measure. With the dedicated
runtime (`RAFT_CORE_WORKERS=2`) and no fsync, the e2e run measured 9,584 writes/s, p50 97.8 µs and
32,416 concurrent writes/s, so the fsync share dominates regardless of runtime topology.

### Async WAL prototype (2026-11-08)

The feature-gated async WAL prototype writes the WAL batch and updates the visible log, then
defers `sync_data` to a Tokio `async-wal` worker; Openraft's `IOFlushed` callback fires only after
the fsync. The storage benchmark queues all 200 batches before awaiting their callbacks, modelling
Raft pipelining. Five runs per mode, 20,000 real order entries in 100-entry batches, 512 KiB
segments:

| Mode | Append throughput | Range |
|---|---:|---:|
| Every append, synchronous | 26,043 entries/s | 25,839-26,314 |
| Every append, async prototype | **172,374 entries/s** | 65,176-192,160 |

The async median is about **6.6x** the synchronous median, but the range is wide: one run only
reached 65.2k while four reached 168k-192k. The worker's fsync batching and filesystem timing
dominate. Read/reopen numbers were similar. This is a storage-only measurement; the
Raft-level A/B follows.

A single-node Raft-level A/B used the same `e2e` suite on the multi-thread runtime, every-append
mode, three runs per configuration. Sequential writes stayed at ~258-260 writes/s in both modes
because each request still waits for `IOFlushed`. Concurrent 1,000-write throughput was 18,344/s
synchronous versus 17,277/s async (median), and 100 x 32 concurrent batches were 152,833 versus
157,414 orders/s. Those differences are within the ~10% run-to-run noise: the async path produced
**no consistent request-path gain** on this single-node macOS host. The storage result comes from
queueing 200 append batches before awaiting callbacks; concurrent Raft requests already overlap
synchronous fsync calls, so operating-system group commit captures most of the available batching.

The prototype is off by default and only supports every-append mode. `tests/cluster/test_storage.rs`
proves the callback waits for fsync, entries are readable before flush, a lost unsynced tail
recovers to the valid prefix, and the log remains appendable; `tests/cluster/test_async_wal.rs`
proves a Raft node commits and reopens with the async path. Productionizing still needs a Linux
multi-node A/B, periodic-mode support, broader crash/failover coverage, and a design that exposes
more append pipelining than the current request path offers.

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

### 2026-11-10 storage/transport hygiene

- **WAL byte cap.** `--max-wal-append-bytes` (default 8 MiB, `0` disables) splits one openraft
  apply batch into whole-record chunks under one disk lock and one fsync. A failed chunk rolls the
  active segment back and removes segments created by the batch; the index is updated only after
  every chunk is written.
- **Shared snapshot body.** `StoredSnapshot::data` and `SnapshotData::Memory` hold `Arc<[u8]>`;
  `build_snapshot` and `get_current_snapshot` share the encoded body instead of copying it.
- **Streamed snapshot receive.** `InstallSnapshot` writes chunks to a `snapshot-staging-*` file
  with a running CRC and the 256 MiB cap. The state machine verifies length/CRC/postcard before the
  atomic durable write; staging files are removed on success, failure and startup.
- **OpenRaft version.** `openraft` is on stable `0.10.0` (upgraded from `0.10.0-alpha.37` on
  2026-11-10). The network API returns `AppendResult`; the gRPC transport converts to the existing
  serializable payloads. Stable 0.10 also adds `enable_local_lease_read`/
  `get_local_read_linearizer`, which is an unmeasured read-path follow-up.

The default 8 MiB cap does not split the 100-entry `storage` benchmark batches; its append rate
stayed within the existing run-to-run spread (1.26M-3.30M entries/s over two local runs versus the
2.15M baseline). The cap is a latency/memory bound for unusually large merged appends, not a
change to the common path.

## Thread and task audit: optimization opportunities

This audit looks at where work runs, not just how much work there is. The goal is
to keep the Raft core and the state-machine apply task responsive; matching
itself is already cheap.

### Runtime topology

- `src/main.rs` uses `#[tokio::main(flavor = "multi_thread")]`, so the node's
  background tasks run on a **multi-threaded Tokio runtime**.
- `build_match_engine_node` uses `Raft::new` on the main runtime by default.
  When `RAFT_CORE_WORKERS` is positive it calls `Raft::build`, then starts
  OpenRaft with `Raft::run` on a **dedicated multi-threaded runtime**
  (`RaftThread`, thread name `raft-worker`, that many workers). RaftCore and the
  state-machine worker live there. The tonic gRPC server, the snapshot timer,
  the market-data watcher and processor, and the HTTP server future stay on the
  main runtime.
- Actix HTTP handlers run on the HTTP worker threads (one current-thread runtime
  per worker), not on the main runtime. They reach the state machine through the
  async mutex and Raft through its command channel.
- WAL append/read/recovery, startup WAL/snapshot open and snapshot file writes
  use `spawn_blocking`, so their disk I/O does not stall the main runtime.
  Raft's WAL work uses the dedicated runtime's blocking pool; HTTP/feed work
  uses the main runtime's pool.
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
  services. Set `RAFT_CORE_WORKERS` >0 to give OpenRaft its own runtime so
  RaftCore and the state-machine worker are not scheduled behind HTTP, gRPC or
  feed tasks; the rest of the node stays on the main runtime. Unset or `0`
  keeps Raft on the shared runtime.
- Use `spawn_blocking` only for work that cannot yield: blocking file I/O,
  synchronous `std::fs`, and CPU-heavy encode/decode. It is Tokio's blocking
  pool, not a manually managed OS thread; the runtime bounds and reuses those
  threads.
- Prefer async I/O (`tokio::fs`, `tokio::io::BufWriter`) inside a spawned task
  for sinks, so the task yields instead of holding a worker.

### Work on the state-machine path

| Work | Current location | Cost | Can be offloaded? |
|---|---|---|---|
| Price-time matching | `apply_one`, on the dedicated Raft runtime, under the lock | CPU, short | No; keep it on apply |
| Event construction and event-window append | `apply_one`, on the dedicated Raft runtime, under the lock | CPU, bounded | No; part of apply |
| Match-result sink (`--match-event-log`) | `publish_pending`, after the lock; blocking sinks run on `spawn_blocking` | JSON + file write + flush | Offloaded; a dedicated writer task is still optional |
| Market-data frame construction | `take_market_frame`, under the lock | event clone + journal drain | Sent after the lock; gate checked first |
| Duplicate `client_order_id` response | `submission_events`, under the lock | binary search + range copy | Implemented |
| Snapshot capture (`to_content`) | snapshot task, under the lock | full book/event copy | Hard (COW) |
| Snapshot encode | `build_snapshot` / `persist_snapshot`, `spawn_blocking` | CPU, tens of MB | Implemented |
| Snapshot decode | `install_snapshot`, `spawn_blocking` | CPU, tens of MB | Implemented |
| Snapshot bytes copy | `build_snapshot` / `get_current_snapshot`, under the lock | full-body memcpy | Yes (`Arc<[u8]>`) |
| Read JSON (`/read orderbook_orders`) | `read_state_owned`, lock; `into_json` after | clone + JSON of the whole book | Implemented |
| Market-data processing (`--market-data-log`) | `MarketDataProcessor` task, runtime | shadow book + wire encode + JSONL flush | JSONL uses a Tokio writer task (`tokio::fs`); Aeron uses a `spawn_blocking` publisher task because its handles are `!Send` |
| gRPC decode/encode and snapshot assembly | tonic tasks, runtime | message-size dependent | Yes, on the multi-thread runtime |
| WAL append/read | `LogStore`, `spawn_blocking` | disk I/O | Already offloaded |

### Tokio blocking audit (2026-11-07)

Migrated off async workers:

- `FileLogSink` opts into `MatchResultSink::is_blocking()`, so `publish_match_events` runs it on
  `spawn_blocking` instead of a Tokio worker.
- `JsonlSink` serializes each message and `try_send`s it to a Tokio writer task using
  `tokio::fs::File` + `tokio::io::BufWriter`; the processor no longer blocks on file I/O.
- `events_bridge` uses `tokio::fs` for the JSONL writer and cursor checkpoint, including
  `sync_data().await`.
- The debug snapshot-install marker uses `tokio::fs::write`.

Kept intentionally:

- `RaftThread` (dedicated Raft runtime) owns affinity-sensitive work; the `wal-fsync` Tokio task
  routes its blocking `sync_data` through `spawn_blocking`; the Aeron publisher runs in a
  `spawn_blocking` task because rusteron's handles are `!Send` and its API is blocking.
- `LogStore` uses `std::sync::RwLock` for the disk, index and runtime state, but only inside
  `spawn_blocking` closures and the fsync task's blocking sync. Tokio locks cannot be held in that
  blocking code.
- The remaining `std::sync::Mutex` guards are short and never cross an `.await`: client leader
  target, console action history, gRPC stream decode error, market-data pending snapshot and
  `MemorySink`. `FileLogSink`'s writer mutex is held during its blocking writes, but those run on
  the blocking pool, not on a Tokio worker. Async mutexes would add overhead without removing a
  stall.
- The state machine uses `futures::lock::Mutex`, which is already async; the WAL append order
  uses `tokio::sync::Mutex`.

### Dedicated Raft runtime (2026-11-06)

OpenRaft's state-machine worker is a `tokio::spawn` task. On the shared node runtime, `apply`
migrated across worker threads during one e2e run (observed ThreadIds 4, 9, 10 and 11), so it
competed with HTTP, gRPC and feed tasks for a worker. When `RAFT_CORE_WORKERS` is positive,
`build_match_engine_node` builds Raft with `Raft::build` and calls `Raft::run` on a dedicated
multi-threaded runtime (`RaftThread`, thread name `raft-worker`) with that many workers. RaftCore
and the state-machine worker stay on this runtime; the gRPC server, HTTP server, snapshot timer
and feed remain on the main runtime. Unset or `0` keeps the shared-runtime behavior.

Single-node e2e A/B (two alternating pairs, `cargo bench --bench e2e`; each cell is one pair):

| Workload | Shared node runtime | Dedicated Raft runtime (2 workers) |
|---|---:|---:|
| Sequential `/write` | 269 / 247 writes/s | 251 / 262 writes/s |
| Sequential p50 | 3.937 / 3.967 ms | 3.979 / 3.928 ms |
| Sequential p99 | 4.251 / 7.202 ms | 7.979 / 5.140 ms |
| Concurrent 1,000 `/write` | 17,817 / 19,218 writes/s | 17,001 / 14,834 writes/s |
| Concurrent 100×32 batch | 148,757 / 154,644 orders/s | 145,499 / 131,136 orders/s |

A clean dedicated-runtime run measured 266 writes/s, p50 3.945 ms / p99 4.617 ms, 18,028
concurrent writes/s and 138,183 concurrent-batch orders/s. The host was noisy (the shared
runtime's p99 also ranged 4.25-7.20 ms), so the throughput ranges overlap; the dedicated runtime
shows **no consistent throughput win** in this fsync-bound single-node test. Its benefit is
isolation: RaftCore and apply no longer share workers with HTTP, gRPC and the feed. An earlier
prototype run saw 19.3-20.2k concurrent writes/s, but the final implementation did not reproduce
that consistently.

The dedicated runtime does **not** pin apply to one thread: with 2 workers, apply still moved
between `raft-worker` threads. A single-worker variant keeps apply on one thread but has never
shown a consistent win. The 2026-11-10 rerun used two fresh four-node rounds per config:

| Run | 256 pre-connected clients | 1,024 burst clients (no preconnect) |
|---|---:|---:|
| Shared node runtime | 75,416 writes/s, p50 3.20 ms, p99 5.19 ms | 40,449 writes/s, p50 16.86 ms, p99 18.68 ms |
| `RAFT_CORE_WORKERS=1` | 76,990 writes/s, p50 3.13 ms, p99 5.18 ms | 41,221 writes/s, p50 16.34 ms, p99 19.16 ms |

The 256-client row is a two-round median and the 1,024 row is a three-round median. Both are within
run noise (2% throughput; p99 within ~3%), and the earlier sustained sweeps found the same
direction. Strict single-thread affinity therefore does not justify the extra runtime or an apply
actor: keep `RAFT_CORE_WORKERS` unset (shared) by default, and use a dedicated runtime only for
isolation or when a deployment measures a benefit. A batched apply actor remains an option only
with a new measurement showing a tail win.

### Prioritized opportunities

The 2026-11-08 sweeps and prototype identified concurrency/admission control and async-WAL
productionization as the next levers; admission control is now implemented (2026-11-09), and
async-WAL productionization remains. See
[Write concurrency and batching sweep](#write-concurrency-and-batching-sweep-2026-11-08),
[Read-path benchmark](#read-path-benchmark-readindex-vs-lease-2026-11-08),
[Async WAL prototype](#async-wal-prototype-2026-11-08) and
[OpenRaft minimal benchmark comparison](#openraft-minimal-benchmark-comparison-2026-11-08).

1. **Run the node on a multi-threaded Tokio runtime, with OpenRaft isolated (P0, implemented).**
   `src/main.rs` now uses `#[tokio::main(flavor = "multi_thread")]` instead of
   `#[actix_web::main]`. `actix-server` detects the Tokio runtime and starts its
   HTTP workers from it; the integration tests already exercised this path.
   The main runtime runs gRPC, the snapshot timer, the feed and HTTP. When
   `RAFT_CORE_WORKERS` is positive, OpenRaft's RaftCore and state-machine worker
   run on a dedicated runtime with that many workers (2 is the tested worker count);
   unset or `0` keeps the shared runtime. The 2026-11-06 A/B shows sequential
   latency unchanged and concurrent writes within the host's noise
   (17.0/14.8k/s dedicated vs 17.8/19.2k/s shared in the final pairs; an earlier
   prototype saw 19.3-20.2k/s). The benefit is isolation, not a measured
   throughput win. Items 2-9 are implemented; items 10-11 remain open.

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
   and `try_send`s the tap frame. `FileLogSink` is marked `is_blocking()`, so
   `publish_match_events` runs it on `spawn_blocking`; the apply task still
   awaits completion, so a slow sink delays the next entry but no longer blocks
   a Tokio worker. A dedicated writer task remains optional.

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

8. **Make the market-data task yield (P1, implemented 2026-11-07).**
   `JsonlSink` now serializes each message and `try_send`s it to a Tokio writer
   task that uses `tokio::fs::File` + `tokio::io::BufWriter` and flushes
   asynchronously. The processor task no longer performs blocking file I/O;
   Aeron runs in a `spawn_blocking` task because its handles are `!Send` and
   the API is blocking.

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
- Do not swap in an arena allocator (`bumpalo` or similar) expecting a win. The external
  optimization notes measured it 79-88% worse for single-operation latency and removed it again; see
  the external review section above.
- Do not compare this engine's replicated, fsync-bound write throughput against a single-node book
  microbenchmark from another project. Compare book-to-book or e2e-to-e2e.

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

## External optimization notes: what transfers here (2026-11-04)

Re-read AsthaMishra/matching-engine's `matching-core/optimization_notes.md` (last updated
26-05-2026) against this engine. It documents nine book-layout and runtime optimizations with
before/after numbers, measured on a single-node, cent-tick, `$0.01-$999.99` book of 100,000 price
slots. This engine is Raft-replicated, one symbol per process, 18-decimal `Fixed`, and its
end-to-end write path is fsync-dominated (~4 ms), while book-only work runs at ~10M fills/s
(`book_replay` above). Most of their wins target a bottleneck this engine does not currently have.
The value in the document is selective: one layout idea is already prototyped here, several are
already covered by design, two are traps, and the measurement discipline transfers wholesale.

| Their optimization | Their measurement | Our status | Verdict |
|---|---|---|---|
| Flat array + bitmap instead of `BTreeMap` | 7.8x-318x at depth 100-1000 | Prototyped as per-level buckets (`level_book`): 2.00x sweep, 3.27x deep cancel, 2.28x/2.74x amend, 6.09x BBO query, 1.04x BBO advance; 0.88x sparse amend and 0.94x depth | Do not adopt wholesale: the sparse case still loses, the prototype is plain-limit only, and no 3-node/real-sink run exists |
| Cached best-bid/ask index | `bbo` 354 ns -> 0.88 ns | BBO already reads the tree edges: ~3.0 ms per 1M queries; the prototype's cached BBO is 6.1x against that and 1.04x on advance | Skip the production change; the prototype keeps the cache only |
| `Vec` order index keyed by per-book sequential ids | 1.7-1.8x after removing SipHash | No secondary index exists here: a resting order is identified by `(side, price, sequence)`, which is also its tree key, and the sequence is dense and per book by construction | Already better; no action |
| Store the index-in-level so cancel needs no intra-level scan | Their stated next win | No intra-level `Vec` to scan; cancel is one tree remove | Already better; a prerequisite only if the bucket layout lands |
| `active` flag + `head_idx` instead of `retain()`/`pop_front()` | Qualitative | A partially filled maker is updated in place, so only fully consumed makers are removed; the prototype's `head` cursor is the analogue (1.99x sweep) | Part of the layout item; nothing to do today |
| Bitmap `top_n_levels` (`trailing_zeros`/`leading_zeros`) | 53 µs -> 794 ns | The depth walk already visits active levels only and is capped at 200; prototype depth was 0.94x | Skip - empty slots are never visited |
| Arena allocator (`bumpalo`) | **Regression**: single-operation latency 79-88% worse; removed again in their Optimization 9 | Inline buffers are the adopted approach (`apply_path`: 3.17/3.00/4.00 allocs/entry for rest/cancel/fill after the response inline change) | Do not adopt; it would also complicate `to_content` and snapshot serialization |
| Pre-allocated collections (`with_capacity`, `next_power_of_two`) | Small, qualitative | Event window, per-entry event buffer, publish list and one response event are preallocated/inline; the fill `Vec`, legacy `value` `String` and retained event window still allocate | Done for response events; a fill `FillBuffer` was reverted after it slowed the book-only replay |
| Response slot pool replacing `oneshot::channel()` | Removes one allocation per request | One `/write` costs ~4 ms and is fsync-bound; their pool (256 slots) also caps in-flight requests, which conflicts with the ~19k concurrent writes already sustained here | Skip; not the bottleneck |
| N-thread sharding, one owner per symbol | Their 132M/s is multi-threaded (~33M/s per thread); remaining single-thread gap ~3x | One symbol per process is deliberate and apply must be serial for determinism; parallelism already exists across processes and across HTTP workers plus the blocking pool | Skip; keep the multi-thread caveat in mind when comparing numbers |
| SIMD slot scan, `mmap` | They judged SIMD marginal over the bitmap and mmap inapplicable | Agreed | Skip |

### Measurement discipline adopted (2026-11-04)

The measurement rules transferred regardless of layout and are now implemented:

- **Warm and cold are separate, and setup/drop are outside the timed window.** Their `iter_custom`
  lesson - dropping a 100,000-slot book inside the timer produced ~960 µs uniformly at every depth -
  is enforced by `benches/bench_support`: clone before `Instant::now()`, drop after the duration is
  recorded, and a 64 MiB eviction sweep between clone and op for the cold rows. The tables label
  warm and cold separately.
- **Percentiles, not just best-of-5.** `book_replay` and `level_book` report warm p50/p99 over every
  per-round sample and keep best-of-5 for continuity. The 1k-ask sweep's p99 is 2.0x its p50 while
  best-of-5 sits on the p50, which is exactly the tail a regression gate needs to see.
- **A realistic-mix replay is now implemented.** They treat ITCH replay as the only
  production-like signal because it carries a real add/cancel/modify mix. `cargo bench --bench
  replay_mix` replays a deterministic 100,000-operation add/cancel/amend/stop/iceberg/taker
  fixture with fees and reports warm/cold whole-replay percentiles plus per-operation p50/p99;
  see the realistic-mix section above. `tests/replay` remains correctness-only.
- **Scheduler noise is now measurable.** `cargo bench --bench e2e -- --pin <cpu>` pins the process on
  Linux; on this macOS host the current-thread run was the closest approximation and showed no
  change (see "Pinned-core pass"). A true pinned Linux run is still pending.
- **Compare like with like.** Their throughput is single-threaded book management with no
  durability; the concurrent ~156k orders/s measured here includes Raft commit, WAL `sync_data`,
  apply, events and JSON. Never quote the two side by side.

### Not applicable here

- Fixed tick and bounded price range (`MAX_PRICE / TICK_SIZE` slots): incompatible with 18-decimal
  `Fixed` and configurable `price_tick`/`base_lot`.
- Per-process symbol sharding: contradictory to one symbol per process/cluster.
- Removing durability for latency: `sync_data`, snapshots and replication are the product.

## Interpretation

- Matching and feed derivation are not the current bottleneck.
- The upstream OpenRaft minimal benchmark (in-memory, no network, empty state machine) runs at
  33k put/s with one client and 3.5M/s with 4,096, while our 4-node no-fsync cluster runs at
  5.0k sequential, 114.5k writes/s at 512 pre-connected in-flight and 99.3k at 1,024. Raft is not
  the current bottleneck;
  HTTP/JSON, WAL, apply and client concurrency dominate. Lease reads, write batching and the
  admission gate are implemented; the remaining levers are verifying lease fallback behavior on
  multi-host clocks and tuning the admission limit for the deployment's p99 target, not Raft or
  book-level work.
- Lease reads (the default) remove the fresh quorum round on leader reads and produced
  2.17x/1.71x throughput and 2.18x/3.46x p50 gains for `/linearizable_read` at 1/64 readers in the
  4-node loopback benchmark; `/events` gained 1.46x/1.18x. The default `local_lease` mode adds
  OpenRaft's caller-side cache and measured another 11-36% linearizable-read throughput with
  ~40% lower 64-reader p50/p99. Set `--read-policy read_index` when bounded clock drift is not
  guaranteed; the fallback path is unit-tested but should be monitored on multi-host clocks.
- With pre-connected clients the write path sustains 116.9k writes/s at 512 in flight (p99
  5.76 ms) and 99.3k at 1,024 (p99 8.74 ms); at 2,048 the unbounded path reaches 107.4k but p99
  rises to 17.46 ms. The earlier "knee at 256" was a TCP-setup artifact. The default 1,024 gate
  admits the measured plateau; 512 is the tighter-tail choice, and 0 disables rejection.
  `/write_batch` remains the throughput path; the default OpenRaft payload/purge/linger and
  runtime/LTO settings were not improved by the one-factor sweeps.
- The async WAL prototype raised every-append storage throughput from a 26.0k entries/s median to
  172k (range 65.2k-192.2k) while preserving the `IOFlushed` durability gate and crash recovery.
  A single-node e2e A/B found no consistent request-path gain: sequential writes still wait for
  durability, and concurrent writes were within noise. It remains off by default pending a Linux
  multi-node A/B and pipelining work.
- Sequential write latency is fsync-dominated; batching concurrent submissions
  improves throughput substantially.
- The feed has ample headroom at 500k fills/s with a null sink. A real sink
  (JSONL or Aeron) must be measured separately because it adds encoding and I/O.
- The realistic-mix replay sustains ~1.0M book operations/s warm (791 ns per-operation p50).
  Single-order operations stay at or below 2.9 µs p99; market takers are the widest tail (6.8 µs
  p99) because they sweep levels and elect stops. This is book-only load, not an end-to-end
  capacity number.
- Setting `RAFT_CORE_WORKERS` to a positive count isolates OpenRaft on its own runtime. With 2
  workers, sequential p50/p99 were unchanged and concurrent single writes stayed within the
  host's noise (17.0/14.8k/s dedicated vs 17.8/19.2k/s shared in the final pairs), so the win is
  isolation rather than throughput. Apply can still migrate between the two `raft-worker`
  threads; the single-worker variant gives strict affinity but the 2026-11-10 A/B was within
  noise (75.4-77.0k/s at 256 pre-connected clients; 40.4-41.2k/s at 1,024 burst clients), so it is
  not adopted as a throughput choice.
- The per-level bucket prototype wins match, deep-cancel, dense-amend and
  BBO-query workloads, is roughly parity on BBO advance, and loses sparse
  one-order-per-level re-queue and depth. Full adoption still needs a sparse-level fast path (or a
  hybrid) and a 3-node/real-sink run; see the prototype section above.
- The external optimization-notes review (2026-11-04) transfers measurement discipline, not
  layout: the per-level bucket prototype already covers the one applicable layout idea, the
  secondary-order-index and cached-BBO wins are already covered by the `(side, price, sequence)`
  tree key, and the arena/response-pool ideas are measured regressions or non-bottlenecks here.
- The runtime flavor A/B is within noise for the write path: WAL fsync and the
  serialized apply path dominate. P0 items 2-6 are implemented: snapshot
  encode/decode and read serialization are off the lock, apply locks per entry
  and publishes after release, and duplicate retries binary-search the event
  window. Snapshot byte sharing and optional sink offload remain open.

## Implementation notes (2026-11-06)

### Implemented choices

- Per-node write admission caps concurrent `/write` and `/write_batch` requests at 1,024 by
  default, returns `429` on overflow and exposes limit/in-flight/peak/admitted/rejected counters
  at `/admission/status`. Lower it to 512 for a tighter tail; 0 disables rejection.
- In every-append mode, one `sync_data` per replicated append batch (split into
  `--max-wal-append-bytes`-bounded writes under the same disk lock), so N entries in one
  AppendEntries RPC cost one fsync; the default no-fsync mode skips the `sync_data`.
- `pread` reads against cached file handles, so replication can read while an append is in flight.
- The order book, event window and dedupe map stay in memory; `apply` does no disk IO.
- gRPC uses bidirectional `AppendStream` for pipelined replication, separate replication,
  heartbeat and snapshot connections per peer, and tuned HTTP/2 flow-control windows/keepalive.
- Snapshots stream in OpenRaft-configured chunks (3 MiB default, 8 MiB cap); `log/state` and
  `snapshot/current.snap` are replaced atomically.
- The event window is bounded by `--max-events` and paged through `/events`; `/read
  orderbook_orders` does not clone it.
- WAL appends avoid per-entry clones, split writes at `--max-wal-append-bytes` under one disk lock and one fsync, recovery scans segments sequentially, contiguous log reads share one `pread`, snapshot encoding/writing runs outside the state-machine lock, and snapshot bodies stream to disk through an incremental CRC writer.
- A gRPC server failure is fatal to `run()`: HTTP is stopped, background tasks are joined and Raft is shut down before the error is returned. HTTP requests are logged once per request.

### Current limits and known gaps

- TLS/mTLS and API authentication are intentionally gateway responsibilities; this process is an internal service.
- Snapshot installation no longer buffers the received body in memory: chunks are staged to a
  `snapshot-staging-*` file under the snapshot directory, checked against the 256 MiB
  `MAX_SNAPSHOT_BYTES` limit, and verified by length/CRC before install. Decoding the postcard
  state still needs one contiguous body plus the in-memory state machine.
- One process/cluster trades one symbol; run one deployment per symbol to scale.

### Data format `Order` prices/quantities, `last_trade_price`, match events, depth/BBO/spread, the
JSONL feed and wire v7 use exact 18-scale fixed-point integers. Snapshot **v11** and wire **v7** are
breaking changes from earlier builds, so recreate data directories when upgrading. See
[`docs/DECIMAL.md`](./DECIMAL.md).

Known gaps and the roadmap are tracked in [`TODO.md`](../TODO.md).
