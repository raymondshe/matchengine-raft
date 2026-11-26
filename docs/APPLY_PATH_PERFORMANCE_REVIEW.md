# Raft apply-path performance review — open items

- **Date:** 2026-11-26
- **Scope:** `match_engine/src/store/state_machine.rs`, `match_result/mod.rs`, `store/types.rs`, and
  the Raft plumbing in `lib.rs`, `network/api.rs` and `store/log_store.rs`.
- **Status:** this file lists **only open items**. Completed findings and their before/after
  measurements are recorded in [`PERFORMANCE.md`](./PERFORMANCE.md) and `git log`; they are not
  repeated here.
- **Line numbers:** refer to the code state at review time. Re-check before editing.

## Open items

### A4 — Peg repricing and GTD purge run once per batch sub-request

`apply_one` calls `append_peg_updates` in every arm (Place, Cancel, Amend, PurgeExpired), and Cancel
and Amend each run their own GTD purge. A 128-request Batch therefore repeats both passes up to 128
times inside one lock acquisition.

Measured by `apply_path --batch-peg` (128 rests over a 32-peg book): **2.21 µs/entry** versus
**0.22 µs/entry** without pegs (~10x), at 1.21 allocs/entry. `apply_path --peg` covers only the
single-entry case.

This is deliberately open: `EventId = (log_index, ordinal)`, and each sub-request's
`first_ordinal`/`event_count` define which events belong to its response and dedupe record. Hoisting
a peg or expiry pass to the end of the entry would move those events between sub-requests. Needed
before acting:

- a batch+GTD fixture (deadlines due inside a batch) alongside `--batch-peg`;
- an ordinal-preserving design that keeps each sub-request's event slice intact.

A non-pegged BBO index could reduce the per-peg walk without moving events, but it needs the same
derived-index consistency assertions used by `order_book/indexes.rs`.

### B3 — Openraft state-machine channel and unbounded buffer are not exposed

`state_machine_channel_size` (default 1,024) and `enable_unbounded_state_machine_buffer` exist in
openraft 0.10 but are not set by `default_raft_config()` in `lib.rs`. When apply falls behind,
RaftCore blocks in `send_sm_command` while continuing to drain notifications.

Upstream A/B results are mixed: an unbounded buffer helps at high client counts single-node but
worsens tails with three nodes, and it grows memory without bound. Open only after
`cluster_e2e --features profiling` plus `/metrics` runtime histograms show the SM channel as the
bottleneck; measure with the high-concurrency sweep before deciding.

### B4 — Large local reads copy the whole book under the state-machine lock

`/read orderbook_orders` clones all orders while holding the mutex (`api.rs` → `read_state_owned`
→ `StateMachineStore::orders`). JSON serialisation already happens after release, and `p0_bench read`
measures the writer/read contention with 100k orders and 8 writers.

A shadow/copy-on-read view could remove the clone, but it is only safe for local `/read`:
`/linearizable_read`, `/events` and gRPC `StreamEvents` must keep reading the authoritative state.
Open only after `p0_bench read` shows a real bottleneck.

## Measurement gaps still open

- No bench can supply `Some(responder)`: openraft's `ApplyResponder` cannot be constructed outside
  the crate, so the response/send handoff needs an instrumented Raft run. Existing `apply_path`,
  `p0_bench` duplicate and snapshot workloads all use `None`.
- No follower-apply workload, so the remaining per-follower costs are unmeasured.
- `save_committed`'s contention with concurrent replication appends on the global `write_order`
  lock is unmeasured; the closest instrumentation is `cluster_e2e --features profiling` and `e2e`
  with versus without `--match-event-log`.

## Constraints that any fix must respect

- Replicated transitions depend only on the entry; the one wall-clock read is the node-local
  market-data reset and must stay out of book/sequence/dedupe state.
- Event ordinals stay continuous across a batch; each submission's event slice must match its
  recorded `first_ordinal`/`event_count`.
- Window and dedupe updates happen before publishing; sink failures are logged and never fail apply.
- `last_applied_log` and snapshot content are captured in one critical section.
- Market-data depth capture, reset snapshot and gate opening are one critical section.

## Order

1. A4, after the batch+GTD fixture and an ordinal-preserving design exist.
2. B3 and B4, only after profiling (`cluster_e2e --features profiling`, `p0_bench read`) shows the
   bottleneck. Durability mode and write admission are deployment decisions, not code work.

## References

- [`PERFORMANCE.md`](./PERFORMANCE.md) — measured apply-path results and benchmark scope.
- [`ORDER_BOOK_PERFORMANCE_REVIEW.md`](./ORDER_BOOK_PERFORMANCE_REVIEW.md) — open book-core items.
- [`TODO.md`](../TODO.md) — roadmap.
- `match_engine/src/store/state_machine.rs`, `network/api.rs`, `store/log_store.rs`, `lib.rs`.
- openraft 0.10.0 source — `core/sm/worker.rs`, `core/raft_core.rs`,
  `storage/v2/entry_responder.rs`, `storage/v2/apply_responder_inner.rs`.
