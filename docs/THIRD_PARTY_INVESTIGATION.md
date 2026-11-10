# Third-party matching-engine investigations

Four external matching-engine projects were reviewed for design ideas, benchmark methodology and
failure modes. This document consolidates the separate reports into one reference.

| Project | Revision | License | Primary value to us |
|---|---|---|---|
| [AsthaMishra/matching-engine](https://github.com/AsthaMishra/matching-engine) | `2a74c3f` (2026-08-14) | No license file exposed | Hot-path order-book layout, binary gateway, measurement discipline |
| [wasif-exe/wire](https://github.com/wasif-exe/wire) | `abd2bf2` (2026-10-19) | MIT | Low-level layout/allocator/MPMC ideas and a cautionary benchmark case |
| [llc-993/matching-core](https://github.com/llc-993/matching-core) | `2cb21c0` (2026-01-03) | No license file exposed | Feature breadth, WAL/snapshot attempt, integration/benchmark caution |
| [flash1-dev/matching-engine-benchmark](https://github.com/flash1-dev/matching-engine-benchmark) | `424e4e5` (2026-10-16) | MIT (vendored parts have their own licenses) | 249-engine differential benchmark, consensus oracle, conformance battery, worst-case methodology |

Review date: 2026-11-01, re-reviewed 2026-11-04, benchmark review added 2026-11-10. The review used
the public READMEs, `PLAN.md`, the benchmark's reports and the key source files listed in the
references; the 2026-11-04 pass added AsthaMishra's `matching-core/optimization_notes.md`, and the
2026-11-10 pass added Part IV. Tests were not executed here. No code was copied; all four projects
are reference material only. The performance-specific verdicts (what transfers to matchengine-raft
and what does not) live in [`docs/PERFORMANCE.md`](./PERFORMANCE.md) ("External optimization
notes").

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

The FlashOne survey (Part IV) later measured this engine at **5.60 M/s worst-case** (`flash-crash`)
after a one-line bounds-check fix. That result is consistent with §2: the direct-indexed array is
fast in its 100k-tick domain, but the bounded domain drops orders once the price path leaves it.
The benchmark classifies it as the representative of the 14 flat-array engines; the category's
ceiling is well below the tree-based leaders.

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

The FlashOne survey (Part IV) pinned the same `2cb21c0` commit and measured **7.14 M/s**, conforming
as shipped on its new/cancel/modify ABI. That is not a contradiction: the ABI does not exercise the
advanced order book, risk engine, WAL, snapshot or exchange pipeline that are the subject of §2–§4.
It is a useful example of why a matching-only conformance result is not a product-path
certification.

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

## Part IV - flash1-dev/matching-engine-benchmark (249-engine survey)

### 1. What it is

A benchmark and correctness oracle, not a matching engine. At `424e4e5` (2026-10-16) the repository
reports **249 engines** across 20+ languages: **162** reproduce a byte-identical report-stream
consensus (111 of them only after the benchmark's documented fix), and **87** diverge, exceed the
2M-message budget, crash, or are domain-incompatible. The README reports **183 upstream issues**
filed from the survey; the resolved-findings roster tracks fixes separately. The accompanying paper
is [arXiv:2606.01183](https://arxiv.org/abs/2606.01183) (Flash One Technologies, 2026).

The harness drives one symbol through a C ABI, one message at a time by default: 1,000,000 new
orders expand to roughly 2,000,000 new/cancel/modify messages. Five scenarios (`static`, `normal`,
`swing-25`, `swing-40`, `flash-crash`) vary the mid-price path and the standing book. The
definitional result is **worst-case throughput**: the lowest of the five scenario medians, not the
best or the average. A full challenge is **10 perf runs + 1 audit run per scenario**; each scenario
reports the median of its ten perf runs, and the engine's result is the lowest of the five medians.
Correctness is the SHA-256 of the full report stream (`OrderAck`,
`Trade`, `CancelAck`, `ModifyAck`, `CancelReject`, `ModifyReject`) against a consensus first
established by three independent engines (Liquibook, QuantCup, Exchange-core), plus **192
book-state probes** (64 indices × best bid/best ask/depth) at unpredictable points. A pre-run gate
adds **34 hand-crafted hard-invariant cases**, and 10,000 precomputed seeds support CI.

The workload is calibrated rather than synthetic-uniform: 15% of new orders are IOC; of the
non-IOC orders, 20% get a modify and 95% are eventually cancelled; ~2% of cancels and of
modified-then-cancelled orders arrive twice; the quantity range is 1-100, depth follows a
power-law profile, and the mid follows a GBM path. The
standing book is a property of the seed's realisation, not a constant: at the canonical seed 23 it
averages/peaks at roughly **20,900/41,800 orders in `static`**, **2,700/5,500 in `normal`**,
**1,300/2,600 in `swing-25`**, **700/1,400 in `swing-40`** and **240/500 in `flash-crash`**. That
is the survey's most transferable measurement insight: the price structure pays for occupancy, not
only for message dispatch, and a matcher must be judged in the regime it handles worst.

The anti-cheat design is unusually careful and worth reading in full:

- Perf and audit are separate passes, but **every** run probes the same random book points, so an
  engine cannot tell whether it is being measured.
- The optional prebuild hook may translate/reserve only; the book must be empty before the clock
  starts, and a shadow pre-matcher is caught by a prebuild/timed ratio.
- The probe window is excluded from throughput but capped at 25% of the raw timed window, so an
  engine cannot move its matcher into `engine_query_*`.
- Each run is a fresh process with a regenerated workload; the host fingerprint and seed are
  recorded, so a third party can reproduce the claim.
- Adapters must answer `engine_query_*` from the engine's own book, never a shadow; the benchmark
  demonstrates a case where a faithful forwarding change exposed a real phantom-level bug that a
  rebuilt shadow had hidden.

One caveat: the repository's own counts drift between documents (at this revision, 28 vs 18 fixed
issues and 42 vs 40 shipped adapters). Use the per-engine issue links and the pinned commits in
`SNAPSHOTS.md` as the authority; the summary tables are snapshots, not a live database. That is the
same docs-vs-code discipline this project applies to its own documentation.

### 2. This project in the roster

`raymondshe/matchengine-raft` is pinned at `5597532` (2026-03-04) and classified as the
**B-tree/B+tree representative**: Rust, **7.20 M/s worst-case**, the fastest of the 27 B-tree
engines. The roster marks it **"with fix"** for issue #1 ([link](https://github.com/raymondshe/matchengine-raft/issues/1)): a
phantom zero-quantity fill that re-stored the next resting order through `Order::new`, resetting
its `id` to `0` and its side to the taker's. The finding is valid for that commit.

The fix landed in `a44d9ae` (2026-10-27), and issue #1 was closed the same day. Current `HEAD` has
the taker-exhaustion guard plus explicit regressions in `src/order_book/tests.rs`:
`exact_fill_does_not_emit_phantom_or_corrupt_bystander_bid`,
`exact_fill_does_not_emit_phantom_or_corrupt_bystander_ask`,
`exact_fill_stops_before_deeper_levels` and
`full_sweep_leaves_book_empty_without_phantom_trades`. The benchmark label describes the old pin,
not current code; a re-pin should clear the "with fix" flag, but that is the benchmark maintainer's
call. The pinned commit declared `MIT/Apache-2.0`; the current repository is MIT.

The 7.20 M/s figure measures the order-book core only. The C ABI has no Raft, HTTP, WAL, snapshot,
replicated event window, fee/asset accounting, stops, icebergs, multi-symbol routing or risk. It is
therefore not comparable to the four-node **227k orders/s** `/write_batch` result or the **~969k
ops/s** `replay_mix` whole-replay result in [`docs/PERFORMANCE.md`](./PERFORMANCE.md): the workload,
hardware and scope all differ. The defensible claim is narrower: the `BTreeMap` core is the fastest
B-tree implementation in this survey, within about 8% of the fastest Rust engine (`melin`, 7.86) and
about 22% of the fastest open engine (`lcv3`, 9.25). It says nothing about the replicated product
path, which our own e2e and cluster suites cover.

### 3. Methodology worth borrowing

| Practice | What the benchmark does | Our current state | Recommendation |
|---|---|---|---|
| Worst-case across regimes | Lowest of five scenario medians, with the scenario named | `book_replay`/`replay_mix` report per-case best-of-5 and warm p50/p99; `e2e`/`cluster_e2e` vary client load, not market regime | Done: `book_replay -- --matrix` reports the worst-case median-of-10 across four occupancy shapes |
| Calibrated order flow | 95% cancel, 15% IOC, 20% modify, ~2% duplicate cancels/modifies, realistic depth | `replay_mix` is a deterministic 100k mixed fixture with fees, stops and icebergs, not calibrated to a market mix | Add a benchmark fixture with these ratios and duplicate/stale requests; keep the existing mixed fixture |
| Standing-book occupancy | Records average/peak resting orders and levels per scenario (~20.9k/41.8k in `static`) | `book_replay` has 1k/20k depth cases, but occupancy is not the headline variable | Done: the `--matrix` mode samples average/peak orders and levels for four calibrated shapes |
| Median of many runs | 10 perf runs, median and standard deviation | Best-of-5 for microbenchmarks; medians of 2-5 for e2e | Done for the matrix: 10 whole-replay trials with median, population stddev and CV; keep p50/p99 for the case-level benches |
| Full output hash plus state audit | Hashes every report and probes 192 random book states | Replay fixtures compare full event streams; property tests compare a reference model; cluster tests read `/read` | Done: `random_point_book_state_audit_matches_reference_model` compares BBO/depth at deterministic random indices |
| Separate edge-case battery | 34 hard-invariant cases outside the timed workload | `src/order_book/conformance_tests.rs` plus cluster/replay suites | Mapped; the missing hard invariants are now explicit in the benchmark conformance batch (see §5) |
| Held-out seeds | 10,000 precomputed seeds; private seeds for adversarial runs | Property tests use seeds and print failures; replay cases are fixed | Add a seed-sweep regression (e.g. 100 seeds) and keep a private-seed path for external validation |
| Report transport and pipeline barrier | Includes the matcher-to-publisher hand-off and `engine_flush()` inside the timed window | `e2e` includes HTTP, Raft, apply and response; book benches exclude events/feed | Keep the two scopes separate and label them; do not compare a book-only number with e2e |
| Open-loop latency under load | The paper reports latency-under-load; the harness itself measures throughput | Our sustained e2e is closed-loop | Add a paced/open-loop latency run, or state the coordinated-omission caveat where only closed-loop tails are reported |
| Adapter faithfulness | Book queries must come from the engine's real state | `/read` reads the real book; no shadow | Keep it that way; never let a test adapter synthesize a cleaner book |

What not to copy: the in-process C ABI, one-message dispatch, prebuild hook and audit machinery are
benchmark-specific. The benchmark's `modify` is **cancel + reinsert**, not our `Amend`; its cancel
and modify identity is a client order id, not our side + price + sequence. A future adapter must
translate those semantics rather than map `Modify` directly to `Amend`. The benchmark deliberately
leaves modify-priority conventions unspecified; our priority rules are ours and are already
documented.

### 4. What the competitor field says about book design

The survey's book-structure table is the clearest external evidence we have about the price ladder.
The figures are the benchmark's seed-23 worst-case throughput on Graviton4/Neoverse-V2 with
`-O3 -march=native`, median of 10 trials; they are not measurements on this host.

| Price structure | Best worst-case M/s | Engines | Representative | Relevance to us |
|---|---:|---:|---|---|
| Proprietary (paper) | 103.39 | - | FlashOne | Not an open implementation; a different algorithm, not a micro-optimization |
| Treap | 9.25 | 1 | `lcv3` | Fastest open structure; intrusive FIFO and fixed pools |
| Binary heap / priority queue | 8.19 | 14 | `e820` | Hybrid price-indexed arena plus heaps |
| Red-black tree / ordered map | 7.94 | 59 | `geseq/cpp-orderbook` | The field default (37% of engines) |
| Sorted vector | 7.86 | 17 | `melin` | Competitive without pointer-chasing |
| AVL tree | 7.26 | 5 | `CppTrader` | Balanced BST, close to us |
| **B-tree / B+tree** | **7.20** | **27** | **`raymondshe` (us)** | The common Rust default; category leader |
| Plain BST (unbalanced) | 6.88 | 1 | `Kautenja` | Fragile under adversarial insert order |
| Flat direct-indexed array (+ bitmap) | 5.60 | 14 | `asthamishra` | O(1) by tick but cache-thrashes a wide domain |
| Adaptive-radix / crit-bit | 2.625 | 3 | `serum` | Pointer-chasing dominates |
| Linked-list price levels | 1.97 | 2 | `coralme` | O(n) level lookup |
| Skip-list | 1.62 | 8 | `apex` | Pointer-chasing and lock-free overhead |
| Hash map + best-price rescan | 0.11 | 5 | `dydx` | O(all levels) best-price lookup |

Observations:

- **All open engines top out at 9.25 M/s**, while FlashOne reports 103.39. The ~94 M/s gap is an
  algorithmic difference, not a compiler-flag difference; do not expect a 10x win from a better
  `BTreeMap` constant.
- **Structure is not destiny.** The red-black tree is the default and tops at 7.94; the treap is
  represented by one engine and tops the open field at 9.25. Our `BTreeMap` beats every other
  B-tree and sits within 8-22% of the open leaders, so a structure swap is a measured experiment,
  not an obvious win.
- **Flat arrays lose on wide domains.** The 14 flat-array engines peak at 5.60, and the benchmark's
  non-conforming roster includes bounded-domain engines that drop orders or crash on
  `swing-40`/`flash-crash` (`mtengine`, `femto_go`, `zackienzle`). Our 18-decimal `Fixed` range and
  sparse `BTreeMap` are a correctness advantage; do not trade them for a tick-indexed array without
  a bounded-domain contract.
- **Implementation quality dominates structure.** The survey's 74 industry-authored engines include
  26 that need a fix and 28 that diverge, time out or crash. A 10,236-star vendor engine
  (`StockSharp`) broke FIFO through `Dictionary` enumeration, and a plain BST with a duplicate-id
  path caused a use-after-free (`Kautenja`). Seniority and stars are not correctness evidence.
- **Our own per-level prototype remains the right comparison.** `level_book` already showed mixed
  results (1.84x match, 3.31x cancel, but 0.75x sparse amend and 0.87x BBO advance). The external
  result does not overturn that; a treap, sorted vector or heap hybrid is worth a book-only A/B only
  if it wins on our workloads too.

### 5. Competitor failure taxonomy and our guards

The survey's most useful output for us is not the leaderboard; it is the recurring failure classes.
Each row is a bug class that appeared in at least one competitor at its pinned commit.

| Failure class | Representative findings | Our guard | Remaining action |
|---|---|---|---|
| FIFO/time-priority loss | `swap_remove` cancel (`pyrsquant`), LIFO insertion (`amer`, `hillside6`), unstable sort (`nirvanasu`, `thelilypad`), `Dictionary` order (`StockSharp`), shared-level sibling loss (`shaunlwm`) | `BTreeMap` keyed by `(price, sequence)`; property model; amend-priority matrix | Covered by `cancel_head_middle_and_scattered_orders_preserve_fifo` |
| Wrong execution price | Aggressor price (`e820`, `jiker_burce`, `apex`, `robaho`, `kodoh`, `shal`), midpoint (`fmstephe`), sell-side-only price (`dx1ngy`) | Fills use the maker price; the reference model asserts `fill.maker.price` | Covered by `fills_print_at_the_maker_price_in_both_directions` |
| Phantom zero-quantity fill / bystander mutation | This repository (#1), `matchina`, `amansardana`, `aas2015001`, `zorrofix` | Taker-exhaustion guard; exact-fill and full-sweep regression tests | None; keep the regression pinned |
| Stale/phantom best price or level | `osmosis`, `robdev`, `mmrath`, `cjboxing`, `mercury` | No cached BBO: `best_bid`/`best_ask` read the tree; `basic_best_*` and the model | Covered by `cancel_best_level_updates_bbo_without_a_follow_on_sweep` |
| Partial-fill non-conservation / over-match | `limitbook`, `cltwski`, `opexdev`, `pyme` | `partial_fill_preserves_maker_identity`, `resting_volume`, property model | Add a repeated partial-fill-across-calls case |
| IOC/marketable residual mishandled | `robdev` (IOC residual rests), `qa-rs` (market remainder rests), `knocte` (crossed book), `sadhbh` (exact touch) | `ioc_cancels_its_remainder`, market tests, `exact_fill_stops_before_deeper_levels` | Covered by `multi_level_ioc_fills_best_price_first_and_cancels_remainder` and `marketable_limit_residual_rests_at_its_limit` |
| Stale cancel/modify acked | `cjboxing`, `mercury`, `robdev`, `gotrader`, `gavincyi` | `Amend` returns `NotFound`; `Cancel` now also publishes `Rejected(OrderNotFound)` | Covered by `cancel_and_amend_of_a_filled_order_are_rejected`, `apply_rejects_cancel_and_amend_of_a_fully_filled_order` and the updated GTD/cancel test |
| Duplicate id / id reuse / capacity drop | `Kautenja`, `qa-rs`, `tembolo`, `onewhitedevil` | Primary key is `sequence`, not `Order.id`; `client_order_id` dedupe; no fixed order-id table | `order_id_is_client_metadata_not_a_book_key` tests duplicate ids; submission-map eviction is covered by `event_log_retains_pages_and_dedupes_submissions` |
| Bounded price domain | `asthamishra` (fixed), `mtengine`, `femto_go`, `zackienzle`, `Clober`, `DeepBook` caps | Full-range `Fixed` plus sparse `BTreeMap` | Covered by `near_limit_prices_rest_and_cancel` |
| Empty-book / deep-sweep crash | `dylanlott`, `laffini`, `raunakchopra`, `zorrofix` | Empty-book, market-without-liquidity and property tests | Covered by `deep_same_price_sweep_conserves_every_fill` |
| O(n)/O(n²) cancel or scan | `afterworkguinness` (whole-book string), `mattdavey` (rescan), `redisexchange` (never stops), `liqian` (domain scan) | `BTreeMap` O(log n); `book_replay` deep cancels | None for now; keep the benchmark |
| Expired maker still trades | `realyarilabs`; llc-993's `AdvancedOrderBook` | GTD purge before matching; `a_place_purges_due_gtd_before_matching` asserts `fills.is_empty()` when the maker expires | Covered; keep the explicit expiry-before-match assertion |
| Async/non-deterministic matcher | `luminengine` | `apply` is serial and deterministic; replay/restart/cluster tests | None |
| Modify semantics ambiguity | Benchmark leaves priority unspecified; our `Amend` refuses crossing, while the benchmark's `modify` is cancel + reinsert | Explicit priority matrix and `AmendError::WouldCross`; documented in `AGENTS.md` | Keep the divergence documented; never map benchmark `Modify` directly to `Amend` |

### 6. Selected competitor notes

| Engine | Structure / result | Benchmark finding | Lesson |
|---|---|---|---|
| `lcv3` | Treap + intrusive FIFO; 9.25 M/s; clean | Fastest open engine; author-permitted license | The treap is the external benchmark's best open ladder; the license is not a code source |
| `e820` | Price-indexed arena + heaps; 8.19 M/s | Singly-linked orphan and aggressor-price bugs fixed | Hybrid structures can beat trees, but FIFO/price invariants still decide correctness |
| `geseq/cpp-orderbook` | RB tree; 7.94 M/s; clean | Near-top with the field's default structure | A clean RB implementation is competitive; structure alone is not the differentiator |
| `melin` | Sorted vector; 7.86 M/s | Stop-cascade single-pass bug fixed | Sorted vectors are competitive; stop cascades need explicit re-evaluation tests |
| `CppTrader` | AVL + pools; 7.26 M/s | Off-path FOK/AON pricing bugs | Feature breadth needs tests outside the canonical workload |
| `raymondshe` (us) | B-tree; 7.20 M/s | Phantom zero-qty/bystander bug; fixed at `a44d9ae` | The external oracle found a real bug our own tests then pinned; keep the regression |
| `llc993` | B-tree + slab + intrusive time queue; 7.14 M/s; clean | Conforms on the new/cancel/modify ABI | Benchmark conformance does not cover its advanced book, risk, WAL or snapshot gaps (Part III) |
| `asthamishra` | Flat array + bitmap; 5.60 M/s | Bounded 100k-tick domain; fixed by a bounds check | Flat arrays lose on wide swings; consistent with Part I |
| `matchcore` | Rust; 6.58 M/s | Marketable limit passed `None` and swept like a market order | Enforce the limit on every cross, including the modify path |
| `matchina` | Rust; 1.60 M/s | Same phantom zero-quantity class as our #1 | Our bug was not unique; a taker-exhaustion guard is a standard invariant |
| `Kautenja` | Plain BST; 6.88 M/s | Duplicate live order id caused a use-after-free | The id table needs duplicate/reuse semantics, not only a tree |
| `StockSharp` | C#; 1.64 M/s | `Dictionary` enumeration broke same-price FIFO | Stars and vendor status are not a FIFO proof |

### 7. Benchmark-derived backlog

The following are the concrete changes this review supports. They are ordered so the cheap
correctness and measurement improvements land before any structural experiment.

**Completed 2026-11-10**

- **Explicit conformance cases.** `src/order_book/conformance_tests.rs` now covers FIFO cancel
  head/middle/scattered, best-level BBO reseating, both-direction maker price, multi-level IOC,
  marketable-limit residual, a 5,000-fill sweep and near-limit prices.
- **Unknown-cancel contract.** `apply` now publishes `Rejected` with `OrderNotFound` for a cancel
  miss (and still consumes one sequence). `apply_rejects_cancel_and_amend_of_a_fully_filled_order`,
  `cancel_and_amend_of_a_filled_order_are_rejected` and the file-sink cluster test cover it;
  `docs/API.md`, `docs/MATCH_RESULTS.md` and `AGENTS.md` document the contract.
- **Identity documentation.** `Order.id` is client metadata, `sequence` is the book key and
  `client_order_id` is a bounded `Place`-only idempotency key; duplicate ids are tested in
  `order_id_is_client_metadata_not_a_book_key`.
- **Random-point state audit.** `random_point_book_state_audit_matches_reference_model` compares
  best bid, best ask and depth at 64 deterministic indices per seed.
- **Occupancy scenario matrix.** `cargo bench --bench book_replay -- --matrix` builds four
  standing-book shapes calibrated to the survey's seed-23 averages (static 20.9k/60 down to
  flash-crash 240/210), samples average/peak orders and levels, and reports the worst-case
  median-of-10 with standard deviation and CV. The recorded macOS result is in
  [`docs/PERFORMANCE.md`](./PERFORMANCE.md).

**P2 - measurement and structure**

1. Add a paced/open-loop latency-under-load run to `e2e` or `cluster_e2e`, or state explicitly in
   `PERFORMANCE.md` that closed-loop tails understate queueing. The benchmark's worst-case
   throughput argument needs a latency companion.
2. Evaluate a treap, sorted vector or heap hybrid only as a book-only A/B on our workloads. The
   external gap to `lcv3` is 22%, but `level_book` already showed that per-level buckets trade
   wins and losses; do not switch on a single external leaderboard.
3. Re-pin the benchmark to current `HEAD` and, if the adapter can be published, ship the
   reproduction. Run a held-out seed and the 10,000-seed `validate_seeds.py` path only if we are
   prepared to maintain the C ABI adapter as a separate artifact.
4. Add a "benchmark scope" paragraph to `PERFORMANCE.md` that records the 7.20 M/s book-only result,
   the pin, the fixed issue and the non-comparability with e2e. Link back to this part.

**P3 - do not chase**

5. The benchmark has no FOK, stop, iceberg, post-only, GTD, fee, asset, persistence, consensus or
   multi-symbol coverage. Those remain covered by our own conformance, property, replay and
   cluster suites; do not weaken them to match a simpler ABI.

### 8. Verdict

The benchmark validates two things: our `BTreeMap` price ladder is a credible top-tier open
structure, and an independent differential oracle can find a real correctness bug that normal
happy-path tests miss. It also demonstrates the limit of that validation: 7.20 M/s is a
matching-only number, and the survey's 34 cases do not exercise the order types, durability,
replication and exact-fee semantics that distinguish this project.

The highest-value transfer is therefore **methodology**, not code: worst-case scenario reporting,
median-of-10 with variance, a calibrated order flow, random-point state audits, and a separate
edge-case battery. The benchmark-derived correctness and occupancy envelope are now in place; the
remaining measurement work is open-loop latency and structural experiments, not a new data
structure.

## Part V - Comparison with matchengine-raft

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
| FlashOne survey (Part IV) | 5.60 M/s, with fix; bounded-domain flat array | Not listed in the roster | 7.14 M/s, as shipped; new/cancel/modify ABI only | 7.20 M/s, fastest B-tree; "with fix" at the old pin, fixed at `HEAD` |

Our engine is stronger on semantics, durability, exactness and recovery. Astha is stronger on
single-node hot-path layout, binary gateway work and measurement scope. Wire is stronger on
low-level allocator/MPMC experiments and honest scope disclosure, but its v9 matching kernel is
narrower than its name suggests. llc-993 is the broadest feature skeleton, but its advanced order
types are not wired into the exchange, its WAL/snapshot is not crash-safe, and its benchmark numbers
measure a separate `AdvancedOrderBook`, not the product pipeline.

On the FlashOne benchmark's matching-only ABI, our `BTreeMap` core is the fastest B-tree entry and
within about 8% of the fastest Rust engine. That is a useful external validation of the price ladder
and the basic FIFO loop; it is not a validation of the replicated product path. The benchmark's
`modify` is cancel + reinsert and its identity is a client order id, so a direct mapping to our
`Amend` would be wrong. Its "conforming" verdict is scoped to limit new/cancel/modify and IOC; it
does not exercise FOK, stops, icebergs, fees, persistence or consensus.

## Part VI - Consolidated takeaways

### Adopt

1. **Measurement-scope discipline.** Done: `docs/PERFORMANCE.md` has the benchmark-scope table,
   closed-loop/timer notes and warm/cold p50/p99 reporting.
2. **A pure order-book replay benchmark.** Done: `book_replay`, `replay_mix` and `level_book` keep
   Raft/HTTP out of the book-only numbers.
3. **Fuzzing.** Done: `fuzz/` builds targets for the order book, WAL, snapshot and wire decoders;
   keep them buildable.
4. **Worst-case scenario matrix (benchmark-derived).** Done:
   `cargo bench --bench book_replay -- --matrix` builds the four calibrated shapes, samples
   average/peak orders and levels, and reports the worst-case median-of-10 with stddev/CV.
5. **Random-point state audit (benchmark-derived).** Done:
   `random_point_book_state_audit_matches_reference_model` compares best bid, best ask and depth
   at deterministic random indices against the reference model.
6. **Explicit benchmark-derived conformance cases.** Done: `src/order_book/conformance_tests.rs`
   covers FIFO cancel head/middle/scattered, best-level BBO reseating, both-direction maker price,
   multi-level IOC, marketable-limit residual, a 5,000-fill sweep and near-limit prices.
7. **Unknown-cancel and identity semantics (benchmark-derived).** Done: a cancel miss publishes
   `Rejected(OrderNotFound)` and still consumes one sequence; `docs/API.md`, `docs/MATCH_RESULTS.md`
   and `AGENTS.md` document `Order.id`, `sequence` and `client_order_id`; duplicate ids are tested.
8. **Open-loop latency (benchmark-derived).** Add a paced/open-loop latency run, or state the
   coordinated-omission caveat where only closed-loop tails are reported. Worst-case throughput
   needs a latency companion.
9. **Preallocation, cache layout and allocation accounting.** `apply_path --check` enforces the
   allocation ceilings and `level_book` measured the per-level prototype. Keep structural changes
   conditional on a book-only A/B.
10. **A reusable MPMC reference.** If we ever need a custom bounded queue, the Vyukov pattern is a
    useful reference; Tokio `mpsc` remains the default.

### Adapt / evaluate later

- A treap, sorted-vector or heap-hybrid price ladder, only after a book-only A/B on our workloads.
  The external gap to `lcv3` is 22%, but `level_book` already showed the per-level prototype trading
  match/cancel wins for sparse-amend/BBO losses.
- Re-pin the FlashOne benchmark to current `HEAD` and publish the adapter/reproduction if the C ABI
  can be maintained as a separate artifact. Held-out seeds and `validate_seeds.py` are optional
  external validation, not a CI dependency.
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

**Completed 2026-11-10**

1. Occupancy scenario matrix. Done: `cargo bench --bench book_replay -- --matrix` reports the four
   standing-book shapes, average/peak orders and levels, and the worst-case median-of-10 with
   stddev/CV; the baseline is in `docs/PERFORMANCE.md`.

**P2 - measurement and structure**

2. Add a paced/open-loop latency run, or document the closed-loop caveat in `PERFORMANCE.md`.
3. Evaluate a treap, sorted vector or heap hybrid only as a book-only A/B; do not switch on the
   external leaderboard alone.
4. Re-pin the FlashOne benchmark and record the book-only result, pin and scope in
   `PERFORMANCE.md`; publish the adapter only if it can be maintained.

**P3 - optional platform work**

5. Real-data replay, custom MPMC/SIMD/io_uring, binary order entry and feature-gated histograms,
   only after a measured bottleneck and a deployment decision.

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

### flash1-dev/matching-engine-benchmark

- <https://github.com/flash1-dev/matching-engine-benchmark>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/README.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/docs/METHODOLOGY.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/docs/ANTI_CHEAT.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/docs/CONFORMANCE.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/docs/INTEGRATION.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/CONSENSUS_CONFORMING_ENGINES.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/NON_CONFORMING_ENGINES.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/INDUSTRY_AUTHORED_ENGINES.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/CORRECTNESS_FINDINGS.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/SNAPSHOTS.md>
- <https://github.com/flash1-dev/matching-engine-benchmark/blob/main/workload/scenarios.json>
- <https://arxiv.org/abs/2606.01183>
- This project's finding: <https://github.com/raymondshe/matchengine-raft/issues/1>
