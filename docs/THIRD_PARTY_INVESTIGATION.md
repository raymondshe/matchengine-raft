# Third-party matching-engine investigations

Six external matching-engine projects were reviewed for design ideas, benchmark methodology and
failure modes, plus the Binance Spot market-data API as the contract reference for a planned
historical kline system. This document consolidates the separate reports into one reference.

| Project | Revision | License | Primary value to us |
|---|---|---|---|
| [AsthaMishra/matching-engine](https://github.com/AsthaMishra/matching-engine) | `2a74c3f` (2026-08-14) | No license file exposed | Hot-path order-book layout, binary gateway, measurement discipline |
| [wasif-exe/wire](https://github.com/wasif-exe/wire) | `abd2bf2` (2026-10-19) | MIT | Low-level layout/allocator/MPMC ideas and a cautionary benchmark case |
| [llc-993/matching-core](https://github.com/llc-993/matching-core) | `2cb21c0` (2026-01-03) | No license file exposed | Feature breadth, WAL/snapshot attempt, integration/benchmark caution |
| [flash1-dev/matching-engine-benchmark](https://github.com/flash1-dev/matching-engine-benchmark) | `424e4e5` (2026-10-16) | MIT (vendored parts have their own licenses) | 249-engine differential benchmark, consensus oracle, conformance battery, worst-case methodology |
| [ronitgupta138/Apex-Matching-Engine](https://github.com/ronitgupta138/Apex-Matching-Engine) | `a31909c` (2026-11-13) | MIT | Readable Java/Spring teaching engine; a case study in documentation-vs-code and benchmark scope |
| [zzxscodes/pitlane](https://github.com/zzxscodes/pitlane) | `ece9c8a` (2026-11-21, v1.0.0) | MIT | Shared-memory fail-closed desk, ordered pre-trade gate, mode-isolated RBAC and a disciplined CI/latency setup |
| [Binance Spot API](https://github.com/binance/binance-spot-api-docs) | `a049a2b`/`adfd443` (2026-11-11) | Documentation (API terms apply) | Kline publication cadence and finality, dense-bar semantics, archive layout, backfill economics |

Review date: 2026-11-01, re-reviewed 2026-11-04, benchmark review added 2026-11-10, Apex review
added 2026-11-16, pitlane review added 2026-11-21, Binance API review added 2026-11-22. The review used the public READMEs, `PLAN.md`, the
benchmark's reports and the key source files listed in the references; the 2026-11-04 pass added
AsthaMishra's `matching-core/optimization_notes.md`, the 2026-11-10 pass added Part IV, and the
2026-11-21 pass read pitlane's README, CI workflow, build file and three headers (`FanRing`,
`ReplicaLog`, `PreTradeGuard`). Tests for the other five projects were not
executed here. For Apex, the engine/model classes were compiled directly and a local harness reran
its benchmark shape plus a same-price cancellation probe on 2026-11-16 (Apple M4, OpenJDK 25); its
Spring test suite was not run. The 2026-11-22 Binance pass read the published
`binance-spot-api-docs` specifications and downloaded and decompressed archive samples from
`data.binance.vision`; the live REST and WebSocket hosts were unreachable from this network, so
the runtime cadence claims there are doc-derived and flagged as such. No code was copied; all seven
references are documentation or reference material only.
Pitlane is MIT licensed, but its single commit and one-star history give it no independent track
record — every claim below is treated as self-reported unless the source was checked. The
performance-specific verdicts (what transfers to matchengine-raft and what does not) live in
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
not current code; clearing the "with fix" label is the benchmark maintainer's call. Our own
current-HEAD re-pin at `ee07ba0` measured a **3.62-4.23 M ops/s** worst-case `static` median
(median 3.68) with `book_replay --matrix`; the local scope and the adapter decision are recorded
in [`docs/PERFORMANCE.md`](./PERFORMANCE.md#current-head-book-only-re-pin-2026-11-10). The pinned
commit declared `MIT/Apache-2.0`; the current repository is MIT.

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
| Open-loop latency under load | The paper reports latency-under-load; the harness itself measures throughput | `e2e --rate` adds a fixed-rate open-loop schedule with scheduled-send, dispatch-lag and service percentiles; the closed-loop run is still reported | Done: 64-client rows at 10k/30k/50k/80k offered writes/s plus the coordinated-omission caveat in `docs/PERFORMANCE.md` |
| Adapter faithfulness | Book queries must come from the engine's real state | `/read` reads the real book; no shadow | Keep it that way; never let a test adapter synthesize a cleaner book |
| External adapter publication | The benchmark expects a maintained engine adapter | No FlashOne adapter is published; the local `book_replay --matrix` baseline is maintained and reads the real `OrderBook` | Do not publish an adapter unless the semantic translation (client-id cancel+reinsert `modify`, ABI, queries) can be maintained externally |

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
   `PERFORMANCE.md` that closed-loop tails understate queueing. Done: `e2e --rate` reports
   scheduled-send, dispatch-lag and service percentiles, and the 10k/30k/50k/80k rows plus the
   caveat are in [`docs/PERFORMANCE.md`](./PERFORMANCE.md#open-loop-write-latency-2026-11-10).
2. Evaluate a treap, sorted vector or heap hybrid only as a book-only A/B on our workloads. Done:
   `match_engine/benches/sorted_book.rs` compares a sorted-vector ladder; it wins match/cancel/amend-volume but
   loses price-move amend and BBO advance, so it is not adopted. A tree/heap hybrid remains a
   book-only option only if a sparse-level fast path is measured.
3. Re-pin the benchmark to current `HEAD` and, if the adapter can be published, ship the
   reproduction. Done: the local book-only re-pin is `ee07ba0` (see
   [`docs/PERFORMANCE.md`](./PERFORMANCE.md#current-head-book-only-re-pin-2026-11-10)). No FlashOne
   adapter is published because it would need external maintenance and semantic translation.
4. Add a "benchmark scope" paragraph to `PERFORMANCE.md` that records the 7.20 M/s book-only result,
   the pin, the fixed issue and the non-comparability with e2e. Done: the scope and re-pin
   subsections carry the 7.20 M/s external pin, the local 3.62-4.23 M ops/s re-pin, the fixed
   phantom-fill issue and the e2e non-comparability.

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
open-loop latency and sorted-vector experiments are also recorded. The remaining work is optional
external-adapter publication and the manual Linux pinned/Aeron validation, not a new data
structure.

## Part V - ronitgupta138/Apex-Matching-Engine

### 1. What it is

A compact Java 21 / Spring Boot 3.3.5 continuous double auction engine. The repository is MIT
licensed, was created on 2026-10-04 and last pushed on 2026-11-13 (`a31909c`); it had 69 stars,
five open issues and four open pull requests when reviewed. The main code is 19 classes and about
900 lines:

- `engine/OrderBook`: the per-symbol book and matching loop.
- `service/MatchingEngineService`: symbol-to-book map, order placement, cancel and market-data
  queries.
- `controller/`: REST endpoints for orders, depth, trades and symbols.
- `service/MarketBroadcaster` and `config/WebSocketConfig`: STOMP/WebSocket market data.
- `simulator/MarketSimulationBot`: a scheduled market-maker bot, enabled by default.
- `model/`, `dto/`: `BigDecimal` prices, `long` quantities, order/trade/depth records.

The package is a Spring Boot demo: Docker multi-stage build, Actuator health/metrics, H2/JPA and
PostgreSQL dependencies, a test suite and a benchmark-shaped JUnit test. It has no persistence,
consensus, replication or recovery.

The open issues are a useful map of what is not implemented in `main`: LMAX Disruptor evaluation
(#1), a primitive `LongObjectHashMap` index (#2), binary WebSocket market data (#3), a
multi-threaded stress test with cancels (#4) and a CI benchmark regression check (#5). Treat the
deeper `docs/` pages as design proposals, not as a description of the current code.

### 2. Order book and matching

The book is deliberately simple:

- `bids` is a `TreeMap<BigDecimal, ArrayDeque<Order>>` with reverse ordering; `asks` is the same
  tree in ascending order.
- `orderIndex` is a `ConcurrentHashMap<Long, Order>` for lookup.
- A **fair** `ReentrantLock(true)` guards `processOrder`, `cancelOrder` and `getMarketDepth` per
  symbol.
- `tradeHistory` is an unbounded `Collections.synchronizedList`.
- Matching walks the opposite tree in price order, stops when a limit would cross, and executes
  FIFO inside each `ArrayDeque`. The maker sets the execution price. A market residual is marked
  `CANCELLED`; a limit residual rests.

The basic price-time-priority path is correct and easy to follow. The important gaps are structural:

- **Cancel is O(n), not O(1).** `ArrayDeque.remove(order)` scans from the head. The README and book
  docs claim O(1) cancellation. A local probe with 100,000 orders resting at one price, cancelling
  the middle element, measured about 199k cancels/s (p50 4.8 us, p99 14.5 us); the same harness
  with one order per price level measured about 8.2M cancels/s (p50 0.083 us). The benchmark shape
  never exposes this because it rarely builds a deep queue at one price.
- **The order index never shrinks.** Filled and cancelled orders stay in `orderIndex`; every order
  ever submitted is retained. `tradeHistory` is also unbounded. There is no eviction, compaction or
  retention policy, so a long-running process grows without bound.
- **Depth is computed under the matching lock.** `getMarketDepth` streams every order in each
  selected level to sum remaining quantity. That is O(orders at the level), not the lock-free
  `ConcurrentSkipListMap` / O(log P) design described in `docs/L2_Market_Depth_Loom_Concurrency.md`.
- **The fair lock serializes all matching, cancel and depth operations.** A depth query blocks
  matching and vice versa. Fairness adds handoff cost without a latency benefit for this workload.
- **The response, trades and depth are not one atomic snapshot.** `placeOrder` matches under the
  book lock, releases it, then takes the lock again for a depth snapshot. Another order can arrive
  between the two, so the HTTP response, trade list and broadcast depth need not describe the same
  instant.
- **No STP, risk checks, tick/lot rules or amend.** Only `LIMIT` and `MARKET` exist; there is no
  IOC/FOK/GTD/Day, stop, iceberg, post-only or market-to-limit order. There is no self-trade
  prevention, price band, maximum order size, tick/lot rounding or symbol whitelist. Bean
  Validation covers required fields and quantity ≥ 1; the only price check is that a `LIMIT` price
  is greater than zero.
- **Non-deterministic identity and time.** Order and trade ids are process-local `AtomicLong`s;
  `Order.createdAt` and `Trade.executedAt` use `Instant.now()`. Replaying the same requests cannot
  reproduce ids or timestamps, and a restart loses the id generators.
- **Index insertion precedes matching.** A duplicate explicit order id could overwrite the index
  entry of a resting order while that order remains in the book. The REST path never supplies ids,
  so this is not reachable through the API today, but the invariant is unguarded.
- **Reads create books.** `getMarketDepth` and `getRecentTrades` call `getOrCreateOrderBook`, so an
  unknown-symbol GET creates an empty book and `getActiveSymbols` pre-populates five symbols even
  when empty.

### 3. Runtime, API and persistence

- **REST:** `POST /api/orders`, `GET`/`DELETE /api/orders/{symbol}/{id}`,
  `GET /api/market/depth/{symbol}`, `/api/market/trades/{symbol}` and `/api/market/symbols`.
- **CORS and auth:** controllers use `@CrossOrigin(origins = "*")`; there is no authentication,
  authorization, rate limit or idempotency key. Retrying a POST creates another order. Any symbol
  string creates a new book, so an unauthenticated client can grow the symbol map without bound.
- **WebSocket:** Spring's simple STOMP broker with SockJS, allowed origin patterns `*`. The
  broadcaster sends a full 15-level depth snapshot after **every** order and cancel, and one message
  per trade. There is no conflation, throttling or drop policy; a failed send is logged at debug and
  discarded.
- **Documented but absent:** `docs/STOMP_WebSocket_Order_Stream.md` describes 50 ms periodic top-20
  depth, FlatBuffers/binary payloads and an authenticated `/user/queue/orders` private feed. The
  code has none of these: it uses JSON, the simple broker, no user destinations and no security.
- **Simulator:** `application.yml` sets `trade.simulator.enabled: ${SIMULATOR_ENABLED:true}` and
  `docker-compose.yml` sets `SIMULATOR_ENABLED: "true"`. The bot injects BTC-USD bids/asks every
  four seconds and occasionally a market order. A deployment that wants only client order flow must
  set `SIMULATOR_ENABLED=false`; the default demo has background liquidity and trades.
- **No durability:** JPA, H2 and the PostgreSQL driver are dependencies and a datasource is
  configured, but there are no entities or repositories. All state is in memory and is lost on
  restart. There is no WAL, snapshot, replay, Raft or other replication.
- **Deployment:** the Dockerfile is a multi-stage build and runs as a non-root user, which is good.
  It does not set ZGC, heap sizing beyond `MaxRAMPercentage`, CPU affinity or other low-latency JVM
  options, despite the benchmark docs' ZGC claims.

### 4. Claims vs code

The README is mostly honest at a high level; several deeper `docs/` pages describe a different
engine. Where they conflict, the code wins:

| Claim | Where | Current code |
|---|---|---|
| 1.24M orders/s, p50 0.62 us, ZGC 0 ms | `docs/benchmarks/Million_Order_Per_Second_Benchmark.md` | 100k sequential LIMIT orders on a shallow book, no ZGC flags, no assertion; a local rerun of the same shape measured 2.08M ops/s, p50 0.17 us on an M4/JDK 25 — a different machine and still not end-to-end |
| 420k orders/s, p50 0.12 ms | `docs/benchmarks/Sub_Millisecond_Latency_Profile.md` | no committed harness reproduces it; internally inconsistent with the 1.24M claim |
| 50,000+ orders/s, sub-millisecond | README | conservative; not a measurement of the REST/STOMP path |
| `ConcurrentSkipListMap`, lock-free read paths, O(log P) depth | `docs/L2_Market_Depth_Loom_Concurrency.md` | `TreeMap` + fair `ReentrantLock`; depth sums under the lock |
| Doubly linked FIFO with O(1) detach; atomic cancel/replace amend | `docs/orderbook/Order_Cancellation_Deterministic_Semantics.md` | `ArrayDeque.remove(order)` scans the queue; there is no amend method or endpoint |
| Java 21 Loom / virtual threads | benchmark docs | no `Thread.ofVirtual` or virtual-thread configuration; the Disruptor evaluation is still an open issue |
| LMAX Disruptor / lock-free ring buffer | benchmark docs | no Disruptor dependency; issue #1 |
| Binary WebSocket / FlatBuffers | STOMP docs | JSON over Spring's simple STOMP broker; issue #3 |
| Primitive `LongObjectHashMap` | issue #2 / docs | `ConcurrentHashMap<Long, Order>` |
| Authenticated `/user/queue/orders` | STOMP docs | no Spring Security, no user destinations, no private feed |
| 50 ms periodic top-20 depth | STOMP docs | full 15-level depth on every mutation |
| ZGC pauses < 0.4 ms | `docs/benchmarks/Worst_Case_Jitter_Distribution.md` | no ZGC selection or GC metrics in the image or benchmark |

Some of these pages label themselves as an "Architectural specification and design document", so
they are best read as a roadmap. The README's architecture diagram is closer to reality: it names a
`ReentrantLock` and a navigable red-black tree.

### 5. Testing and benchmarks

The suite is small: four `OrderBookMatchingTest` cases (exact limit match, partial fills/FIFO,
cancel, market order), two integration tests (health, submit/fetch) and one benchmark-shaped JUnit
test. There are no tests for FOK/IOC/stops/STP, invalid prices, duplicate ids, deep same-price
queues, concurrency, restart or persistence. CI runs `mvn clean verify`; the benchmark prints
numbers but asserts nothing, so it cannot catch a regression.

The committed benchmark:

- warms up 20,000 orders, then discards the book;
- submits 100,000 LIMIT orders with prices around 60,000 ±50 and alternating sides;
- creates the order before the timer and times only `processOrder`;
- sorts per-order latencies and prints p50/p90/p99/p99.9;
- uses one thread, a shallow/empty book, no cancels or amends, no HTTP/JSON/STOMP and no GC or JVM
  reporting.

A local rerun of that shape measured about 2.08M ops/s, p50 0.167 us, p99 1.25 us, p99.9 4.96 us
and a 6.49 ms max on an Apple M4 with OpenJDK 25. That number is not evidence of exchange
throughput: it measures an in-memory book with a narrow price range and alternating flow, on
different hardware, with order construction outside the timer. The harness was ad hoc and is not
checked in; treat these numbers as a local cross-check, not a reproducible benchmark. A same-price
cancellation probe is a better stress case: with 100,000 resting orders at one price, cancelling
middle elements measured about 199k/s (p50 4.8 us, p99 14.5 us), versus about 8.2M/s when each
price level holds one order. The gap is the `ArrayDeque.remove` scan the O(1) claim hides.

### 6. Findings and risks

- **Good basic semantics, narrow scope.** Price-time priority, maker pricing, partial fills, market
  residual cancellation and status transitions work for the two supported order types.
- **No self-trade prevention.** A user can trade with their own resting order; there is no owner
  check, STP mode or self-trade event.
- **No idempotency.** A client retry after a timeout creates a second order; there is no
  `client_order_id` equivalent or dedupe map.
- **No edge controls.** CORS `*`, no auth, no rate limit, arbitrary symbol creation and a
  default-on market-maker bot are acceptable for a demo and unacceptable for an exposed service.
- **Unbounded memory.** Every order stays in `orderIndex`; every trade stays in `tradeHistory`;
  neither has retention or eviction. A long-running demo grows until the JVM heap is exhausted.
- **No durability or consensus.** A restart loses the book, ids, trades and simulator history. The
  JPA/H2 dependencies do not change that; they are unused for the engine.
- **Latency claims are not product measurements.** The benchmark is book-only, shallow, sequential
  and not asserted; the docs' 1.24M/420k/50k figures conflict with each other and with the code.
- **The depth/feed path shares the matching lock.** Depth snapshots and the full-snapshot broadcast
  add work to the request path instead of a shadow-book/feed task. Under load this trades tail
  latency for implementation simplicity.
- **No risk or validation layer.** Bean Validation covers required fields and quantity ≥ 1, but
  there is no symbol whitelist, tick/lot, max quantity, price band, duplicate-id or user balance
  check.

### 7. Strengths and lessons

The project is a readable, well-packaged teaching demo. The model/engine/service/controller split is
clear, the basic FIFO and L2 depth logic is compact, Docker and CI are present, and the MIT license
makes it easy to study. The README's badges and diagrams are polished; the deep docs are a useful
feature checklist even where they describe planned work.

For matchengine-raft, the lessons are mostly confirmations:

- **Keep claims code-backed.** Mark planned versus implemented explicitly; the deep docs here
  describe Loom, Disruptor, FlatBuffers, lock-free reads and ZGC that `main` does not contain. Our
  docs should continue to separate measured behavior from design notes.
- **Benchmark the real shape.** A shallow, alternating, book-only loop cannot justify an exchange
  throughput claim. Deep same-price queues, cancels/amends, the HTTP/WS path, GC and an asserted
  regression bound are all required; our `book_replay`, `replay_mix` and `apply_path --check` cover
  the first part, and `e2e` covers the integration path.
- **Use fixed-point and deterministic identity.** `BigDecimal` price keys and `Instant.now()` on
  the matching path are simple to write and impossible to replay; our 18-decimal `Fixed` and
  `(log_index, ordinal)` event ids are the right trade.
- **Bound the state.** Remove filled/cancelled orders from lookup indexes and cap trade/event
  history; a demo can leak and a production process cannot.
- **Do not put the feed on the matching lock.** Snapshot depth after the lock is released, or better,
  build it from a shadow book as our market-data task does.
- **Protect the edge.** CORS `*`, no auth, no idempotency and default-on simulation are demo
  choices; our gateway-owned TLS/auth, bounded request limits, admission gate and `client_order_id`
  dedupe are the production equivalents.

No code should be adopted from this repository. Treat it as a readable Spring teaching reference
and a cautionary example of benchmark/documentation drift, not as a low-latency or production
engine.

## Part VI - zzxscodes/pitlane

### 1. What it is

A C++17 header-only trading kernel plus `pitlane-trading`, a CTP/paper console. The repository is
MIT licensed, has **one commit** (`ece9c8a`, 2026-11-21, "pitlane 1.0.0", ~101.8k added lines), one
star and no forks. That single-commit history matters: there is no independent review or bug-fix
trail, so claims are treated as self-reported unless source was checked.

The kernel is a fixed-layout **shared-memory desk**: market data, strategy, risk and clearing run as
separate processes that meet at one `PITLANE1` memory-mapped region rather than calling each other.
Prices are integers, timestamps microseconds. The library opens no exchange or broker socket; the
optional UDP layer carries pitlane-to-pitlane frames only. The only runnable program is the trading
console. The hot path is explicitly lock-free, allocation-free and I/O-free; startup may allocate or
mmap. The desk is versioned (`kDeskVersion` 8, magic `PITLANE1`) and processes sharing it must all
restart together — `desk_view` refuses an older layout rather than migrating it.

The scope is much broader than a matching engine: order lifecycle (OMS/EMS/SOR/PMS/RMS), position
and cash ledgers, clearing, factor research, backtests, block tapes, a C ABI, a Python bridge and a
ten-tab web console with RBAC. Paper matching covers 256 instruments and 1024 working orders; the
docs use Chinese futures/CTP semantics (seven offset codes, close-today/close-yesterday, margin
trading, corporate actions). There is no external consensus service: the optional replication is a
fixed-capacity in-process log (see §3).

### 2. What the shared desk actually guarantees

`include/pitlane/desk/ring.hpp` was read directly. `FanRing<T, Cap>` is a single-producer ring where
each of up to 64 readers owns a bit:

- `claim()` returns `nullptr` when the ring is full; `publish_batch()` writes n slots and advances
  the tail once, failing and leaving the ring unchanged if any slot is still busy.
- A slot carries `need` (required reader bits), `ack` and `seq`. `blocked(pos)` refuses reuse while
  a required reader has not acked; with `need == 0`, the first reader to ack claims the slot.
- `view()`/`poll()` double-check `state` and `seq` after reading, so a concurrently reused slot is
  detected rather than returning torn data.
- `ack()` uses CAS for `need == 0` and `fetch_or` for named readers; `ack_span()` stops at the first
  position the reader cannot ack.
- Each `Slot` is cache-line aligned; the tail is a separate aligned atomic. No grow, no overwrite of
  an unacked slot, no blocking.

This is the project's strongest transferable idea, and it is the same contract our market-data
`try_send` + `GAP` path already implements on a Tokio `mpsc`: **fail closed, bound the queue, and
account for the drop** instead of silently growing or overwriting. The named-reader ack bitmask is a
different mechanism from our `(log_index, ordinal)` event cursors, but the discipline matches: a
consumer's position is explicit and a slot is not recycled underneath a lagging reader.

### 3. Replication is a log primitive, not consensus

`include/pitlane/net/replicate.hpp` was read directly. `ReplicaLog<T, Cap = 4096>` is a ring buffer
with 1-based indexes, `terms_`, `index_`, `value_`, `count_`, `commit_`, `applied_`, `match_[3]`,
`quorum_` and a two-state `ReplicaRole {Follower, Leader}`:

- `become_leader(term, quorum)` is called by the host; there is **no election algorithm** in this
  class. `leader_append` writes at the current term and advances `match_[0]`.
- `follower_append` checks `term >= term_`, `prev_index <= count_`, a matching `prev_term`, and
  truncates a divergent suffix before appending. It advances `commit_` from the leader's commit.
- `commit_quorum()` sorts the three match indexes and takes `m[q-1]` (q clamped to 1..3). `apply()`
  copies committed-but-unapplied entries into a caller buffer.
- The backing arrays are fixed at `Cap`; `leader_append` simply returns `false` when full. There is
  no disk persistence, no snapshot, no membership change, no PreVote and no leader transfer here.

That is a useful contrast to our OpenRaft choice. A quorum match-index rule plus term checks is a
building block; durability, elections, membership and recovery live outside this class (or not at
all). It reinforces the same lesson as Parts II–III: a "replicated log" label is not evidence of a
consensus implementation, and a fixed ring that fails a write at capacity is not a WAL.

### 4. Pre-trade gate: ordered, deterministic and fail-closed

`include/pitlane/order/precheck.hpp` was read directly. `PreTradeGuard::check()` runs a fixed first-failure-wins
pipeline and returns a typed `PreCode`:

```
input -> halt -> open/close blacklist -> notional cap by class ->
account flow (1s, 10s) -> market flow (1s) -> cancel ratio ->
cancelled-amount ratio -> self-trade -> Ok
```

Concrete details worth borrowing:

- **Zero means "rule off"**, so the same pipeline serves strict and permissive deployments.
- Every code is named by `pre_code_name()`, so a rejection reason is deterministic for a given
  state. No allocation and no locks; caller-owned fixed arrays only.
- A 128-slot active-order table provides self-trade cover. `on_submit` returns `false` when the
  table is full and the caller **must drop the order** because cover would be incomplete — fail
  closed rather than silently accept an unchecked order.
- Cancel-ratio and cancelled-**amount**-ratio gates use a warm-up minimum and a configurable window,
  which is a more complete version of the simple in-flight cap we use at `--max-inflight-writes`.

This is directly relevant to the P1 security/gateway workstream and to the now-stateless order desk:
rate limits, blacklists and self-trade screening are edge/policy concerns that do not require
moving user state into the engine. It is **not** a replacement for our `owner_id` + STP modes —
pitlane's self-trade check is a pre-trade screen against a local active table, while ours is
replicated matching semantics.

### 5. Console, RBAC and mode isolation

The console defines five roles — `viewer`, `trader`, `risk`, `ops`, `admin` — with permission bits
checked on every `/api/*` route except `health` and `login`. Accounts are PBKDF2-HMAC-SHA256
verifiers (10,000 rounds, per-user salt); login issues a 64-hex token with an 8-hour TTL in an
HttpOnly cookie and as a bearer token. A session picks **test** or **live** at login, every object
is filtered by mode, and a cross-mode request is refused with `409`. Gateway start/stop, control
actions (pause, halt, risk-block, reload, cancel_all, stop) and clearing operations are audited;
alarms escalate on stale/jump/link/gateway/risk events and can cancel all orders for a product.

Two things transfer cleanly to our TODO: **mode isolation at the session layer** and **one typed
permission bit per route**. Our manager has no auth today; the pitlane pattern is a useful concrete
shape for the P1 workstream, though its single `poll` thread, 8 KB request ceiling and plaintext
loopback binding are lab-scale, not a production gateway.

### 6. Measurement and engineering discipline

- `ctest` runs the core, shared-library, C-ABI and trading tests plus `perf`/`perf_shared`. The
  tick-to-trade test spans three processes on one desk: quote stamp to gateway stamp, report path
  excluded; cycles use a scale sampled before the measured section. It reports five batches of
  2,000 samples, p50/p99/p999, plus a separate "set aside" count for outliers several times above
  the body. Sample README result: p50 395 ns, p99 1,658 ns, p999 1,929 ns.
- The strict build enforces a 2 µs bound on p99 **and** p999 and expects a tuned host; CI configures
  `-DPITLANE_RELAX_LATENCY=ON`. This is the same "loose in CI, strict on a quiet host" split our
  `bench-regression` job and `bench-linux` manual workflow already use.
- The build uses `-O3 -fno-exceptions -fno-rtti -march=native`, LTO, `-Werror`, optional PGO
  (train on the paths you care about; `USE` treats untrained code as cold), explicit huge pages with
  ordinary-page fallback, `mlockall`, NUMA binding and a deliberate AVX-512 opt-out
  (`-mprefer-vector-width=256 -mno-avx512f`) because AVX-512 can drop core frequency.
- CI regenerates `doc/site` with `python3 doc/gen_site.py` and fails on a diff; `CONTRIBUTING.md`
  requires `README.md`/`README_CN.md` and `doc/API.md`/`doc/API_CN.md` to be updated in the same
  change. That is a concrete guard against the docs drift our own P2 docs-sync item keeps flagging.

### 7. Paper matching and failure taxonomy

Paper matching is intentionally narrow: day orders reaching their own limit fill the whole
remainder even beyond displayed quantity; passive-only expires as soon as it would trade through;
immediate takes only visible quantity; all-or-nothing expires entirely when visible quantity is
short; fills use the passive price; empty levels disappear at once; a self-trade stops the order.
Call auctions pick the max-matched-price with ties resolved by a reference. Two details echo bugs
already catalogued in Parts II–III: **prints on the same aggressive side must not fill resting
orders**, and **later level changes must not subtract the same print twice** — the same
double-decrement class as Part II's `wire-exchange` LOB and Part III's expired-maker volume bug.
Our `BTreeMap` book plus property/conformance model already guards both; keep those invariants.

The block tape is also relevant to storage: append-only 64 KiB blocks, a 64-byte header, and
64-byte commit records that publish immutable versions. Readers can query an older version, project
columns and binary-search inside a block. It uses `pread` rather than `mmap` so a truncated tail
cannot raise `SIGBUS`. Our WAL already uses `pread`, CRC and torn-tail truncation; the extra idea is
publishing an **immutable version pointer** rather than mutating a "current" record in place.

### 8. Findings and risks

- **No consensus durability.** The desk is shared memory and `ReplicaLog` is a capped in-memory
  ring with host-driven leadership. A crash loses everything unless the host persists it; our WAL,
  snapshots and Raft recovery remain a non-comparable strength.
- **One commit, one star, self-reported numbers.** No independent reproduction of the 395 ns / 2 µs
  figures was performed here. The three headers checked did match the README's described contracts.
- **Fixed capacities everywhere.** Orders 32,768; reports 32,768; quotes 8,192; cash 512;
  positions 4,096; paper matching 256 instruments / 1,024 working orders; signals 128
  subscriptions / 256 queue. This is a deliberate fail-closed design, not a general engine; our
  18-decimal `Fixed` and sparse `BTreeMap` are the right trade for an unbounded price domain.
- **The console is lab-scale.** Plaintext HTTP, loopback default, one `poll` thread, one request per
  connection. Do not treat its RBAC as a production gateway.
- **C++ intrinsics are not portable evidence.** SSE2/SSE4.2/AVX2 with scalar fallbacks, no
  AVX-512. Our `level_book`/`sorted_book` prototypes must still win on our own workloads.

### 9. Lessons for matchengine-raft

- Keep the **fail-closed bounded queue + drop accounting** discipline; our `try_send`/`GAP` path and
  the replicated event window already follow it. Named-reader acks are a different mechanism but
  reinforce explicit consumer cursors.
- Treat a quorum match-index log as a primitive, not as a reason to weaken Raft. Do not trade our
  election, membership, fsync and snapshot guarantees for a simpler replication shape.
- Add **typed permission bits and test/live mode isolation** to the P1 security workstream; make a
  cross-mode request fail with `409` rather than silently reading the other mode's data.
- Consider an **ordered pre-trade rule pipeline** with typed first-failure codes, zero-off limits,
  cancel-ratio gates and fail-closed cover at the edge/gateway — kept policy-side so the stateless
  order desk stays stateless. This is recorded in `TODO.md`.
- A **generated-docs diff check** would close the known README/API drift permanently; recorded in
  the P2 docs-sync item in `TODO.md`.
- Report latency like pitlane does where it matters: several batches, p50/p99/p999 plus outliers
  set aside, and strict bounds only on a quiet host. Our `PERFORMANCE.md` warm/cold and CI-vs-manual
  split already has the shape.

## Part VII - Binance Spot market-data API: kline updates and historical retrieval

This part is an API-contract review rather than an engine review, and it exists to scope a
historical data system for this project. Binance is the reference for two separate things: how an
open kline is published while it is still changing, and how the same kline is later retrieved as
final data. The two paths share no sequence number, and that single fact drives most of the design
below.

**Evidence.** Specification text was read from `binance/binance-spot-api-docs` at
`web-socket-streams.md` `a049a2b37d` (2026-11-11), `rest-api.md` `adfd443f85` (2026-11-11) and
`web-socket-api.md` `907657c4d0` (2026-11-11); archive facts from `binance/binance-public-data`
`d571690eb6` (2026-10-16). Statements marked *(doc)* are quoted from those files. Statements marked
*(measured)* were reproduced on 2026-11-22 by downloading and decompressing files from
`data.binance.vision`. The live endpoints `api.binance.com` and `stream.binance.com` were **not
reachable from the review network**, so frame cadence, `x` timing and `startTime` inclusivity were
not re-measured; they are doc-derived and are carried as verification items in section 6.6.

### 1. Live kline updates (WebSocket Streams)

| Property | Value |
|---|---|
| Stream name | `<symbol>@kline_<interval>` (lowercase symbol) |
| Combined form | `/stream?streams=<s1>/<s2>/...`, frames wrapped `{"stream":...,"data":...}` |
| Intervals *(doc)* | `1s`, `1m`, `3m`, `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `8h`, `12h`, `1d`, `3d`, `1w`, `1M` |
| Update speed *(doc)* | **1000 ms for `1s`, 2000 ms for every other interval** |
| Time zone *(doc)* | UTC by default; `<symbol>@kline_<interval>@+08:00` anchors windows in an offset |
| Time unit *(doc)* | milliseconds by default; `?timeUnit=MICROSECOND` switches all timestamps |
| Connection lifetime *(doc)* | 24 h, then a forced disconnect; a `serverShutdown` event precedes planned shutdowns |
| Liveness *(doc)* | server pings every 20 s; pong within 60 s or the connection drops |
| Limits *(doc)* | 1024 streams per connection, 5 inbound messages/s, 300 connections per 5 min per IP |
| All-market stream | **none for klines** — only `!miniTicker@arr` and rolling-window statistics |

Payload *(doc, field meanings confirmed against the archive's column order)*:

```javascript
{
  "e": "kline", "E": 1672515782136, "s": "BNBBTC",
  "k": {
    "t": 1672515780000,  // window open time
    "T": 1672515839999,  // window close time, = t + interval - 1
    "s": "BNBBTC", "i": "1m",
    "f": 100, "L": 200,  // first / last trade id in the window
    "o": "0.0010", "c": "0.0020", "h": "0.0025", "l": "0.0015",
    "v": "1000", "n": 100,          // base volume, trade count
    "x": false,                     // is this kline closed?
    "q": "1.0000",                  // quote volume
    "V": "500", "Q": "0.500",       // taker-buy base / quote volume
    "B": "123456"                   // documented as "Ignore"
  }
}
```

The three properties that matter for a consumer:

1. **Each frame is a cumulative snapshot of one window, not a delta.** `o`/`h`/`l`/`c`/`v`/`q` are
   totals for the window so far; a client overwrites the bar rather than applying an increment.
2. **The same open window is re-sent on a timer.** The doc says the stream "push updates to the
   current klines/candlestick every second" with an update speed of 1-2 s, i.e. the cadence is time
   driven, not trade driven. A quiet market still produces frames whose values do not change.
3. **`x` is the only finality signal.** The last frame of a window carries `"x": true`; every
   earlier frame of that window carries `false`. Nothing orders frames beyond `t`, and unlike the
   depth streams there is no `U`/`u` range and no `lastUpdateId`.

That last point is the operational trap. Binance gives a kline consumer **no way to detect a dropped
frame** from the stream alone: two frames whose `t` differ by one interval look perfectly normal if
the one in between never arrived. A missed `"x": true` frame is invisible until the chart's last
candle silently stops updating. Consumers therefore reconcile against REST on `t`, or track `f`/`L`
(the first/last trade id) and detect a discontinuity in the trade sequence instead of the bar
sequence.

### 2. On-demand history (REST and WebSocket API)

| Endpoint | Weight *(doc)* | `limit` *(doc)* | Parameters *(doc)* | Data source |
|---|---|---|---|---|
| `GET /api/v3/klines` | 2 | default 500, max 1000 | `symbol`, `interval`, `startTime`, `endTime`, `timeZone` | Database |
| `GET /api/v3/uiKlines` | 2 | default 500, max 1000 | same; "modified kline data, optimized for presentation" | Database |
| WS API `klines` / `uiKlines` | 2 | default 500, max 1000 | same, as a request/response frame on the socket | Database |
| `GET /api/v3/aggTrades` | 4 | default 500, max 1000 | `symbol`, `fromId`, `startTime`, `endTime` | Database |
| `GET /api/v3/historicalTrades` | 25 | default 500, max 1000 | `symbol`, `fromId` | Database |

The kline response is a twelve-element array per bar, with prices and volumes as strings:
`[openTime, open, high, low, close, volume, closeTime, quoteVolume, trades, takerBuyBase,
takerBuyQuote, ignore]`. The REST doc states "Klines are uniquely identified by their open time";
the WebSocket API page says "open & close time". In practice `closeTime = openTime + interval - 1`
exactly *(measured in every sample file)*, so the open time is the key and the close time is derived.

Other properties worth recording:

- **`timeZone` shifts interval anchoring only.** Any offset in `[-12:00, +14:00]` is accepted;
  `startTime`/`endTime` remain UTC. This is how a `1d` candle can open at a local midnight.
- **Pagination is by `limit`, not by cursor.** 1000 bars per call maximum, so one year of `1s` is
  31,536 calls and one year of `1m` is 526. The next page's `startTime` should be the last returned
  `openTime + interval` so the boundary bar is not fetched twice.
- **Rate limits are per IP, not per API key** *(doc)*. The published `exchangeInfo` example shows
  `REQUEST_WEIGHT` 6000 per minute; every response carries
  `X-MBX-USED-WEIGHT-(intervalNum)(intervalLetter)`. Exceeding it returns `429` with `Retry-After`;
  repeatedly ignoring `429` escalates to `418` and an IP ban that scales from 2 minutes to 3 days.
- **`Data Source: Database`** and, from the API notes, "The API system is asynchronous, so some
  delay in the response is normal and expected." A kline read is therefore not a linearizable view
  of the matching engine; it is a database view that can trail the live stream by an unspecified
  amount. That is exactly the window in which a backfill can disagree with a live bar.
- **`aggTrades` and `historicalTrades` are the trade-level complement.** `aggTrades` aggregates
  fills by taker order and price and accepts `fromId`, `startTime` or `endTime`; `historicalTrades`
  is the older, id-addressed form at weight 25. Together with the kline `f`/`L` fields they allow a
  bar to be rebuilt from trades and checked against the served bar.

### 3. Bulk archive (`data.binance.vision`)

The archive is the practical way to seed history; the REST ladder is only for the tail.

| Property | Value |
|---|---|
| Layout | `data/spot/{daily,monthly}/klines/<SYMBOL>/<interval>/<SYMBOL>-<interval>-<YYYY-MM[-DD]>.zip` |
| Monthly interval name *(doc)* | `1mo`, not `1M`, to survive case-insensitive file systems |
| Publication *(doc)* | daily data "the next day"; monthly data "on the first monday of the month" |
| Contents | one CSV per zip, same twelve columns as REST, **no header row in any sample** |
| Integrity *(doc, measured)* | sibling `<file>.zip.CHECKSUM`, `sha256sum` format; verified `OK` on a 1s daily file |
| Timestamps *(doc, measured)* | **microseconds from 2025-01-01**, milliseconds before that |
| Revisions *(doc)* | archive files can be replaced after publication; a changelog lists past corrections |
| Enumeration *(measured)* | `https://s3.ap-northeast-1.amazonaws.com/data.binance.vision?prefix=<path>` returns a `ListBucketResult` XML with `<Key>` entries and `<IsTruncated>` |

Measured samples (BTCUSDT), 2026-11-22:

| File | Rows | CSV bytes | Zip bytes | Notes |
|---|---|---|---|---|
| `1s` daily, `2025-01-02` | 86,400 | 12,697,051 | 2,667,429 | microsecond timestamps |
| `1s` daily, `2024-12-31` | 86,400 | — | 2,637,831 | millisecond timestamps |
| `1s` daily, `2020-01-01` | 86,400 | 11,619,794 | 2,091,917 | zip mtime 2022-10-18, i.e. backfilled |
| `1s` daily, `2017-08-17` | 71,972 | — | 480,298 | starts at the listing second (04:00:28 UTC) |
| `1m` monthly, `2024-01` | 44,640 | 6,699,071 | 2,169,570 | 31 days × 1440 |
| `1m` monthly, `2017-08` | 21,360 | — | 595,271 | starts at listing, not month start |

Four findings from those samples change how a loader must be written:

1. **Bars are dense.** A full day has exactly 86,400 `1s` rows and a full month exactly
   `days × 1440` `1m` rows, including intervals with no trades. On the sampled `1s` day, 4,259 rows
   (4.9%) had `volume = 0`, `trades = 0` and `open = high = low = close` equal to the **previous**
   row's close. Empty intervals are carried forward, not omitted. The 2020 file's zip entry is
   dated 2022-10-18, i.e. the dense `1s` series was generated after the fact rather than streamed
   as it happened.
2. **The twelfth column is not always `0`.** The `2017-08` `1m` monthly file ends its rows with
   values such as `7960.54017996`, while modern files end with `0`. The column is documented
   "Ignore"; a loader must discard it rather than assume a constant.
3. **The unit changed mid-history.** Rows before 2025 are milliseconds
   (`1735603200000`), rows from 2025-01-01 are microseconds (`1735776000000000`). A single file can
   be read correctly only after deciding the unit from the digit count, not from the file date.
4. **`1s` history is complete, not recent.** The archive serves `1s` daily files from
   `2017-08-17` (spot's first day for BTCUSDT) onward, so the whole spot history is available at
   one-second granularity regardless of when the live `1s` interval was documented.

### 4. Bar semantics, condensed

1. **Identity is the open time.** `(symbol, interval, openTime)` is the key; `closeTime` is
   `openTime + interval - 1`; no other identifier exists.
2. **A bar is a cumulative value, not an event.** There is no depth-style delta chain, no
   sequence number and no gap marker.
3. **Finality is `x`** (stream) or simply "the window is in the past" (REST/archive). A closed bar
   is stable in practice, subject to the documented archive revisions.
4. **Every bar is derivable from trades**, and the payload carries the linkage: `f`/`L` bound the
   trade ids, `n` counts them, `v`/`q` are total volume, `V`/`Q` split out taker buys.
5. **Empty intervals are still bars.** The public contract is a dense series; a chart consumer that
   receives sparse bars will draw gaps where the reference implementation draws flat candles.

### 5. Where matchengine-raft differs today

| Aspect | Binance Spot | matchengine-raft today | Consequence for a historical system |
|---|---|---|---|
| Open-bar emission | timer, 1 s (`1s`) / 2 s (others) *(doc)* | only when a fill lands, gated to ≤1 Hz, first fill of a window suppressed | a quiet market publishes no bars at all (logged as TODO P2 #11) |
| Empty intervals | dense flat bar, `v = 0` | absent | charts must synthesise flat candles or show gaps |
| Open-bar refresh for a late joiner | next timer tick | replay on subscribe only | a subscriber can hold a stale open bar until the next fill |
| Identity | `(symbol, interval, openTime)` | `(symbol, interval, window_start_ms)` | same shape, both in milliseconds |
| Order-flow fields | `n`, `v`, `q`, `V`, `Q`, `f`, `L` | `trade_count`, `volume` | no taker-buy split, no trade-id linkage |
| Finality | `x` boolean | `BAR_CLOSE` flag | equivalent |
| Ordering aid | none | `bar_seq` per `(symbol, interval)` | ours is stronger; keep it as an extension, do not rename it into the public shape |
| Long-term history | REST + archive + 1000-bar replay | 1000-bar in-memory replay per service | no durable store, no backfill, no rollup |
| Time unit | ms, optional µs *(doc)* | ms | archive import must normalise |

### 6. Design implications

1. **One key, one row, one writer.** Store `(symbol_id, interval, open_time)` as the primary key
   with `close_time` derived. Reject a write whose `close_time` disagrees, and never accept a write
   that moves `open_time` backwards into a sealed segment. Everything else in this section follows
   from that constraint.
2. **Seal per day, mirror the archive's granularity.** Segments of
   `<symbol>/<interval>/<YYYY>/<YYYY-MM-DD>` (sealed, immutable, checksummed, with a catalog row of
   `min_t`, `max_t`, `rows`, `sha256`) make the archive import, the REST backfill and the live tap
   converge on the same unit of work. Columnar encoding is the obvious fit: 86,400 rows/day/symbol
   at `1s` is 12.7 MB as CSV and 2.7 MB zipped, so a year is ~4.6 GB raw per symbol before
   compression; delta/varint columns should land far below the zip.
3. **Three ingest paths, one merge rule.** Live tap (authoritative for the open bar), REST
   gap-fill (authoritative for closed bars), archive import (bulk seed, idempotent by content
   hash). Merge rule: a closed bar is final; an open bar is replaced by the newest source; a closed
   bar that disagrees with a stored closed bar is a **revision**, recorded with both values and the
   source, never a silent overwrite. Then assert the invariants
   `close_time = open_time + interval - 1`, strictly increasing `open_time` within a segment, and
   `low ≤ open, close ≤ high`.
4. **Decide sparse-vs-dense once, at the storage boundary.** Store what the source actually gave
   (sparse for our engine's per-fill bars, dense for an archive import) and densify at the serving
   layer with the carry-forward rule, so "no trade in this interval" stays a fact rather than
   becoming seven identical zero-volume rows. Serving must be dense to be Binance-compatible.
5. **Backfill is an archive problem, not a REST problem.** One year of `1s` is 31,536,000 bars per
   symbol: 365 archive files (~1 GB zipped) or 31,536 REST calls at weight 2 (63,072 weight, about
   ten minutes at the published 6000/min ceiling before retries and other traffic). Use the archive
   for anything older than a day, then reconcile the tail with REST. `1m` from 2017-08 is only
   ~5.3 M bars, i.e. ~5,300 REST calls — but the archive is still cheaper and far less
   rate-limit-exposed.
6. **Verification items before the design is frozen.** Because the live hosts were unreachable
   here, confirm on a network that can reach them: (a) that the open bar really is re-pushed on the
   1-2 s cadence with no trades, and what `x: true` looks like for a window with zero trades;
   (b) whether `startTime`/`endTime` are inclusive of bar open times at both edges; (c) whether the
   REST response for a closed bar can differ from the final `x: true` frame (and how often);
   (d) whether `timeUnit=MICROSECOND` also changes `t`/`T` inside `k.` or only `E`; (e) the
   `1M`/`1mo` naming behaviour for the WebSocket stream versus the archive; (f) `Retry-After` and
   the `X-MBX-USED-WEIGHT-*` header on `429`/`418` under controlled load.
7. **Read surface to target.** A `GET /klines` with Binance's exact parameter names, defaults
   (500) and cap (1000), returning the twelve-element array, is cheap to implement over a
   columnar store and makes every existing Binance client work unchanged. Pair it with the
   subscribe-time replay the market-data service already does, and keep a cursor/`since` extension
   (`bar_seq`) for internal consumers that need gap detection.
8. **Rollups.** Deriving `1m` from `1s` is only exact if the densification rule matches (an empty
   `1s` interval must not fabricate a trade). Deriving from trades is exact by construction and is
   the safer canonical form; deriving from `1s` is a cost optimisation to be adopted only after a
   differential test over a month of data.
9. **Failure cases to test explicitly.** Duplicate ingest, out-of-order bars, a late `x: true` after
   a newer window, an archive revision replacing a month, a µs/ms mix-up, a request whose `limit`
   would cross a month boundary, an empty interval at the edge of a trading halt, and a symbol whose
   listing day is mid-month (the 2017-08 samples in section 3 start at the listing second, not at
   the calendar boundary).

### 7. Recommended shape

```text
engine apply ──► market-data tap ──► per-symbol ring + open-bar cadence ──► WS /kline
                        │
                        └──────────► historical writer ──► TDengine
                                                          bars_1s (canonical)
                                                          bars_1m (materialized rollup)
                                                                  ▲
data.binance.vision importer ─────────────────────────────────────┤
REST /api/v3/klines gap-fill ─────────────────────────────────────┘
                                                                  │
                                                    GET /klines + WS replay
```

The live path stays on the existing asynchronous tap; the writer consumes the same frames and never
touches the matching path. The archive importer and the REST gap-filler write through the same merge
rule (an idempotent upsert keyed by `(symbol, open_time)`), so a month can be seeded twice, corrected,
or replayed without a special case.

The full system design — data model, TDengine schema and the exact-`DECIMAL(38,18)` contract, merge
and revision rules, API shapes, capacity and rollout — is
[`HISTORICAL_BARS.md`](./HISTORICAL_BARS.md).

## Part VIII - Comparison with matchengine-raft

Binance is deliberately absent from this table: it is a market-data API contract, not an engine
implementation, and its kline-specific comparison against this project is in Part VII §5.

| Dimension | AsthaMishra | Wire v9 | llc-993/matching-core | Apex | pitlane | matchengine-raft |
|---|---|---|---|---|---|---|
| Topology | Single node | Single node, benchmark kernels | Single-node library + Disruptor pipeline | Single-node Spring Boot service | Shared-memory multi-process desk; optional capped UDP replication | Raft-replicated cluster |
| Durability | None | No WAL on matching path | WAL/snapshot attempt without fsync/atomicity | None (JPA/H2 dependencies unused) | Shared region + optional host WAL/tape; `ReplicaLog` is in-memory | Segmented WAL + atomic snapshots + recovery |
| Symbol model | Multi-symbol sharded; single-book OUCH path | One flat book, 65k slots | Symbol-sharded routers; Direct book per symbol | Multi-symbol map, one fair lock per book | 256 paper instruments; 1024-name directory | One symbol per process/cluster |
| Book | Flat array + bitmap + `Vec` level | Flat array + linked-list levels | BTreeMap/Slab/AHashMap (Direct); BTreeMap/SmallVec (Advanced); SOA prototype unused | `TreeMap` + `ArrayDeque` + fair lock | Fixed tables on the shared desk; paper venue | `BTreeMap` price/sequence |
| Price/quantity | Cents, `u32` | `u32` price index, `u32` shares | `i64` price/size | `BigDecimal` price, `long` quantity | Integer price, microsecond timestamps | 18-decimal `Fixed` `i128` |
| Order types | Limit/Market/IOC/FOK | Single-fill primitive | Advertised many; exchange Direct only GTC/IOC/FokBudget | Limit/Market only | Day/passive/immediate/AON + call auction; CTP offsets/styles | GTC/IOC/FOK/GTD/Day/market/post-only/iceberg/stop/market-to-limit/trailing/peg + STP |
| User/STP | Cancel-resting STP | None in matcher | No STP; Advanced cancel ignores uid | User id on orders/trades; no STP or balances | Pre-trade self-trade screen + paper self-trade stop; RBAC roles | `owner_id` + STP modes; fees/assets modeled |
| Events | In-process stream; global trade id | One `MarketEvent` per fill | Trade/Reject/Reduce only | REST response + STOMP trade/depth; process-local ids | Fail-closed `FanRing` reports/alarms/audit with reader acks | Replicated event window; deterministic ids |
| Protocol | OUCH/ITCH + REST | No integrated order entry | Library only | HTTP JSON + STOMP/WebSocket | Shared desk + C ABI + Python bridge + web console | HTTP JSON + gRPC Raft |
| Tests | Unit/proptest/fuzz (stale target) | Unit per kernel; no v9 integration | Tests present; no pipeline/consensus integration observed | 4 unit + 2 integration; benchmark not asserted | ctest across areas + perf + C ABI; 5-batch tick-to-trade assertion | Unit/property/conformance/integration/restart/snapshot/fees |
| Benchmarks | Criterion/ITCH/tail | Pinned-core microbenchmarks | AdvancedOrderBook TPS; estimated memory; flawed Criterion cases | 100k shallow sequential limits; docs claim 1.24M QPS | 3-process tick-to-trade p50/p99/p999; strict CI-relaxed split | e2e/batch/feed/storage/P0 benchmarks |
| FlashOne survey (Part IV) | 5.60 M/s, with fix; bounded-domain flat array | Not listed in the roster | 7.14 M/s, as shipped; new/cancel/modify ABI only | Not listed in the roster | Not listed in the roster | 7.20 M/s, fastest B-tree; "with fix" at the old pin, fixed at `HEAD` |

Our engine is stronger on semantics, durability, exactness and recovery. Astha is stronger on
single-node hot-path layout, binary gateway work and measurement scope. Wire is stronger on
low-level allocator/MPMC experiments and honest scope disclosure, but its v9 matching kernel is
narrower than its name suggests. llc-993 is the broadest feature skeleton, but its advanced order
types are not wired into the exchange, its WAL/snapshot is not crash-safe, and its benchmark numbers
measure a separate `AdvancedOrderBook`, not the product pipeline. Apex is the clearest teaching
implementation of the five and the easiest to read, but it is a Spring demo with no durability,
risk, STP or production edge, and its docs claim concurrency and benchmark behaviour the code does
not implement. Pitlane is the strongest of the six on process-to-process transport discipline and
pre-trade policy structure, but its "replication" is a capped in-memory log without elections or
durability, and its console is a lab-scale plaintext service; it is a source of patterns, not of a
durability or consensus story.

On the FlashOne benchmark's matching-only ABI, our `BTreeMap` core is the fastest B-tree entry and
within about 8% of the fastest Rust engine. That is a useful external validation of the price ladder
and the basic FIFO loop; it is not a validation of the replicated product path. The benchmark's
`modify` is cancel + reinsert and its identity is a client order id, so a direct mapping to our
`Amend` would be wrong. Its "conforming" verdict is scoped to limit new/cancel/modify and IOC; it
does not exercise FOK, stops, icebergs, fees, persistence or consensus.

## Part IX - Consolidated takeaways

### Adopt

1. **Measurement-scope discipline.** Done: `docs/PERFORMANCE.md` has the benchmark-scope table,
   closed-loop/timer notes and warm/cold p50/p99 reporting.
2. **A pure order-book replay benchmark.** Done: `book_replay`, `replay_mix` and `level_book` keep
   Raft/HTTP out of the book-only numbers.
3. **Fuzzing.** Done: `match_engine/fuzz/` builds targets for the order book, WAL, snapshot and wire decoders;
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
8. **Open-loop latency (benchmark-derived).** Done: `e2e --rate` adds the paced run and reports
   scheduled-send, dispatch-lag and service percentiles; `docs/PERFORMANCE.md` records the
   coordinated-omission caveat and the 80k/s generator-bound tail.
9. **Preallocation, cache layout and allocation accounting.** `apply_path --check` enforces the
   allocation ceilings and `level_book` measured the per-level prototype. Keep structural changes
   conditional on a book-only A/B.
10. **A reusable MPMC reference.** If we ever need a custom bounded queue, the Vyukov pattern is a
    useful reference; Tokio `mpsc` remains the default.
11. **Fail-closed bounded queues with explicit drop accounting (pitlane-derived).** Done: the
    market-data tap uses `try_send` + `GAP` and the event window is bounded with `(log_index,
    ordinal)` cursors; never grow or silently overwrite a lagging consumer's slot.
12. **Strict-vs-relaxed latency bounds by host (pitlane-derived).** Done: `bench-regression` keeps
    loose CI bounds while `bench-linux` is the manual pinned-core strict pass. Keep the split and
    report outliers separately rather than hiding them in p99.

### Adapt / evaluate later

- A treap, sorted-vector or heap-hybrid price ladder, only after a book-only A/B on our workloads.
  Done for the sorted vector: `match_engine/benches/sorted_book.rs` shows level-grouping wins but loses on
  price-move amend and BBO advancement, so it is not adopted. A tree/heap hybrid remains a
  book-only option only if a sparse-level fast path is measured; the external `lcv3` treap lead is
  not sufficient evidence on its own.
- Re-pin the FlashOne benchmark to current `HEAD` and publish the adapter/reproduction if the C ABI
  can be maintained as a separate artifact. Done locally at `ee07ba0`; no external adapter is
  published. Held-out seeds and `validate_seeds.py` remain optional external validation, not a CI
  dependency.
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

### pitlane-specific takeaways

- **Do not mistake a quorum log for consensus.** `ReplicaLog` has match indexes and commit/advance
  but no election, persistence or membership; our Raft/WAL/snapshot story stays non-negotiable.
- **Keep bounded queues fail-closed.** A full `FanRing` refuses the write and never overwrites an
  unacked slot; our `try_send`/`GAP` and event cursors already do the equivalent.
- **Put policy before the book, not in it.** The ordered `PreTradeGuard` pipeline with typed
  first-failure codes, zero-off limits and fail-closed cover is a good shape for the gateway/edge.
  Keep it out of the matching core and out of the stateless order desk's durable state.
- **Isolate test and live at the session layer**, return `409` on cross-mode requests, and give
  every API route one named permission bit; all three are recorded in the P1 security item.
- **Guard docs drift mechanically.** A generated-site diff check plus paired EN/CN updates stopped
  that class of drift upstream; our known README/API drift needs the same discipline.
- **Measure latency with batches and set-aside outliers.** Strict bounds belong on a quiet host,
  loose bounds in CI; do not let a single outlier silently inflate a headline percentile.

### Apex-specific takeaways

- **Separate the demo from the engine story.** A small Spring service is a good way to demonstrate
  order flow, but it is not evidence of exchange latency. Keep the demo tier, API adapter and
  matching core clearly labelled in docs.
- **Do not market a book-only microbenchmark as engine throughput.** The Apex benchmark times a
  shallow, alternating limit-order loop with order construction outside the timer; it does not
  include HTTP, STOMP, cancels, deep queues or GC. Our `book_replay`/`e2e` split is the right shape.
- **Bound order and trade state.** `orderIndex` and `tradeHistory` grow forever. Real engines need
  removal on fill/cancel and a retention policy for trade/event history.
- **Keep the feed off the matching lock.** Full-depth broadcast after every mutation amplifies the
  request path; a shadow book and asynchronous feed task are required for tail latency.
- **Make planned work explicit.** The Apex `docs/` describe Loom, Disruptor, FlatBuffers and
  lock-free maps that are not in `main`. Mark design notes as planned and link them to issues.

### Binance Spot API takeaways

- **A bar is a value, not a delta.** Klines carry no sequence number and no gap marker, so a dropped
  frame is invisible; consumers reconcile on `t` or on the trade ids (`f`/`L`). Do not invent a
  cursor into the Binance shape — expose ours as an extension instead (Part VII §1, §4).
- **Publish on a cadence, not only on activity.** The reference feed re-pushes the open bar every
  1-2 s regardless of trades. Our feed emits only on fills and suppresses each window's first emit,
  which is why a quiet market produces no candles (TODO P2 #11).
- **Dense is the public contract.** Empty intervals are flat bars carrying the previous close, not
  omitted bars; a sparse series draws gaps where the reference draws candles. Store what the source
  gave, densify when serving.
- **Two sources, one key.** Live and historical paths never share a sequence, so
  `(symbol, interval, openTime)` is the only join. Closed bars win over open bars; a disagreeing
  revision is recorded, never silently overwritten.
- **Archive first, REST for the tail.** One year of `1s` per symbol is 365 files (~1 GB zipped) or
  31,536 weight-2 requests; the archive is enumerable, checksummed and idempotent, and the REST
  ladder is rate-limited per IP (429 → 418 bans of 2 minutes to 3 days).
- **Normalise the file quirks at ingest.** Milliseconds before 2025 and microseconds after, no
  header row, `1mo` instead of `1M`, and a twelfth column documented "Ignore" that is not always
  zero. The listing-month files start at the symbol's listing second, not at the calendar boundary.
- **Roll up from trades, verify against `1s`.** Fabricating bars from empty intervals is a
  correctness trap; a differential test over a month is the adoption gate for any rollup.

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
- `BigDecimal` price keys, a fair `ReentrantLock` per book and `ArrayDeque.remove` on the matching
  and cancel path; they are readable and slow.
- Unbounded order/trade retention, plus full-depth broadcasts on the request thread.
- Benchmarks that run as ordinary tests without assertions, or an empty-book microbenchmark
  presented as exchange throughput.
- Docs that describe Loom, Disruptor, FlatBuffers, lock-free maps or ZGC without the matching code,
  dependency or JVM flags.
- Copying a project's code or claims without a license/buildable test evidence. AsthaMishra and
  llc-993 have no license file; Wire's fuzz/README claims need verification.
- Feature matrices that are only demonstrated through a separate book type, or benchmark numbers
  from a different code path than the exchange pipeline.
- Calling a fixed-capacity quorum match-index log "consensus" (pitlane's `ReplicaLog`): no election,
  no persistence, no membership; our Raft/WAL guarantees are not negotiable for a simpler shape.
- Trusting a single-commit, one-star repository's latency claims without reproduction, even when
  the three headers checked do match the prose.

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

**P2 - policy and docs (pitlane-derived, tracked in `TODO.md`)**

5. Add typed permission bits and test/live mode isolation with `409` cross-mode refusal to the P1
   security workstream; evaluate an ordered first-failure pre-trade gate at the edge/gateway, kept
   out of the stateless order desk and the matching core.
6. Add a generated-docs diff check to close the known README/API drift permanently.

**P3 - optional platform work**

7. Real-data replay, custom MPMC/SIMD/io_uring, binary order entry and feature-gated histograms,
   only after a measured bottleneck and a deployment decision.

**P2 - historical market data (Part VII)**

8. Before building, close the six live-API verification items in Part VII §6.6 and decide the
   sparse-store/dense-serve boundary. Then implement the store (TDengine `bars_1s`/`bars_1m` with
   exact `DECIMAL(38,18)` columns and read-time `INTERVAL` rollups), the three ingest paths under
   one merge rule, and a Binance-shaped `GET /klines` with subscribe-time replay. Seed from
   `data.binance.vision`; use REST only for the tail.

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

### ronitgupta138/Apex-Matching-Engine

- <https://github.com/ronitgupta138/Apex-Matching-Engine>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/README.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/engine/OrderBook.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/service/MatchingEngineService.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/model/Order.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/dto/OrderRequest.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/service/MarketBroadcaster.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/config/WebSocketConfig.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/java/com/tradematching/simulator/MarketSimulationBot.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/main/resources/application.yml>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/test/java/com/tradematching/engine/OrderBookBenchmark.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/src/test/java/com/tradematching/engine/OrderBookMatchingTest.java>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/benchmarks/Million_Order_Per_Second_Benchmark.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/benchmarks/Sub_Millisecond_Latency_Profile.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/benchmarks/Sub_Microsecond_Matching_Latency.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/benchmarks/Orders_Per_Second_Throughput.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/benchmarks/Worst_Case_Jitter_Distribution.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/orderbook/Order_Cancellation_Deterministic_Semantics.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/L2_Market_Depth_Loom_Concurrency.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/blob/main/docs/STOMP_WebSocket_Order_Stream.md>
- <https://github.com/ronitgupta138/Apex-Matching-Engine/issues/1> (Disruptor evaluation)
- <https://github.com/ronitgupta138/Apex-Matching-Engine/issues/2> (primitive order index)
- <https://github.com/ronitgupta138/Apex-Matching-Engine/issues/3> (binary WebSocket feed)
- <https://github.com/ronitgupta138/Apex-Matching-Engine/issues/4> (multi-threaded stress test)
- <https://github.com/ronitgupta138/Apex-Matching-Engine/issues/5> (benchmark CI regression)

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

### zzxscodes/pitlane

- <https://github.com/zzxscodes/pitlane>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/README.md>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/CHANGELOG.md>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/CONTRIBUTING.md>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/.github/workflows/ci.yml>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/CMakeLists.txt>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/include/pitlane/desk/ring.hpp>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/include/pitlane/net/replicate.hpp>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/include/pitlane/order/precheck.hpp>
- <https://github.com/zzxscodes/pitlane/blob/ece9c8acc52ad4cb9ad54e1877b11ec548418190/doc/web-api.md>

### Binance Spot market-data API

- <https://github.com/binance/binance-spot-api-docs>
- <https://github.com/binance/binance-spot-api-docs/blob/a049a2b37d/web-socket-streams.md>
- <https://github.com/binance/binance-spot-api-docs/blob/adfd443f85/rest-api.md>
- <https://github.com/binance/binance-spot-api-docs/blob/907657c4d0/web-socket-api.md>
- <https://github.com/binance/binance-public-data/tree/d571690eb6>
- <https://data.binance.vision/>
- <https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01.zip>
- <https://data.binance.vision/data/spot/daily/klines/BTCUSDT/1s/BTCUSDT-1s-2025-01-02.zip>
- <https://data.binance.vision/data/spot/daily/klines/BTCUSDT/1s/BTCUSDT-1s-2020-01-01.zip>
- <https://s3.ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/spot/monthly/klines/BTCUSDT/1m/>
