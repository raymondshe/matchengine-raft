# Performance measurements

Measured on **2026-10-30** (runtime flavor A/B and P0 offloads: **2026-10-31**; measurement scope and RSS: **2026-11-01**; book p50/p99 and the pinned-core approximation: **2026-11-04**; end-to-end/storage rerun with the OpenRaft quick wins: **2026-11-06**; realistic-mix replay: **2026-11-06**) on the reference development machine:

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
cargo bench --bench feed_load     # market-data feed at 50k/500k fills/s
cargo bench --bench storage       # WAL append/read/recovery
cargo bench --bench p0_bench -- duplicate|read|snapshot  # P0 lock/scan A/B
cargo bench --bench book_replay   # book-only sweeps, deep cancels and BBO advances
cargo bench --bench replay_mix    # realistic add/cancel/amend/stop/iceberg replay with per-op percentiles
cargo bench --bench level_book    # per-level bucket prototype vs the current book
cargo bench --bench apply_path    # apply-path heap allocations per applied entry
```

## Measurement scope

Each number below answers a different question. Do not compare one row to another without checking
its scope.

| Benchmark | Includes | Excludes | Load model | Timer / sampling | Variance |
|---|---|---|---|---|---|
| `matchengine` | Order-book operations: sweep, rest, partial fill, IOC cancel, iceberg refill, amend, cancel; optional depth journal | HTTP, Raft, WAL, snapshots, market-data encoding/sink | Closed-loop, in-process | `Instant`, best-of-5 per round | Best-of-5 only; not a tail study |
| `book_replay` | `OrderBook` only: 100-level and single-level sweeps, deep cancels, BBO advancement and static BBO queries; clone/eviction outside the timed window | HTTP, Raft, WAL/snapshots, market-data feed, event construction, fees | Closed-loop, in-process; one pristine book per round | `Instant` per round; best-of-5 repeats, warm p50/p99, interleaved cold p50/p99 after a 64 MiB eviction sweep | 5-14% run-to-run; cold adds ~8-22%; 1k/20k cases differ in tree depth, not just order count |
| `replay_mix` | Deterministic 100,000-operation `OrderBook` stream: limit adds, icebergs, parked stops, cancels, amendments and market takers, with maker/taker fees on every order; whole-replay and per-operation timings | HTTP, Raft, WAL/snapshots, market-data feed, event construction, symbol/tick validation | Closed-loop, in-process; fixture generated at startup, book rebuilt per round | `Instant` per operation for the per-kind table; 20 whole-replay warm rounds plus 20 interleaved cold rounds after a 64 MiB eviction sweep | Four runs; whole-replay warm p50 within 5%, per-operation p50 within ~30%; p99 and max are host-noise sensitive |
| `level_book` | Prototype per-level buckets plus the current `OrderBook`: match sweeps, deep cancels, sparse/dense amends, depth, BBO queries and BBO advancement; plain limit orders only; clone/eviction outside the timed window | Stops, icebergs, GTD expiry, validation, events, depth journal, market-data feed, fees beyond `MatchResult` construction | Closed-loop, in-process; paired current/prototype books | `Instant` per round; best-of-5 repeats, warm p50/p99, interleaved cold p50/p99 after a 64 MiB eviction sweep | 5-10% run-to-run, BBO-query ratio 4.9-5.6x; sparse one-order-per-level cases are the prototype's worst case |
| `apply_path` | `StateMachineStore::apply` only: blank, rest, cancel, fill and 128-request batch workloads; a counting global allocator reports heap calls and requested bytes | HTTP, Raft, WAL, snapshots, market-data tap, event-log reads, response serialization | Closed-loop, direct apply; 50k entries or 1k batches per workload | Counting allocator; per-entry averages | Requested bytes include realloc churn and are not a live-memory high-water mark |
| `e2e` `/write` | HTTP decode/validation, Raft proposal/commit, WAL append + `sync_data`, apply, response serialization | Cross-node replication, external client network beyond loopback | Sequential is closed-loop; concurrent is a 1,000-request burst | `Instant` per request; p50/p95/p99/max | Medians/percentiles; runtime A/B uses four alternating runs, the 2026-11-06 rerun uses three |
| `e2e` `/write_batch` | Same as `/write`, with 1–128 requests per Raft entry and one WAL fsync per entry | Same as `/write` | 200 sequential batches per size; 100 concurrent 32-order batches | `Instant` per batch | Medians of three runs per size; batch latency includes payload/apply growth |
| `feed_load` | Tokio tap, bounded queue, shadow book, wire encoding and processor drain with a null sink | JSONL/network sink I/O, Raft, HTTP | Offered-load generator at 50k/500k/2M fills/s | `Instant`; queue/drain counters | One run per target; no p99 table |
| `storage` | WAL append/read/reopen with 20k real order entries and 512 KiB segments | State machine, Raft, HTTP | Closed-loop, 100-entry batches | `Instant` | Three runs, medians; no tail study |
| `p0_bench duplicate` | State-machine duplicate-retry lookup over a 262k-event window | HTTP, Raft, WAL | 200 retries after one setup | `Instant`; mean/p50/p99/max | Single run; a single scheduling outlier moves the mean |
| `p0_bench read` | In-process node `/read orderbook_orders` with 100k orders and 8 concurrent writers | Cross-node replication; client-side parsing beyond bytes received | 2 readers x 10 large reads, writers in flight | `Instant`; read/write p50/p99/max | Single run; allocator/HTTP buffers affect peak RSS |
| `p0_bench snapshot` | `persist_snapshot` encode/write/fsync for 100k orders while 8 writers run | Raft snapshot shipping; cross-node replication | 8 writers, snapshot on the main benchmark task | `Instant`; writer p50/p99/max and snapshot duration | Single run; encode/fsync dominated |

Timer source is `std::time::Instant` throughout; there is no `rdtsc` or assumed CPU frequency.
Loopback HTTP and local disk are part of the measured path where stated. Run-to-run variance is
not a confidence interval; the e2e runtime A/B is the only comparison with four alternating runs.

The two book benches share `benches/bench_support`: each round clones the state before
`Instant::now()` and drops the mutated state after the duration is recorded, so setup and teardown
are outside the timer. "Warm" means the clone immediately precedes the timed operation; "cold"
means a 64 MiB read-modify-write sweep runs between the clone and the timed operation. Warm and
cold rounds are interleaved in one time window: running all warm rounds first biased the warm pass
(early samples carried clock ramp-up and the cold pass inherited a boosted clock), which showed up
as cold p50 beating warm p50 on this host.

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

## Per-level bucket prototype (2026-11-04)

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
| Match 1k asks / 100 levels | 84.8 µs | 46.1 µs | 1.84x | 1.82x |
| Match 20k asks / 1 level | 2.316 ms | 1.830 ms | 1.27x | 1.27x |
| Cancel 5k at depth / 50 levels | 262.4 µs | 79.3 µs | 3.31x | 3.21x |
| Amend volume 5k (same price) | 182.7 µs | 104.5 µs | 1.75x | 1.72x |
| Amend price 1k sparse (1 order/level) | 72.3 µs | 96.0 µs | 0.75x | 0.75x |
| Amend price 5k dense (50 orders/level) | 634.1 µs | 244.8 µs | 2.59x | 2.58x |
| Depth 10 levels, 100k queries | 6.408 ms | 6.660 ms | 0.96x | 0.96x |
| BBO query 1k levels, 1M calls | 2.233 ms | 0.456 ms | 4.90x | 5.18x |
| BBO advance 1k levels x 2 sides | 103.0 µs | 118.4 µs | 0.87x | 0.88x |

The prototype wins where a level is populated: a sweep consumes a contiguous vector and advances
`head` instead of removing every order from the tree, deep cancels amortize the level lookup, a
same-price reduction is an in-place update, a dense re-queue leaves the old level alive and cached
BBO reads are a couple of field loads. The losses are the sparse worst case: with one order per
level, the sequence lookup, level indirection and level creation/removal cost more than a single
`BTreeMap` remove/insert. BBO advancement has the same shape because every cancellation removes a
level. Depth is effectively unchanged.

The cold pass does not change the ranking: cancel is 303.4 µs cold for the current book vs 86.4 µs
for the prototype, sparse amend is 84.9 µs vs 128.5 µs, and BBO query is 2.808 ms vs 0.453 ms.
One case moves: BBO advancement is a warm loss (0.87x) but cold parity (115.2 µs vs 117.5 µs), so
the cached best index pays off mainly while the book is cache-resident. The layout conclusion is
therefore not an artifact of warm caches.

Full adoption is therefore **not** justified by this prototype; the P2 adoption item remains.
A follow-up would need a single-order-level fast path (or a hybrid that keeps tree keys for sparse
levels), a memory A/B in separate processes, and a 3-node or real-sink run. The benchmark is
closed-loop; repeated mutation runs vary by roughly 5-10%, while the BBO-query ratio moved between
4.90x and 5.57x across runs and the 1k match between 1.84x and 1.99x, so treat the ratios as
directional. The 2026-11-02 table quoted best-of-5 only; the numbers above are the same workloads
with warm p50 as the headline and best-of-5 kept for continuity.

## Apply-path allocation benchmark (2026-11-02)

`cargo bench --bench apply_path` drives `StateMachineStore::apply` directly with a counting global
allocator. It covers blank entries, one-event rest/cancel/fill entries, and 128-request batches;
the blank and batch-blank cases isolate the harness and response allocations. Counts are heap
allocation calls per applied entry; bytes are the sum of requested sizes **including realloc
churn**, not live memory.

Before this change, the per-entry event path allocated a temporary `Vec` for event construction,
another `Vec` for the request's events, a `Vec` for the response events and a `Vec` for the
per-entry publish list. The change writes fills/rests directly into an inline
`SmallVec<[MatchEvent; 4]>` and uses an inline publish list for the common single-request entry.
The retained event window and the public response still own `Vec`s.

| Workload | Before allocs/entry | After allocs/entry | Before bytes/entry | After bytes/entry |
|---|---:|---:|---:|---:|
| Blank | 0.00 | 0.00 | 0.0 | 0.0 |
| Rest, 1 event | 7.17 | 4.17 | 2,323 | 595 |
| Cancel, 1 event | 6.00 | 4.00 | 1,308 | 156 |
| Fill, 1 event | 8.00 | 5.00 | 3,308 | 2,012 |
| Batch blank, 128 requests | 1.02 | 1.02 | 381 | 381 |
| Batch rest, 128 x 1 event | 6.23 | 4.19 | 2,393 | 1,693 |

The single-entry rest/cancel/fill times were 0.61/0.30/0.40 µs before and 0.55/0.30/0.38 µs after;
these are within the benchmark's noise. The batch-rest time was 0.27 µs/entry both before and
after. The batch bytes also fell because the publish list is reserved once and the per-request
event buffers stay inline; an all-`PurgeExpired` batch avoids the reserve entirely.

Remaining allocations are the `BTreeMap` node on a rest, the response `events` `Vec`, the legacy
`value` sequence `String`, and the `MatchResult` `Vec` on a fill. Avoiding the response `Vec` would
change the public `MatchEngineResponse.events` type; avoiding the `MatchResult` `Vec` would change
`Outcome.fills`. The market-data tap path still copies events into `TapFrame.events` when the tap is
enabled, and is not covered by this no-tap benchmark.

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
the single-node e2e path is dominated by HTTP plus durable append.

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
a 3-node cluster adds one majority round trip per batch, but the WAL fsync
amortization is unchanged. Keep batches within the 128-request cap: `apply`
holds the state-machine lock for the whole batch.

### OpenRaft quick wins (2026-11-06)

The 2026-11-06 rerun includes the P1 OpenRaft utilization changes:
`replication_lag_threshold = 150_000`, `heartbeat_min_interval = 500 ms`,
1 MiB HTTP/2 stream / 4 MiB connection windows with 5 s / 3 s keepalive on both
sides, snapshot chunks from `RPCOption::snapshot_chunk_size()` (3 MiB default,
8 MiB cap), and `/ready` gated on `Raft::wait_for_recovery`. The e2e and storage
tables above are the post-change baseline. A dedicated heartbeat-stream-count
and snapshot-transfer-time measurement is still a P2 follow-up (`TODO.md` §16).

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

## Storage benchmark

`cargo bench --bench storage` writes 20,000 real `MatchEngineRequest::place`
entries in batches of 100, with 512 KiB segments:

| Operation | Result |
|---|---:|
| Append 20k entries | 782.9 ms |
| Append throughput | 25,547 entries/s |
| Read 20k entries | 8.31 ms |
| Read throughput | 2,408,176 entries/s |
| Reopen/recover 20k entries | 7.30 ms |
| Recovered entries | 20,000 |

The 2026-11-06 rerun reports the median of three runs. Append throughput is
fsync-bound (one durable batch per 100 entries); read and recovery benefit from
sequential segment scanning and coalesced `pread` calls.

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
| Flat array + bitmap instead of `BTreeMap` | 7.8x-318x at depth 100-1000 | Already prototyped as per-level buckets (`level_book`): 1.84x sweep, 3.31x deep cancel, 1.75x/2.59x amend, but 0.75x sparse amend and 0.87x BBO advance | Keep as the P2 hybrid with a sparse-level fast path; do not adopt wholesale (it needs a bounded cent price range) |
| Cached best-bid/ask index | `bbo` 354 ns -> 0.88 ns | BBO already reads the tree edges: 434M queries/s, ~2.3 ns per call; the prototype's cached BBO is 4.9x against that and loses on advance (0.87x) | Skip |
| `Vec` order index keyed by per-book sequential ids | 1.7-1.8x after removing SipHash | No secondary index exists here: a resting order is identified by `(side, price, sequence)`, which is also its tree key, and the sequence is dense and per book by construction | Already better; no action |
| Store the index-in-level so cancel needs no intra-level scan | Their stated next win | No intra-level `Vec` to scan; cancel is one tree remove | Already better; a prerequisite only if the bucket layout lands |
| `active` flag + `head_idx` instead of `retain()`/`pop_front()` | Qualitative | A partially filled maker is updated in place, so only fully consumed makers are removed; the prototype's `head` cursor is the analogue (1.84x sweep) | Part of the layout item; nothing to do today |
| Bitmap `top_n_levels` (`trailing_zeros`/`leading_zeros`) | 53 µs -> 794 ns | The depth walk already visits active levels only and is capped at 200; prototype depth was 0.96x | Skip - empty slots are never visited |
| Arena allocator (`bumpalo`) | **Regression**: single-operation latency 79-88% worse; removed again in their Optimization 9 | Inline buffers are the adopted approach (`apply_path`: 4-5 allocs/entry after the `SmallVec` change) | Do not adopt; it would also complicate `to_content` and snapshot serialization |
| Pre-allocated collections (`with_capacity`, `next_power_of_two`) | Small, qualitative | Event window, per-entry event buffer and publish list are preallocated/inline; the response `events` `Vec`, the legacy `value` `String` and the fill `MatchResult` `Vec` still allocate | P2 micro: reserve the known lengths where the shape is fixed |
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
- Sequential write latency is fsync-dominated; batching concurrent submissions
  improves throughput substantially.
- The feed has ample headroom at 500k fills/s with a null sink. A real sink
  (JSONL or Aeron) must be measured separately because it adds encoding and I/O.
- The realistic-mix replay sustains ~1.0M book operations/s warm (791 ns per-operation p50).
  Single-order operations stay at or below 2.9 µs p99; market takers are the widest tail (6.8 µs
  p99) because they sweep levels and elect stops. This is book-only load, not an end-to-end
  capacity number.
- The per-level bucket prototype wins match, deep-cancel, dense-amend and
  BBO-query workloads but loses sparse one-order-per-level re-queue and BBO
  advance. Full adoption still needs a sparse-level fast path (or a hybrid) and
  a 3-node/real-sink run; see the prototype section above.
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

- One `write_all` + one `sync_data` per replicated append batch, so N entries in one AppendEntries
  RPC cost one fsync.
- `pread` reads against cached file handles, so replication can read while an append is in flight.
- The order book, event window and dedupe map stay in memory; `apply` does no disk IO.
- gRPC uses bidirectional `AppendStream` for pipelined replication, separate replication,
  heartbeat and snapshot connections per peer, and tuned HTTP/2 flow-control windows/keepalive.
- Snapshots stream in OpenRaft-configured chunks (3 MiB default, 8 MiB cap); `log/state` and
  `snapshot/current.snap` are replaced atomically.
- The event window is bounded by `--max-events` and paged through `/events`; `/read
  orderbook_orders` does not clone it.
- WAL appends avoid per-entry clones, recovery scans segments sequentially, contiguous log reads share one `pread`, snapshot encoding/writing runs outside the state-machine lock, and snapshot bodies stream to disk through an incremental CRC writer.
- A gRPC server failure is fatal to `run()`: HTTP is stopped, background tasks are joined and Raft is shut down before the error is returned. HTTP requests are logged once per request.

### Current limits and known gaps

- TLS/mTLS and API authentication are intentionally gateway responsibilities; this process is an internal service.
- Snapshot installation needs one contiguous buffer, bounded by 256 MiB per snapshot.
- One process/cluster trades one symbol; run one deployment per symbol to scale.

### Data format `Order` prices/quantities, `last_trade_price`, match events, depth/BBO/spread, the
JSONL feed and wire v6 use exact 18-scale fixed-point integers. Snapshot **v10** and wire **v6** are
breaking changes from earlier builds, so recreate data directories when upgrading. See
[`docs/DECIMAL.md`](./DECIMAL.md).

Known gaps and the roadmap are tracked in [`TODO.md`](../TODO.md).
