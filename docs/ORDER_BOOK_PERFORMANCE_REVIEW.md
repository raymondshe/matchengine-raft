# Order-book performance review — open items

- **Date:** 2026-11-26
- **Scope:** `match_engine/src/order_book/` and the apply/snapshot paths that drive it.
- **Status:** this file lists **only open items**. Completed findings, before/after measurements and
  the external-idea verdicts are in [`PERFORMANCE.md`](./PERFORMANCE.md) and `git log`; they are not
  repeated here.
- **Line numbers:** refer to the code state at review time. Re-check before editing.

## Open items

### F4b — Depth aggregates for many orders at one price

`OrderBook::depth()` walks every order at the visited price levels, so its cost is
O(orders until N distinct prices), not O(levels): 100k orders at one price with `levels=10` walks all
100k. The boxed iterator and virtual dispatch were already removed; the remaining cost is the walk.

An aggregate of visible volume and order count per price would make this O(levels). It must exclude
hidden iceberg reserve and be maintained on partial fills, full removals, iceberg refills, STP
cancels, amendments and snapshot restore — the same stale-derived-state risk the external reviews
warn about. Add an `assert_matches`-style full recomputation to `OrderBook` if it is attempted.

Measurable with `read_e2e` (`/read orderbook_depth:20`) and the depth case in `level_book` /
`sorted_book`, but those benches use one order per level; a deep same-price fixture is needed to show
the win.

### F6 — Per-fill tree descent and large struct copies

`OrderBook::sweep_side` re-descends `BTreeMap::first_entry()` after each maker removal (O(log N) per
maker over a sweep) and copies roughly 1 KiB per fill: a ~256 B `Order` for the maker, another ~256 B
for the taker, and a ~592 B `MatchResult`. `Fixed::notional` is also computed twice per fill.

Candidate directions, none adopted:

- a compact internal fill record carrying only what events consume, with a compatibility wrapper
  for the public `MatchResult` (high risk — tests read `taker.side`, fee rates and `maker.volume`);
- pass the precomputed notional into the constructor (trivial but below bench resolution);
- a cursor-capable tree to avoid re-descending (high risk; `BTreeMap` cannot mutate through a live
  iterator, and the external treap lead is not evidence for our workloads).

**Measured trap:** an inline `SmallVec<[MatchResult; 1]>` fill buffer was tried and reverted —
`replay_mix` whole replay regressed ~107 ms → ~185 ms. Do not retry without a different design.

### F7 — FOK preflight duplicates the sweep walk

`fillable_volume()` sums `open_volume()` over crossing makers, applying STP skip/break, then
`sweep` walks the same makers again — roughly double the matching work for a fill-or-kill order.
The boxed iterator was already removed; fusing the preflight into the sweep is still open because
an unfillable FOK must roll back per-fill mutations (STP cancels, iceberg refills, event ordinals,
expiry-index updates). No bench sets `TimeInForce::Fok`; add a FOK-at-depth fixture before acting.

### F8 — Snapshot content still copies the whole state under the lock

`StateMachineStore::to_content()` copies all resting orders, stops, up to 262,144 retained events and
262,144 dedupe records while holding the state-machine lock. Encoding and writing already happen on
`spawn_blocking`, and the two temporary order Vecs were removed. The remaining structural copy needs
a copy-on-write or incremental capture scheme that keeps `last_applied_log` and the content
atomically consistent. `p0_bench snapshot` measures the resulting writer latency spike; gains are
likely second-order versus disk I/O.

### Small, niche-path costs — deferred

| Item | Site | Idea | Bench? |
|---|---|---|---|
| Duplicate notional compute | `order_book/mod.rs` sweep + `MatchResult::new_with_policy` | pass the value in | below noise |
| Validation `%` and `10i128.pow` | `order_book/mod.rs`, `fixed.rs` | precomputed reciprocals in config | below bench resolution |
| `Fixed::checked_div` i256 | `fixed.rs`, quote-mode sweep | guarded u128 fast path; rounding/overflow risk | no quote-mode bench |
| Fills `Vec` growth | `Outcome` and sweep | reserve heuristic / stream into events; STP interleaving risk | `book_replay` sweeps |
| `install_content` rebuilds indexes twice | `state_machine.rs` | skip index updates during restore | rare; not worth it |

## Measurement gaps still open

No benchmark covers FOK, GTD/Day deadlines on resting orders, STP/owner_id, or a trailing-stop
election cascade. `apply_path --peg`, `--trail`, `--sink`, `--tap` and `--batch-peg` cover the peg,
trailing-print, sink and batch-peg cases. No treap/heap-hybrid ladder prototype exists; a sorted
vector and per-level buckets were A/B'd and not adopted. `--pin` is Linux-only; `--matrix` is manual.

## Constraints that any fix must respect

- Keep sparse 18-decimal `Fixed` prices and the replicated B-tree ladder; no fixed tick array.
- Partial fills keep maker identity; iceberg reserves stay invisible; deletions and amendments must
  keep price-time priority and continuous event ordinals.
- Any cached aggregate needs test-only full-recomputation checks; competitor stale-BBO and
  expired-volume bugs are the cautionary examples.
- Do not weaken Raft/WAL/snapshot guarantees or emit fewer lifecycle events to match a simpler ABI.

## Order

1. F7 first: add a FOK-at-depth fixture, then decide whether rollback complexity is justified.
2. F4b next: add a deep same-price depth fixture, then A/B aggregates with full consistency checks.
3. F6 and F8 only with targeted benches; do not repeat the reverted fill-buffer or arena experiments.

## References

- [`PERFORMANCE.md`](./PERFORMANCE.md) — measured book/apply results and measurement discipline.
- [`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) — external ideas and verdicts.
- [`APPLY_PATH_PERFORMANCE_REVIEW.md`](./APPLY_PATH_PERFORMANCE_REVIEW.md) — open apply-path items.
- [`TODO.md`](../TODO.md) — roadmap.
