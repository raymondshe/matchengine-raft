# Third-party matching-engine investigations

Three external matching-engine projects were reviewed for design ideas, benchmark methodology and
failure modes. This document consolidates the separate reports into one reference.

| Project | Revision | License | Primary value to us |
|---|---|---|---|
| [AsthaMishra/matching-engine](https://github.com/AsthaMishra/matching-engine) | `2a74c3f` (2026-08-14) | No license file exposed | Hot-path order-book layout, binary gateway, measurement discipline |
| [wasif-exe/wire](https://github.com/wasif-exe/wire) | `abd2bf2` (2026-10-19) | MIT | Low-level layout/allocator/MPMC ideas and a cautionary benchmark case |
| [llc-993/matching-core](https://github.com/llc-993/matching-core) | `2cb21c0` (2026-01-03) | No license file exposed | Feature breadth, WAL/snapshot attempt, integration/benchmark caution |

Review date: 2026-11-01, re-reviewed 2026-11-04. The review used the public READMEs, `PLAN.md`
and the key source files listed in the references; the 2026-11-04 pass added AsthaMishra's
`matching-core/optimization_notes.md` to Part I. Tests were not executed here. No code was copied;
all three projects are reference material only. The performance-specific verdicts (what transfers to
matchengine-raft and what does not) live in
[`docs/PERFORMANCE.md`](./PERFORMANCE.md) ("External optimization notes").

## Part I - AsthaMishra/matching-engine

### 1. What it is

A single-node Rust engine split into five crates:

- `matching-core`: pure order book, matching and types; no threads, async or I/O.
- `matching-engine`: sharded worker runtime, symbol routing and request/response slots.
- `ouch-gateway`: NASDAQ OUCH/ITCH-style binary TCP gateway and latency harness.
- `rest-gateway`: Axum REST adapter for queries/convenience.
- `server`: binary wiring.

It has no consensus, WAL, snapshot, recovery or multi-node story. Its value is the hot-path data
structure, the gateway separation and the way it reports measurements.

### 2. Order book and matching

The core book is deliberately specialized:

- `bid`/`ask` are `Vec<Option<PriceLevel>>` indexed by `price / tick`.
- `PriceLevel` holds a `Vec<Order>`, `total_qty`, `active_count` and `head_idx`.
- `order_index` is a `Vec<Option<(side, price, qty, level_index)>>` keyed by order id.
- Bitmaps mark active price slots; cached best-bid/ask indices make BBO a field read.
- Cancel marks an order inactive and advances `head_idx`; matching skips inactive entries.
- `match_order_into` appends to a caller-provided event buffer; the hot path reuses it.

The constraints are explicit: 100,000 price slots, $0.01-$999.99, one-cent ticks, `u32` quantities
and roughly 9.6 MB per book. That buys O(1) price lookup, bitmap BBO scans and cheap cancel at
depth, but it is not a general 18-decimal, multi-asset price model.

The matching loop pre-checks FOK liquidity, walks levels with a local cursor, uses `head_idx` to
skip inactive orders, and implements self-trade prevention as **cancel-resting**. It emits
`Executed`, `Accepted`/`Replace`, `Modified`, `Canceled` and `Rejected` events. `match_order_into`
is allocation-free in steady state; `match_order` is only an allocating convenience wrapper.

### 3. Runtime and gateway

The crate README describes a sharded runtime: one single-threaded owner per book, symbols sharded by
`symbol_id % num_workers`, workers using `crossbeam::Select`, and callers using a pre-allocated slot
pool plus request/response channels. The sender map is immutable after startup.

`PLAN.md` records an important architectural turn: the OUCH datapath moved to a single-threaded,
run-to-completion **io_uring** loop with SQPOLL because the cross-thread handoff dominated the tail.
The sharded runtime remains part of the REST/slot path; the README's sharded description is no
longer the whole current OUCH datapath.

The gateway adds a fixed-size binary codec, TCP sessions with login/heartbeat lifecycle, io_uring
run-to-completion, an off-by-default `metrics` feature and separate load clients for lock-step RTT,
io_uring RTT and pipelined throughput. REST is explicitly a convenience/query adapter, not the
low-latency path.

### 4. Testing and benchmarking

The project reports unit tests, `proptest` invariants (fill quantity, no crossed book, FOK
all-or-nothing, IOC/Market never rest, price-time priority, self-trade prevention), fuzzing, NASDAQ
ITCH replay, synthetic deep sweeps and Criterion microbenchmarks.

The benchmark documentation is notably honest:

- ITCH replay is mostly insert/cancel/modify book management, not matching throughput.
- Order-to-ack numbers are loopback with software timestamps and a closed-loop client; the tail is
  understated by coordinated omission.
- Timer resolution is about 100 ns, so sub-100 ns percentiles are quantized.
- The book is allocated outside the timed window; earlier in-timer allocation inflated results by
  4-15x and caused 2.4x run-to-run swings.

`PLAN.md` also says the fuzz target is currently stale and does not build, despite the README's
historical fuzz claim. Treat that claim as unverified until the target is fixed.

`matching-core/optimization_notes.md` (re-reviewed 2026-11-04) records nine dated optimization
steps with before/after numbers: the flat array + bitmap (7.8x-318x at depth 100-1000 over their
BTreeMap baseline, with `bbo`/`top_n` regressions they then fixed through cached best indices and
bitmap-word iteration), a `Vec` order index keyed by per-book sequential ids after dropping SipHash
(1.7-1.8x), an `active`-flag + `head_idx` cancel/match path, a `bumpalo` arena that was added and
then **removed** after measuring 79-88% worse single-operation latency, a pre-allocated
response-slot pool replacing `oneshot::channel()`, N-thread symbol sharding, and a NASDAQ ITCH
replay as the only production-like signal (AAPL, 1.94M ops: p50 100 ns, p99 1.8 us, p99.9 14.9 us;
100 symbols, 108M sequential ops at ~4.1M ops/s). The notes are unusually honest about benchmark
mechanics: `iter_batched` timed the book's O(MAX_PRICE) drop and produced ~960 us uniformly until
they moved it outside the timer with `iter_custom`, and they separate warm-cache throughput from
cold-cache per-operation numbers.

### 5. Findings and risks

- **Self-trade prevention without an event.** In the reviewed matching code, a self-trade-prevented
  resting order is marked inactive and removed from the order index without an `OrderEvent`. Any
  adoption of this design must emit a `Cancelled`/`Rejected` event for the event log and feed.
- **FOK + STP edge.** `can_fully_fill` sums `PriceLevel.total_qty`, which includes same-trader
  resting orders. With STP enabled, a FOK could pass the pre-check, lose that liquidity to STP and
  then drop its remainder without an event. This needs a targeted test if STP is added.
- **Documentation drift.** The README describes the sharded runtime, while `PLAN.md` says the OUCH
  path is now a single-threaded io_uring loop; the fuzz target is stale. Verify code before trusting
  prose.
- **No license file.** The GitHub API reports no license. Do not copy code from this repository.
- **Optimizations are provisional until re-measured.** Optimization 7 added a `bumpalo` arena and
  Optimization 9 removed it after it regressed single-operation latency 79-88%; the 132M/s headline
  turned out to be multi-threaded (~33M/s per thread). Treat each technique in that document as a
  hypothesis with its own A/B, and require the same evidence of ourselves before adopting one.
- **No durability/consensus.** This is a single-node in-memory engine; our Raft/WAL/snapshot story
  is a separate, non-negotiable strength.

## Part II - wasif-exe/wire

### 1. What it is

A 24-crate Rust/C systems portfolio spanning userspace TCP/IP, AF_XDP, io_uring, VFIO PCIe DMA, a
LMAX-style disruptor, SIMD parsers, CME MDP 3.0 SBE, PTP, eBPF risk and a v9 matching/memory kernel.
The v9 work adds four pieces:

- `wire-matching`: flat-array price-time matching kernel.
- `wire-alloc`: thread-local HugePage slab allocator.
- `wire-mpmc`: Vyukov-style bounded MPMC queue.
- `wire-exchange::simd_cancel`: AVX2 scan kernels for cancellation masks.

The README explicitly discloses that v9 is **not** a production exchange: no network I/O on the
matching path, no live FIX/ITCH parsing during matching, no WAL persistence, no pre-trade risk and
no fault tolerance. That honesty is the project's strongest feature.

### 2. v9 matching kernel

`wire-matching` uses `[OrderSlot; 65_536]` (16-byte slots), `[PriceLevel; 65_536]` for each side,
a free list, generation-based ABA protection and cached best/min/max price watermarks. Price levels
are indexed by `price & 65_535`.

The critical limitation: `match_order` performs **at most one fill per call**. It fills the head of
the best opposing level and rests any remainder as a new order. It does not continue to the next
order or the next price level. Consequences:

- A marketable order larger than the best resting order cannot sweep the book.
- A resting remainder can cross the opposite side.
- There are no market/IOC/FOK order types; the API is only price/shares/side/timestamp.
- There is no user id or self-trade prevention in the matching slot; the separate SIMD module has
  user ids only for masks.
- `MatchResult` has `fill_count` 0 or 1 and no taker order id; `new_order_id` stays at its default.

The README's metric table says "Single fill per match", which is accurate at that level. The
high-level "Price-Time Priority Matching Engine" label is broader than the code; treat it as a
single-fill matching primitive.

Other edge cases:

- `price & 65_535` aliases prices 100 and 65,636; there is no bounds check.
- `best_bid == 0` is the "no bid" sentinel; a valid bid at price 0 is not representable.
- `alloc_order` returns `NULL_IDX` at capacity and the remainder is silently dropped.
- `cancel_order` walks the level's singly linked list, so cancel is O(n), not O(1).
- `advance_best_bid`/`advance_best_ask` scan linearly between watermarks; the benchmark never
  triggers an advance because every operation uses the same price.
- The matching engine has five unit tests (buy/sell fill, generation cancel, BBO, alignment); no
  multi-fill, best-level removal, price overflow, capacity or zero-share tests.

The code relies on raw-pointer indexing and `unsafe impl Send`; no Miri/loom/fuzz evidence was in
the reviewed material.

### 3. Allocator, MPMC queue and SIMD cancel

**`wire-alloc`** uses three thread-local fixed-size slabs: 64, 128 and 256 bytes, each backed by a
HugePage allocation with a bump pointer and intrusive free list. `SlabBox<T>` selects the class and
returns the block on drop. It is fast for fixed-size, thread-local objects, but each active thread
can reserve **192 MiB** (64 + 64 + 64 MiB); there is no cross-thread free, fragmentation handling or
size class above 256 bytes. The benchmark does not assert which HugePage kind was actually used.

**`wire-mpmc`** is a conventional Vyukov bounded ring with power-of-two capacity, per-slot sequence
counters, 64-byte alignment, acquire/release ordering and `cpu_relax` spinning. Tests cover FIFO,
full/backpressure and a 4-producer/4-consumer run. The benchmark is single-thread push+pop, as the
README states; it is not a contention measurement.

**Tokio compatibility:** the queue has no Tokio dependency and is `Send + Sync` when `T: Send`, so
`try_push`/`try_pop` can be called from Tokio tasks. It is not an async queue: there is no waker,
`close`, or suspendable send/receive. `push_spin`/`pop_spin` busy-wait with `_mm_pause`; calling
them on a Tokio worker blocks that worker, and on a current-thread runtime a producer/consumer pair
can deadlock because the consumer cannot run while the producer spins. `try_push`/`try_pop` are
short and non-blocking in the uncontended case, but a retry loop in an async task still needs
`yield_now`/`Notify` backoff. Use Tokio `mpsc` for task-to-task communication; use `wire-mpmc` only
between dedicated/pinned threads or inside `spawn_blocking`, then bridge to async with a waker or
channel. The multi-thread tests do not cover Tokio scheduling or a contended async producer.

**`simd_cancel`** packs eight order slots into 128 bytes and provides AVX2 comparisons for user id
and price threshold, returning an 8-bit mask. Important limits:

- It does **not cancel** anything: the functions take `&mut` but only read; no flag is written, no
  order is removed, and no store is in the measured path.
- The AVX2 function is called unconditionally on x86_64; there is no runtime AVX2 detection.
- Price comparison is signed (`_mm256_cmpgt_epi32`), which is fine only for the 65k price domain.
- The packed layout is separate from `wire-matching::OrderSlot`; integrating it would require a
  second representation.

### 4. `wire-exchange` LOB bookkeeping

`wire-exchange/src/lob.rs` is a market-data LOB updater, not a matcher. It applies add/execute/
cancel/delete/replace events and returns a BBO. The reviewed code has several correctness issues:

- `OrderExecuted` decrements `order_count` on every execution, even a partial fill where the order
  remains active. `order_count` is the number of orders, so the level can be marked inactive while
  an active order remains.
- When the best level empties, the cached best price is not advanced; `bbo()` can return a stale
  price with zero shares instead of the next level.
- `OrderCancel` marks the order inactive unconditionally, even for a partial cancel.
- The order map is indexed by `order_id & 65_535`; ids differing by 65,536 collide, with no
  generation handling.
- The LOB test only adds a bid and an ask and checks the BBO; executions, cancels, deletes, replaces
  and best-level removal are untested.

### 5. Benchmarking

The v9 dashboard has good habits: pinned core (`taskset -c 2`), native CPU flags, LTO/codegen
settings disclosed, `black_box`, setup outside the timer, per-row scope descriptions and a clear
"not production" warning.

Caveats:

- `rdtsc` is a reference/TSC counter, not a core-cycle counter. The code converts cycles to ns with
  a hard-coded 3 GHz (`cycles * 0.333`); turbo/thermal/virtualization can make the ns figure wrong.
- The matching benchmark pre-fills alternating sell/buy orders at one price and times alternating
  rest/fill at that same price. It does not measure deep sweeps, multiple fills, cancel at depth,
  BBO advancement, multi-level walks or crossed-book prevention.
- Pass/fail thresholds (`cycles <= 15`, `ns <= 3`, `ns <= 8`, `cycles <= 3`) are self-graded.
- The v9 binary does not connect matching to the gateway, WAL, risk filter or feed. There is no
  end-to-end benchmark.
- 8x manual unrolling is a benchmark technique; production call patterns may differ.
- The "46/46 tests pass" claim was not executed here; unit tests exist in the v9 modules, but no v9
  integration suite was observed.


## Part III - llc-993/matching-core

### 1. What it is

A single-node Rust matching-engine library (`matching-core` 0.2.0) with API types, several order-book
implementations, a Disruptor-based exchange pipeline, risk engine, WAL/snapshot attempt, examples,
Criterion benches and integration tests. The README advertises GTC/IOC/FOK/Post-Only/Stop/Iceberg/
GTD/Day, spot/futures/perpetual/options, SOA layout, rkyv WAL, sharding, SIMD and an ART index.

The repository's own note is important: it says the code was copied from a production environment,
contains only the general logic and has not been production-tested. The GitHub API reports no
license file, so this review treats it as reference-only.

### 2. Integration reality: the advertised features are not the ones the exchange uses

The biggest finding is a gap between the examples and the actual `ExchangeCore` pipeline:

- `MatchingEngineRouter::add_symbol` always creates a `DirectOrderBook`.
- `DirectOrderBook` supports only `Gtc`, `Ioc` and `FokBudget`. Post-Only, Stop, Iceberg, GTD, Day
  and regular FOK are not implemented there.
- The `AdvancedOrderBook` that implements the advertised order types is used by the README examples
  and `AdvancedOrderBook` benchmarks, but is not wired into `ExchangeCore`.
- A command with an unsupported order type returns `MatchingUnsupportedCommand` from the matching
  router. The risk engine has already held funds in `pre_process`; with no matcher event, the
  post-process path has nothing to refund. This can leave a rejected order's hold stuck.
- `ExchangeCore::submit_command` journals the command, publishes a clone to the Disruptor and
  returns the original `cmd` immediately. The processed clone is delivered to the result consumer,
  so the return value is not the result when the producer path is active.
- `ExchangeCore::serialize_state` panics after `startup()` because the pipeline has been moved into
  the Disruptor handler; `take_snapshot` therefore cannot snapshot a running exchange.
  `load_latest_snapshot` replaces `self` and drops the configured journaler/snapshot store.
- `ExchangeConfig.wait_strategy` is ignored; `startup` always uses `BusySpin`. The matching/risk
  shard counts are used, but the matching router still picks `DirectOrderBook`.

This is a classic "feature exists in a module, not in the product path" problem. Any feature matrix
must be tested through the public exchange API, not only through a direct order-book example.

### 3. Order-book correctness findings

**`DirectOrderBook`** (the exchange's actual book) uses `Slab`, `AHashMap` and `BTreeMap` price
indexes with a linked list per bucket. It supports GTC/IOC/FokBudget only. The code has a duplicate
order-id path that treats the duplicate as a taker and rejects the remainder instead of rejecting the
duplicate. Move/reduce are simplified, and there is no self-trade prevention. The linked-list and
best-order maintenance is fragile and is not covered by the README's performance claims.

**`AdvancedOrderBook`** (the advertised advanced book) has several semantic problems:

- `can_fill_completely` (FOK pre-check) sums `bucket.total_volume`, including orders whose
  `expire_time` has passed. A FOK can pass the pre-check, match less than its size, and then push a
  reject for the remainder after partial fills; that is not all-or-nothing.
- Expired orders found during matching are queued for removal without decrementing the bucket's
  total/visible volume. The level can keep inflated volume and a stale best price.
- Iceberg `visible_size` is used only for display volume; `match_order` consumes the order's full
  remaining size, so the hidden reserve is not protected by the display tranche.
- Stop orders trigger from `last_trade_price`, which is set to the taker's limit price rather than
  the actual execution price. StopMarket has no real market-order path.
- GTD/Day expiry is checked only when a taker reaches that bucket; there is no global due-order
  purge at request intake. Day has no special close handling.
- Cancels and reductions emit `MatcherEventType::Reject`, not a distinct Cancel/Reduce event.
  `AdvancedOrderBook::cancel_order` ignores `cmd.uid`, so a user can cancel another user's order.
- The event enum only has `Trade`, `Reject` and `Reduce`; the README example that matches
  `MatcherEventType::Cancel` does not compile against the code.

**`DirectOrderBookOptimized`** is an SOA/SIMD prototype that is not used by `ExchangeCore`:

- `cancel_order` deallocates the order from the pool but does not unlink it from its bucket, update
  bucket volume, or refresh the best price.
- `insert_to_bucket` inserts at the head (LIFO), which reverses time priority.
- The SIMD batch path updates `filled` and bucket volume but does not deallocate fully filled orders
  or remove them from `order_index`; completed orders remain reachable.
- `serialize_state` returns a new empty `DirectOrderBook`, losing all state. A snapshot of this book
  would silently restore an empty book.
- Move/reduce are `MatchingUnsupportedCommand`.

**`simd_utils`** is mostly scalar. `simd_price_compare_le/ge`, `simd_min_pairs` and
`simd_batch_match_prepare` loop over `chunks_exact(4)` and compare elements individually; only
`simd_sum_sizes` and `simd_sub_vectors` construct `i64x4` vectors. The README's "SIMD batch matching"
is not a real SIMD matching path.

### 4. Persistence, risk and events

- **WAL** (`journal.rs`): rkyv length-prefixed records, one `flush` per command, no `sync_data`, no
  CRC/magic/version, no torn-tail recovery and no rotation. `read_commands` reads the whole file
  into a `Vec`; a truncated payload returns an error instead of truncating to the last good record.
  `submit_command` ignores journal write errors.
- **Snapshot** (`snapshot.rs`): bincode into `File::create` + `BufWriter`, with no flush/fsync,
  atomic rename, CRC, version or manifest. The latest snapshot is selected by filename. The
  `ExchangeCore` snapshot path is also blocked after startup (see above).
- **Risk** (`risk_engine.rs`): pre-trade hold uses `size * price * quote_scale + size * taker_fee`.
  The README describes fees in basis points, but the code does not divide by 10,000; the fee is not
  notional-proportional in general. Margin/futures/options fields are not used. Unsupported matching
  returns before `post_process`'s success assignment, but the pre-trade hold is not refunded because
  no event was emitted; a non-empty reject event also causes `post_process` to set `Success`. No
  self-trade prevention.
- **Events**: only `Trade`, `Reject` and `Reduce`. There is no explicit Accepted/Rested/Cancelled/
  Expired/Amended event, so a consumer cannot reconstruct the full order lifecycle from the event
  stream alone.

### 5. Benchmark claims

The README's 7.2M TPS / 3.6M QPS / memory table come from
`examples/generate_benchmark_data.rs`:

- It measures `AdvancedOrderBook::new_order` directly, not `ExchangeCore`, the Disruptor pipeline,
  risk engine, WAL or network.
- It creates unique order IDs and alternating ask/bid orders at 100 prices; this is a synthetic
  single-book workload, not an exchange workload.
- `QPS` is actually `cmd.matcher_events.len()` (events per second), not queries.
- `Memory_MB` is an estimate of 200 bytes per order, not measured RSS. The Criterion
  `comprehensive_bench` `measure_memory` reads the process's `VmRSS` once and records that same
  value for each book size; it is not per-book memory either.
- There is no hardware disclosure, CPU pinning, warmup, percentile or run-to-run variance in the
  README table. The numbers are best treated as a synthetic order-book microbenchmark, not exchange
  TPS.
- `benches/orderbook_optimized_bench.rs` reuses the same `order_id` on every iteration; after the
  first iteration the Direct books take the duplicate-ID path, so the benchmark does not measure
  normal placement.
- `benches/exchange_bench.rs` repeatedly calls `RiskEngine::pre_process` with the same user; the
  balance is drained and later iterations exercise the insufficient-funds path, not the success
  path.

### 6. Strengths and lessons

Strengths: broad feature ambition, examples for advanced orders, several book implementations, a
Disruptor/risk/pipeline skeleton, a WAL/snapshot attempt, SmallVec/Slab/AHashMap use and a committed
benchmark dataset. The README's note that it is copied from production and not production-tested is
honest.

Lessons for matchengine-raft:

- Test every advertised order type through the real pipeline; an advanced book that only exists in
  examples is not a feature.
- Every terminal state change must emit an event. Do not overload Reject for cancel/expiry/STP.
- FOK/iceberg/GTD/stop need explicit tests against expiry, self-trade, hidden volume and actual
  execution price.
- WAL/snapshot need fsync, atomic replace, CRC/version, torn-tail recovery and a recovery test;
  `flush` alone is not durability.
- Benchmark the actual product path and measure memory, not an estimate. Separate TPS, events/s and
  queries/s.
- Avoid unintegrated "optimized" implementations whose state serialization loses data.

## Part IV - Comparison with matchengine-raft

| Dimension | AsthaMishra | Wire v9 | llc-993/matching-core | matchengine-raft |
|---|---|---|---|---|
| Topology | Single node | Single node, benchmark kernels | Single-node library + Disruptor pipeline | Raft-replicated cluster |
| Durability | None | No WAL on matching path | WAL/snapshot attempt without fsync/atomicity | Segmented WAL + atomic snapshots + recovery |
| Symbol model | Multi-symbol sharded; single-book OUCH path | One flat book, 65k slots | Symbol-sharded routers; Direct book per symbol | One symbol per process/cluster |
| Book | Flat array + bitmap + `Vec` level | Flat array + linked-list levels | BTreeMap/Slab/AHashMap (Direct); BTreeMap/SmallVec (Advanced); SOA prototype unused | `BTreeMap` price/sequence |
| Price/quantity | Cents, `u32` | `u32` price index, `u32` shares | `i64` price/size | 18-decimal `Fixed` `i128` |
| Order types | Limit/Market/IOC/FOK | Single-fill primitive | Advertised many; exchange Direct only GTC/IOC/FokBudget | GTC/IOC/FOK/GTD/Day/market/post-only/iceberg/stop |
| User/STP | Cancel-resting STP | None in matcher | No STP; Advanced cancel ignores uid | No trader id; fees/assets modeled |
| Events | In-process stream; global trade id | One `MarketEvent` per fill | Trade/Reject/Reduce only | Replicated event window; deterministic ids |
| Protocol | OUCH/ITCH + REST | No integrated order entry | Library only | HTTP JSON + gRPC Raft |
| Tests | Unit/proptest/fuzz (stale target) | Unit per kernel; no v9 integration | Tests present; no pipeline/consensus integration observed | Unit/property/conformance/integration/restart/snapshot/fees |
| Benchmarks | Criterion/ITCH/tail | Pinned-core microbenchmarks | AdvancedOrderBook TPS; estimated memory; flawed Criterion cases | e2e/batch/feed/storage/P0 benchmarks |

Our engine is stronger on semantics, durability, exactness and recovery. Astha is stronger on
single-node hot-path layout, binary gateway work and measurement scope. Wire is stronger on
low-level allocator/MPMC experiments and honest scope disclosure, but its v9 matching kernel is
narrower than its name suggests. llc-993 is the broadest feature skeleton, but its advanced order
types are not wired into the exchange, its WAL/snapshot is not crash-safe, and its benchmark numbers
measure a separate `AdvancedOrderBook`, not the product pipeline.

## Part V - Consolidated takeaways

### Adopt

1. **Measurement-scope discipline.** Add a table to `docs/PERFORMANCE.md` stating what each
   benchmark includes/excludes, whether it is closed-loop, and whether it measures TSC ticks, wall
   time or system latency.
2. **A pure order-book replay benchmark.** Keep the existing end-to-end `replay_orders` test, but
   add a book-only benchmark with deep sweeps, cancel-at-depth and BBO-advance cases. Do not mix
   Raft/HTTP into that number.
3. **Fuzzing.** Add `cargo-fuzz` targets for `OrderBook::execute_order` and snapshot/WAL decoding;
   keep the targets buildable. Astha's stale fuzz target is a warning.
4. **Preallocation and cache-layout experiments.** Try a per-level `Vec` + `head_idx` + order index
   and a cached BBO in a branch; measure before committing. Do not copy the fixed 65k flat array.
5. **Allocation accounting.** Measure apply-path allocations and consider a scratch event buffer,
   `Arc<[MatchEvent]>` sharing or `SmallVec` for common cases.
6. **A reusable MPMC reference.** If we ever need a custom bounded queue, the Vyukov pattern is a
   useful reference; Tokio `mpsc` remains the default.

### Adapt / evaluate later

- A binary order-entry path (gRPC/protobuf or compact codec) separate from HTTP admin/reads, only if
  latency requirements justify it.
- Feature-gated latency histograms, off by default.
- Real ITCH-style replay data, if a legal and representative dataset is available.
- Custom MPMC/SIMD/io_uring only after a measured bottleneck and a Linux-only deployment decision.

### llc-993-specific takeaways

- **Test the product path, not the demo.** Add a public-API matrix test that every advertised order
  type reaches the matching engine; llc-993's advanced book exists only in examples.
- **Emit explicit lifecycle events.** Cancel, expiry and STP must not be encoded as `Reject`; a
  consumer must be able to reconstruct the full order state.
- **Test FOK/iceberg/GTD/stop against expiry, STP, hidden volume and the actual execution price.**
- **Make persistence crash-safe.** WAL/snapshot need fsync, atomic replace, CRC/version, torn-tail
  recovery and a recovery test; a `BufWriter::flush` is not durability.
- **Benchmark the real pipeline and measure memory.** Separate TPS, events/s and queries/s; do not
  report an estimated 200 bytes/order as measured memory.
- **Do not ship an unintegrated optimized book.** If a book cannot serialize its state or is not
  reachable from the exchange, it is a prototype, not a production path.

### Avoid

- Calling a one-fill primitive a matching engine, or comparing its cycles to a full exchange stack.
- A fixed flat price array without a price-range contract; our 18-decimal model needs a sparse or
  hybrid book.
- O(n) cancel hidden behind an "O(1) flat array" claim; our BTreeMap cancel is already O(log n).
- SIMD "cancel" that only computes a mask and never updates state.
- Hard-coded `rdtsc` frequency conversion; report TSC ticks or measure wall time.
- A 192 MiB-per-thread slab allocator for variable-sized, long-lived orders/events.
- An arena allocator on a per-entry apply path: the AsthaMishra notes measured `bumpalo` 79-88%
  worse for single-operation latency and removed it (their Optimization 7 -> 9).
- Global process-local trade ids; our deterministic `(log_index, ordinal)` ids are required for
  replication and replay.
- Copying a project's code or claims without a license/buildable test evidence. AsthaMishra and
  llc-993 have no license file; Wire's fuzz/README claims need verification.
- Feature matrices that are only demonstrated through a separate book type, or benchmark numbers
  from a different code path than the exchange pipeline.

### Prioritized plan

**P0 - documentation, benchmarks, tests**

1. Add the measurement-scope table and closed-loop/timer caveats to `docs/PERFORMANCE.md`.
2. Add `benches/book_replay.rs` for book-only replay with deep sweeps, cancel-at-depth and BBO
   advancement.
3. Add `cargo-fuzz` targets for matching and snapshot/WAL decoding.

**P1 - measured data-structure and allocation work**

4. Prototype a per-price-level `Vec` + `head_idx` + order index + cached BBO; benchmark cancel,
   match, amend and depth before merging.
5. Measure and reduce apply-path allocations.
6. Evaluate a binary order-entry path or feature-gated metrics only if a target requires it.

**P2 - optional platform work**

7. Real-data replay and custom MPMC/SIMD/io_uring experiments, only after a measured bottleneck.

**Non-negotiable foundations:** Raft consensus, WAL/snapshot durability, deterministic event ids,
exact 18-decimal values, fees/assets and the existing property/conformance/integration suites.

## References

### AsthaMishra/matching-engine

- <https://github.com/AsthaMishra/matching-engine>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/matching-core/README.md>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/matching-engine/README.md>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/ouch-gateway/README.md>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/PLAN.md>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/matching-core/src/order_book.rs>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/matching-core/src/matching.rs>
- <https://github.com/AsthaMishra/matching-engine/blob/stable/matching-core/optimization_notes.md>

### llc-993/matching-core

- <https://github.com/llc-993/matching-core>
- <https://github.com/llc-993/matching-core/blob/master/README_EN.md>
- <https://github.com/llc-993/matching-core/blob/master/src/core/exchange.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/processors/matching_engine.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/orderbook/direct.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/orderbook/direct_optimized.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/orderbook/advanced.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/journal.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/snapshot.rs>
- <https://github.com/llc-993/matching-core/blob/master/src/core/processors/risk_engine.rs>
- <https://github.com/llc-993/matching-core/blob/master/examples/generate_benchmark_data.rs>

### wasif-exe/wire

- <https://github.com/wasif-exe/wire>
- <https://github.com/wasif-exe/wire/blob/main/README.md>
- <https://github.com/wasif-exe/wire/blob/main/wire-echo/src/bin/v9_benchmark.rs>
- <https://github.com/wasif-exe/wire/blob/main/wire-matching/src/lib.rs>
- <https://github.com/wasif-exe/wire/blob/main/wire-exchange/src/lob.rs>
- <https://github.com/wasif-exe/wire/blob/main/wire-exchange/src/simd_cancel.rs>
- <https://github.com/wasif-exe/wire/blob/main/wire-alloc/src/lib.rs>
- <https://github.com/wasif-exe/wire/blob/main/wire-mpmc/src/lib.rs>
