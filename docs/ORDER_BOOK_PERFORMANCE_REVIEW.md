# Order-book performance review against the third-party investigation

- **Date:** 2026-11-26
- **Scope:** `match_engine/src/order_book/`, the apply/snapshot paths that drive it, and the
  benchmark harnesses in `match_engine/benches/`.
- **Sources:** [`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) (Parts I, II, IV, V,
  VI, VII and the consolidated takeaways), [`PERFORMANCE.md`](./PERFORMANCE.md) (measured baselines,
  external-optimization verdicts and benchmark scope), `TODO.md` (shipped/deferred items).
- **Method:** cross-reference each external finding and "Adopt/Avoid" verdict with current code;
  audit the matching loop, peg repricing, depth reads, expiry/stop indexes and apply-path
  allocations; inventory which hypotheses the existing benches can actually measure; A/B the
  uncommitted stop/GTD index refactor against `HEAD` with `replay_mix` on this host.
- **Code state:** `HEAD` is `4f347a9`; the working tree additionally holds the uncommitted
  stop/GTD index refactor (`match_engine/src/order_book/indexes.rs` plus edits to `order_book/mod.rs`
  and its call sites). Line numbers below refer to that working tree. This review changed no
  implementation code — only this document and two cross-links into it.
- **Status:** F2, F1a/F1c/F1b, F3 and the F4a/F5 micro cleanups are implemented in the working
  tree and measured (§2a). F4b and F6–F8 remain open; the recommended order is in §5.

## Executive summary

The external investigations validate our price ladder and measurement discipline, not a need for a
different data structure. Their most transferable warning — "O(n)/O(n²) cancel or scan" and
"benchmark the product path" — exposed one gap that has since been closed (stop/expiry scans) and
one that is still open: **`reprice_pegged()` walks the entire book on every mutating request**, and
**no benchmark anywhere sets a pegged order**. A second, much cheaper finding is that
`SymbolConfig::clone()` accounts for two of the three allocations in the measured `apply_path`
rest row.

The just-completed stop/GTD index refactor is a large, measured win, not a speculative one:

| `replay_mix` (100k ops, 4,858 parked stops, 2,546 triggers) | warm p50 | throughput |
|---|---:|---:|
| `HEAD` before refactor (two runs) | 163.9 / 170.3 ms | ~612–616k ops/s |
| Working tree after refactor (two runs) | 33.4 / 33.6 ms | **~3.08M ops/s** |

Throughput is the bench's warm-best figure; p50 is reported separately. About **5x** on the one
book-only fixture that covers stops. `docs/PERFORMANCE.md` records an older
pre-refactor baseline of ~107 ms (968,709 ops/s) from a different host/session; use the paired
A/B above for the refactor's effect, not the cross-session comparison.

## 1. What the investigation already settles (no action)

These items are recorded as done or rejected in `THIRD_PARTY_INVESTIGATION.md` and
`docs/PERFORMANCE.md` §"External optimization notes". Re-litigating them without a new A/B would
waste effort:

| External idea | Investigation verdict | Our state |
|---|---|---|
| Flat tick array + bitmap | 7.8–318x in a bounded 100k-slot, cent-tick book; 14 flat-array engines peak at 5.60 M/s and lose on wide swings | Wrong contract for 18-decimal sparse `Fixed`; `level_book` prototype loses on sparse amend (0.88x) and depth (0.94x) |
| Cached best bid/ask | 354 ns → 0.88 ns in their book; competitors in Parts II/IV ship stale/phantom BBO | `best_bid`/`best_ask` read the tree edges; prototype's 6.09x query win is explicitly "skip the production change" |
| Arena (`bumpalo`) | 79–88% **worse** single-operation latency; they removed it again | Inline `SmallVec` buffers adopted; `apply_path --check` enforces 3.17/3.00/4.00 allocs/entry |
| Sorted-vector ladder | External 7.86 M/s; our `sorted_book` prototype wins match/cancel but loses sparse price-move amend (0.14x) and BBO advance (0.34x) | Not adopted; a treap/heap hybrid would need its own verified prototype |
| Treap lead (`lcv3`, 9.25 M/s) | One engine, external hardware/workload | Not evidence for our workloads; no treap bench exists here |
| SIMD cancel | Wire's AVX2 code computes a mask but never updates an order | Excluded categorically |
| Bounded MPMC on Tokio workers | `push_spin`/`pop_spin` block a worker and can deadlock a current-thread runtime | Tokio `mpsc` + `try_send`/`GAP` is the production path |
| `Vec` order index keyed by sequential ids | 1.7–1.8x after dropping SipHash | Already better: `(side, price, sequence)` is the tree key; no hash index |
| `active` flag + `head_idx` | Their follow-up win | Partial makers are updated in place and only fully consumed makers are removed |
| Response slot pool | Removes one allocation; caps in-flight requests | `/write` is fsync-bound (~4 ms); admission already caps 1,024 in flight |
| N-thread sharding | Their 132M/s is multi-thread (~33M/s per thread) | Serial apply is required for determinism; parallelism is across processes |

Methodology already adopted: warm/cold separation and setup outside the timer, worst-case
occupancy matrix (`book_replay -- --matrix`), median-of-10 with stddev/CV, random-point state audit,
conformance battery, open-loop `e2e --rate` with the coordinated-omission caveat, and
`apply_path --check` allocation gates.

## 2. Findings

### F1 — `reprice_pegged()` is a full-book scan per mutating request (highest remaining cost)

**Code.** `match_engine/src/order_book/mod.rs:1387-1409`:

```rust
let bid_keys: Vec<BidKey> = self.bids.iter().filter(|(_, o)| o.is_pegged()).map(...).collect();
let ask_keys: Vec<AskKey> = self.asks.iter().filter(|(_, o)| o.is_pegged()).map(...).collect();
for key in bid_keys { self.reprice_one_peg(...); }
for key in ask_keys { self.reprice_one_peg(...); }
```

`best_non_pegged_bid/ask` at `mod.rs:1335-1342` use `.values().find(|o| !o.is_pegged())`, so each
peg can walk past every better-priced peg. `peg_target` at `mod.rs:1351-1352` computes **both**
non-pegged BBOs even for `PegReference::LastTrade`, which uses neither.

**Call frequency.** `state_machine.rs:499-500` (`append_peg_updates`) is called from Place (`:1389`),
Cancel (`:1456`), Amend (`:1520`) and PurgeExpired (`:1555`); inside a batch, `apply_one` is called
per sub-request (`:1649-1656`), i.e. **up to 128 full scans per applied entry**.

**Cost.** `O(B + A)` just to discover there are zero pegs; `O(P·(B+A))` with `P` pegs, plus
`O(P log B)` remove/insert per reprice. With 128 rests into a 50k-order book this is ~6.4M order
visits per batch entry. Part IV §5's "rescan" failure class applies; our "BTreeMap O(log n)" guard
did not cover this path.

**Fix, staged.**
1. Low risk: `pegged_count` early-out when zero pegs, and hoist `best_non_pegged_*` into the
   `Primary`/`Midpoint` match arms only. Event order and keys are unchanged.
2. Medium-high risk: a `BTreeSet<BidKey>`/`BTreeSet<AskKey>` of pegged keys, so the pass visits only
   pegs, plus a non-pegged BBO index. Must be maintained on rest/cancel/full fill/STP cancel/amend
   re-insert/restore, and must keep the full (including pegged) BBO cross-check at
   `mod.rs:1371-1374` — `conformance_tests.rs:1332-1350` pins that behaviour. Add a
   `#[cfg(test)] assert_matches`-style full recomputation like `order_book/indexes.rs` already does.

**Measurability.** None of the benches sets `peg` or `peg_offset`. A peg-heavy fixture is required,
preferably as new `Op`/`Kind` variants in `benches/replay_mix.rs` (`:38-81`, `:183-218`) with a peg
update counter. Until then the cost is visible only indirectly in `apply_path` µs/entry and
`e2e`/`cluster_e2e` write latency, neither of which is a peg workload.

### F2 — `SymbolConfig::clone()` on every mutating apply

**Code.** `state_machine.rs:719-724`:

```rust
fn configured_symbol_for(&self, order: &Order) -> Result<SymbolConfig, RejectReason> {
    ...
    Ok(self.symbol_config.clone())
}
```

`SymbolConfig` owns two `AssetId { code: String }`, so each clone allocates two strings. Call sites:
Place (`:1303`), Cancel (`:1434`), and `validate_amend_values` (`:735`) for Amend;
`execute_order_with_symbol` takes the config by value.

**Cost.** Two heap allocations and two string copies per applied order, including batch sub-requests.
This is 2 of the 3.17 allocations in the measured `apply_path` rest row.

**Fix (low risk).** Return `&SymbolConfig` (or an error-only result) and add an internal
`execute_order_with_config_ref`; keep the public by-value wrapper for API compatibility. The config
is immutable once the symbol is locked and all validation only reads it.

**Measurability.** Strong: `apply_path --check` counts allocations exactly and would show rest
~3.17 → ~1.17 allocs/entry. Fewer allocations always pass the exact ceilings.

### F3 — Trailing-stop snapshot `Vec` per print

**Code.** `mod.rs:1171-1209`, specifically `:1177`:

```rust
if self.stop_index.has_trailing() {
    let trailing: Vec<u64> = self.stop_index.trailing_sequences().collect();
    for sequence in trailing { ... self.stop_index.retrigger(...) }
}
self.stop_index.on_last_trade(previous, price);
```

**Cost.** One small allocation per print (primary fill and each stop-child fill) when any trailing
stop is parked; the crossed-trigger range walk itself is already efficient. Fixed stops are not
scanned — the refactor fixed that.

**Fix.** Reuse a scratch `Vec<u64>` field in `OrderBook` (cleared per print), or stage updates so no
snapshot is needed. Risk: the snapshot exists because `retrigger` mutably borrows the index while
iterating; high/low-water reference updates must stay in trade order because a cascade depends on
intermediate prices.

**Measurability.** Weak. `replay_mix` has fixed stops but no `trail_offset` orders. Add a
trailing-stop case before claiming a gain.

### F4 — `depth()` walks every order at visited levels

**Code.** `mod.rs:1498-1528`: `Box<dyn Iterator<Item = &Order>>` over the side, then one iteration per
order, accumulating visible volume into at most `levels` `DepthLevel`s.

**Cost.** `O(orders traversed until N distinct prices)`, not `O(levels)` — 100k orders at one price
with `levels=10` walks all 100k. One guaranteed heap allocation and virtual dispatch per side per
query. Callers: `network/api.rs:808-816` (`orderbook_depth`) and market-data reset snapshots
(`market_data/mod.rs:1456-1462`, leadership changes only).

**Fix.** (a) Replace the boxed iterator with a generic helper called twice — removes allocation and
vtable, low risk, likely below noise. (b) Maintain per-price visible-volume/order-count aggregates
so depth jumps between levels: `O(levels)`, but hidden iceberg reserve must stay excluded and the
aggregate must be updated on partial fills, full removals, iceberg refills, STP cancels, amends and
restore — the same stale-state class the investigation warns about.

**Measurability.** Yes: `read_e2e` uses `/read orderbook_depth:20`; `level_book` and `sorted_book`
have a 10-level depth case over 100k orders. (b) only matters when many orders share a price.

### F5 — FOK preflight duplicates the sweep walk

**Code.** `mod.rs:2429-2458` (`fillable_volume`) boxed iterator over the opposite side, summing
`open_volume()` over crossing makers with STP skip/break; called at `mod.rs:1737` for FOK only, then
`sweep` walks the same makers again.

**Cost.** `O(K)` plus one box allocation per FOK, roughly doubling matching work for that order.

**Fix.** Fusing preflight into the sweep needs undo of per-fill mutations (STP cancels, iceberg
refills, event ordinals, expiry-index updates) on an unfillable FOK — high risk. Removing the `Box`
is trivial but marginal.

**Measurability.** No bench sets `TimeInForce::Fok` (only unit tests in `order_book/tests.rs`). Defer
until a FOK-at-depth fixture exists.

### F6 — Per-fill tree descent and large struct copies

**Code.** `mod.rs:2247-2418`: `resting.first_entry()` at `:2251` re-descends the tree after each
maker removal (`O(log N)` per maker, not once per sweep); `let maker = *best.get()` at `:2255`
copies ~256 B; the taker copy at `:2337` is another ~256 B; `MatchResult::new_with_policy`
(`:891`) builds a ~592 B record. `Fixed::notional` is computed at both `:2330` and inside the
constructor.

**Cost.** Roughly 1 KiB of memcpy per fill plus tree misses and geometric growth of the fills
`Vec`. With normal `F` of 1–5 the tree misses dominate; deep sweeps are the main cost.

**Fix ideas.** (a) A compact internal fill record carrying only what events consume; high risk
because `MatchResult` is public API and tests read `taker.side`, fee rates and `maker.volume`.
(b) Pass the precomputed notional into the constructor — trivial but below bench resolution.
(c) A cursor-capable tree to avoid re-descending — high risk; Rust `BTreeMap` cannot mutate through
a live iterator, and the external treap lead is not sufficient evidence.

**Measured trap.** An inline `SmallVec<[MatchResult; 1]>` fill buffer was tried and reverted:
`replay_mix` whole replay regressed ~107 ms → ~185 ms (`PERFORMANCE.md:448-452`). Do not retry
without a different design.

### F7 — Snapshot content copies the whole book under the lock

**Code.** `state_machine.rs:755-780` collects all bids/asks into two `Vec<Order>` (plus stops,
events and submissions) while holding the state-machine lock in `persist_snapshot` (`:1206`) and
`build_snapshot` (`:1238`); `vectorize::serialize` (`mod.rs:540-549`) builds another `Vec` of key
references, and `stop_orders::serialize` (`:576-585`) a third sequence.

**Cost.** `O(N)` large copies and `O(N)` pointer pairs per snapshot (100k logs or 30 minutes, plus
gRPC snapshot shipping). Encoding and writing already happen outside the lock.

**Fix.** Serialize directly from the map iterators with a custom adapter; keep the postcard
sequence shape byte-identical. Low-medium risk, likely second-order versus disk I/O.

**Measurability.** `p0_bench snapshot` and `p0_bench read` report the build/write and full-copy paths.

### F8 — Small and niche-path costs

| Item | Location | Cost / frequency | Idea | Bench? |
|---|---|---|---|---|
| Duplicate notional compute | `mod.rs:2330`, `:891` | one extra i128 mul/div per fill | pass value in; trivial | below noise |
| `client_order_id` cloned twice | `state_machine.rs:543,549` | two small `String` allocs per id-bearing Place | one `Arc<str>` shared by key and record | `apply_path` uses no ids |
| Validation `%` and `10i128.pow` | `mod.rs:2689-2743`, `fixed.rs:180-186` | ~8 divisions per Place | precomputed reciprocals in config | below bench resolution |
| `Fixed::checked_div` i256 | `fixed.rs:120-129`, `mod.rs:2320-2325` | 256-bit divide per quote-mode fill | guarded u128 fast path; rounding/overflow risk | no quote-mode bench |
| Fills `Vec` growth | `mod.rs:1633-1636`, `:2233` | reallocs 1,2,4,… per fill | reserve heuristic / stream into events; STP interleaving risk | `book_replay` sweeps |
| `install_content` indexes twice | `state_machine.rs:471-480` | `O(N log N)` twice per snapshot install | skip index updates during restore | rare; not worth it |

## 2a. Implemented findings and measurements (2026-11-26)

All changes are in the working tree, uncommitted. Two new `apply_path` modes provide the missing
measurements: `--peg` (a deep book with zero or 32 pegged bids) and `--trail` (eight parked trailing
stops and repeated prints). Both are informational; only the six standard workloads are gated.

| Finding | Change | Measured effect |
|---|---|---|
| F2 | `configured_symbol_for` returns `&SymbolConfig`; `execute_order_with_symbol` accepts `impl Borrow<SymbolConfig>`; no per-order `String` clones | `apply_path` rest 3.17 → **1.17** allocs/entry, cancel 3.00 → 1.00, fill 4.00 → 2.00, batch rest 3.19 → 1.19. Call ceilings tightened to 60k/52k/102k/155k. Gate passes. |
| F1a | `OrderBook::pegs` skips `reprice_pegged` entirely when no pegs are parked | `--peg` zero-peg row 18.9 µs → **0.42–1.02 µs**/entry (~20–45x) on a 5,000-order book |
| F1c | `peg_target` computes only the BBO the peg type needs (`LastTrade` needs none) | Included in the peg rows; no regression |
| F1b | `PegIndex` holds pegged bid/ask keys in ladder order; cancel/fill/amend/reprice maintain it; `reprice_pegged` iterates only pegs | `--peg` 32-peg row ~16–19 µs → **4.3 µs**/entry, allocations 5.17 → 2.17 |
| F3 | Reusable `trailing_scratch` in `OrderBook` replaces the per-print `Vec<u64>` | `--trail` 3.00 → **2.00** allocs/entry |
| F4a/F5 | Generic helpers replace `Box<dyn Iterator>` in `depth()` and `fillable_volume()` | Removes one allocation and virtual dispatch per call; below bench noise |
| Test guard | New conformance case `peg_index_tracks_cancel_amend_and_fill`, plus peg-count/index checks in `assert_indexes_consistent` | Property tests and 228 lib + 84 cluster tests pass |

Verification: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, full engine
`cargo test`, `apply_path --check` (tightened ceilings), `book_replay --check` and
`replay_mix --check` all pass; `replay_mix` warm p50 is 33–35 ms (~3.18M ops/s), consistent with the
post-index-refactor baseline in the executive summary.

`PegIndex` is a derived index and is rebuilt by `rebuild_derived_indexes` after snapshot install;
`#[cfg(test)] assert_matches` recomputes both key sets from the ladders after every randomized
operation. Event order is unchanged because the peg key sets use the same `BidKey`/`AskKey` ordering
as the maps they replaced.

## 3. Benchmark coverage and CI gates

Existing coverage (details in `PERFORMANCE.md:52-79`):

- `book_replay` and its `--matrix` mode: plain limits, cancels, amends and market takers only.
- `replay_mix`: icebergs and fixed parked stops; no GTD/Day deadlines, trailing stops, pegs, FOK,
  STP or explicit `PurgeExpired`.
- `level_book` / `sorted_book`: plain limits only; prototypes, not production code.
- `apply_path`: `StateMachineStore::apply` with blank/rest/cancel/fill and 128-request batches;
  allocation-counted, no advanced order types.
- `e2e` / `cluster_e2e` / `read_e2e`: write and read paths with shallow books; no advanced types.

The only peg workload is the new informational `apply_path --peg` mode; FOK, GTD/Day resting
orders and STP still have no bench. F5 has no FOK workload in CI. The `--trail` mode covers trailing
prints but no trailing-stop election cascade.

CI `bench-regression` runs `apply_path --check`, `book_replay --check` and `replay_mix --check`.
Exact `apply_path` ceilings (`benches/apply_path.rs:55-71`):

| Workload | Max calls | Max requested bytes |
|---|---:|---:|
| Blank | 0 | 0 |
| Rest, 1 event | 60,000 | 30,000,000 |
| Cancel, 1 event | 52,000 | 1,000,000 |
| Fill, 1 event | 102,000 | 120,000,000 |
| Batch blank | 130,000 | 80,000,000 |
| Batch rest, 128 × 1 event | 155,000 | 250,000,000 |

The hot rows were tightened on 2026-11-26 after F2 (`docs/PERFORMANCE.md` records the history). `replay_mix --check` bounds are whole-replay warm
p50 ≤ 1 s and p99 ≤ 2 s; `book_replay --check` uses loose per-case p50/p99 bounds. `--matrix` is
manual/Linux-only, and `--pin` fails loudly on this macOS host.

## 4. Non-negotiable constraints on any change

From the investigation's "Avoid" list and our own tests:

- Keep sparse 18-decimal `Fixed` prices and the replicated B-tree ladder; no fixed tick array.
- Keep deterministic `(log_index, ordinal)` event ids, exact fees/assets, and serial `apply`.
- Every terminal state change must emit an explicit event (cancel/expiry/STP are not `Reject`).
- Any cached derived state (BBO, depth aggregates, peg keys) needs a test-only full recomputation
  assertion; competitors' stale-BBO and expired-volume bugs are the cautionary examples.
- Do not trade Raft/WAL/snapshot guarantees for a simpler replication primitive (pitlane's
  `ReplicaLog` is a capped in-memory log, not consensus).

## 5. Sequence and remaining work

1. **Done — F2** — `SymbolConfig` by-reference path; `apply_path --check` ceilings tightened.
2. **Done — F1a + F1c** — `apply_path --peg` fixture, baseline, zero-peg early-out and per-peg BBO
   hoist. Event order was not touched.
3. **Done — F1b** — the 32-peg A/B still showed a 16 µs scan, so `PegIndex` now visits only pegs,
   with ladder-ordered keys and test-only full recomputation.
4. **Done — F3** — `apply_path --trail` showed one allocation per print; the reusable
   `trailing_scratch` removes it.
5. **Done — F4a / F5** — generic iterator helpers remove the boxed dispatch; no behavioural change.
6. **Open — F4b** — depth aggregates need a measured many-orders-per-level workload before adopting
   a cache with the same staleness risk as cached BBO.
7. **Open — F6–F8** — leave until a targeted bench isolates them; do not re-run the reverted
   fill-buffer or arena experiments without a different design.

## References

- [`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) — Parts I, II, IV, V, VI, VII and
  the consolidated takeaways; failure taxonomy in Part IV §5.
- [`PERFORMANCE.md`](./PERFORMANCE.md) — benchmark scope table, occupancy matrix, book-only re-pin,
  `apply_path` ceilings, external optimization notes, level/sorted-book verdicts.
- [`TODO.md`](../TODO.md) — shipped stop/GTD index item (2026-11-26 History entry) and remaining
  P2/P3 work.
- `match_engine/src/order_book/indexes.rs` — the new derived stop/expiry indexes and their
  test-only consistency checks.
