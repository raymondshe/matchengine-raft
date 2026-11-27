# Raft edge-case test matrix

> Engine commands below run from `match_engine/` or with
> `--manifest-path match_engine/Cargo.toml` from the repository root.

Date: 2026-11-27 · Baseline: `master` after the late-join coverage work

**Status 2026-11-27:** all 20 cases E01–E20 are implemented and pass in
`cargo test --test cluster` (236 lib + 106 cluster; the async-WAL case also
passes under `--features async-wal`). See §6a–§6c for where each one landed.

## 1. Purpose and scope

The matching engine is a critical service: a committed order must never be lost,
never be duplicated, and must be observed identically on every replica after a
leader change, a node restart, a membership change, or a storage/transport
fault. `docs/TESTING.md` inventories what exists; this document is the **design
for what is missing**, with concrete, deterministic cases.

Scope is the engine's Raft consensus, replication, storage durability, snapshot
and read-consistency paths. Application-level order-type conformance, market
data fan-out and the Kubernetes operator have their own suites. The control
plane's `/plan` scenarios are simulations only and do not exercise engine Raft.

### How to read the matrix

- **Priority** — P0 blocks a release; P1 is required before the next incident
  review; P2 is coverage hardening.
- **Injection** — how the fault is made deterministic. Existing hooks are
  called out; new hooks are listed in §5.
- **Invariant** — the assertion that must hold. Invariants are stated as
  *safety* (never wrong) or *liveness* (eventually progresses).

Baseline invariants for every case:

1. A sequence is assigned once; a committed sequence never disappears.
2. `event_id = (log_index, ordinal)` is globally unique and ordered; every
   replica exposes the same tape for the same committed prefix.
3. A `client_order_id`/`client_request_id` retry returns the original response
   even after a leadership change.
4. `/ready` is `200` only when the applied state covers the durable WAL tail;
   otherwise it is `503` and the operator must not roll the next pod.

## 2. Failure dimensions

| Dimension | Extremes to cover | Covered today | Biggest gap |
|---|---|---|---|
| Leadership | clean election, failover, transfer, split vote, deposed leader | clean election, kill-1 failover, console transfer | stale-log candidate, PreVote runtime proof, transfer under load |
| Quorum | 4/4, 3/4, 2/4, 1/4, partition 1+3 / 2+2 | 1 killed voter, 4-voter tail loss | **actual quorum loss**, partition/heal without divergent history |
| Replication | caught up, modest lag, purged-behind, reverted log | learner catch-up, snapshot install, late starter with lag | **automatic reversion watcher end to end**, stale match index |
| Membership | add learner, promote, remove, shrink, concurrent changes | add/promote/shrink, remove/re-add | interrupted change, concurrent change, 4-voter floor shrink |
| Snapshot | log-count, timer, install, restart, retention | all of these | install on a follower with a pre-existing cursor, cancellation |
| Storage | torn tail, corruption, ENOSPC/EIO, no-fsync tail loss | torn tail, corruption, append errors, tail loss | **pure index gap**, fsync failure, commit-marker crash point |
| Transport | stream append, separate channels, snapshot stream | fail-fast task, HTTP proxy retries | gRPC status mapping, listener loss, snapshot cancellation |
| Reads | local, lease, local-lease, read-index, follower hop | all policies at rest | stale lease → ReadIndex fallback, reads during transition |
| Time | request-driven GTD/Day, lease clock | request-driven expiry everywhere | clock skew, Day close across a transfer |

## 3. Prioritized scenario matrix

| ID | Scenario | Risk | Pri | Target file | New hook | Est. |
|---|---|---|---|---|---|---|
| RAFT-E01 | Quorum loss: 3 voters → 1, writes rejected, peers return, no divergent commit | Split brain / lost commit | P0 | `test_quorum.rs` | — | ~10 s |
| RAFT-E02 | Partition 1 vs 3: majority elects, minority steps down, heal, single history | PreVote / split brain | P0 | `test_partition.rs` | peer filter | ~15 s |
| RAFT-E03 | Automatic log-reversion recovery via `allow_next_revert` | Stuck follower with stale match index | P0 | `test_log_reversion.rs` | gRPC pause | ~15 s |
| RAFT-E04 | Pure WAL index gap recovery | Silent truncation of a valid suffix | P0 | `test_storage.rs` | gap injector | <1 s |
| RAFT-E05 | Interrupted membership change (kill leader mid-change) | Two valid configs / orphan learner | P0 | `test_membership.rs` | — | ~12 s |
| RAFT-E06 | Shrinking a live no-fsync cluster below four voters is rejected | Durability contract broken by API | P1 | `test_min_voters.rs` | — | ~8 s |
| RAFT-E07 | Leader transfer under sustained write load (gRPC `TransferLeader`) | Lost in-flight entry / duplicate response | P1 | `test_leadership.rs` | — | ~12 s |
| RAFT-E08 | Stale lease falls back to ReadIndex | Serving stale data under clock drift | P1 | `test_read_policy.rs` | lease skew | ~8 s |
| RAFT-E09 | Snapshot install on a follower with a pre-existing event cursor | Cursor gap / event replay divergence | P1 | `test_snapshot.rs` | — | ~10 s |
| RAFT-E10 | Concurrent writes across a failover: no loss, no duplicate | Committed prefix divergence | P1 | `test_leadership.rs` | — | ~15 s |
| RAFT-E11 | Injected fsync failure at the commit marker | False ack / lost commit | P1 | `test_failure.rs` | fsync error | ~8 s |
| RAFT-E12 | Snapshot stream cancellation and retry | Staged file leak / bad install | P1 | `test_snapshot.rs` | cancel gate | ~12 s |
| RAFT-E13 | Five voters, two lost (zone simulation) | Quorum math with even/odd sets | P2 | `test_quorum.rs` | — | ~12 s |
| RAFT-E14 | Cross-node event tape after failover + snapshot + truncation | Per-replica state divergence | P2 | `test_determinism.rs` | — | ~18 s |
| RAFT-E15 | Day close crossing a leadership transfer | Expiry applied at a different point per node | P2 | `test_determinism.rs` | — | ~10 s |
| RAFT-E16 | gRPC status mapping (`Unavailable`/`DeadlineExceeded`) | Backoff storm or silent stall | P2 | `test_transport.rs` | status injection | ~10 s |
| RAFT-E17 | Exhausted leader discovery and read auto-forward | Client hang / wrong-node read | P2 | `test_client.rs` | — | ~8 s |
| RAFT-E18 | Two learners joining concurrently | Membership race / stuck second learner | P2 | `test_membership.rs` | — | ~12 s |
| RAFT-E19 | Async WAL crash matrix (pending tail loss, purge/truncate gate) | Unflushed tail resurrected | P2 | `test_async_wal.rs` | delay hook | ~10 s |
| RAFT-E20 | Whole-cluster restart with RPO > 0 | Divergent tails after simultaneous crash | P3 | `test_quorum.rs` | process grid | ~20 s |

## 4. Detailed case specifications

### RAFT-E01 — Quorum loss and recovery (P0)

**Setup.** Three in-process voters on dedicated ports; `init`, promote, commit
20 orders. Capture `sequence=20` and the leader's `data_metrics.last_log`.

**Inject.** Stop two voters (one of them the leader) with `node.raft.shutdown()`
so their processes stay alive but Raft stops. Wait past two election timeouts.

**Observe.** `client.metrics()` on the survivor; `/write` and `/write_batch`
on the survivor; the survivor's `state`/`current_leader`.

**Invariants (safety).** No write returns `Ok` after quorum is lost; the
survivor's `last_applied` does not advance; no new sequence is assigned; the
survivor does not report itself as leader with a committed vote.

**Recover.** Restart one stopped node on its original storage. A leader is
elected within 15 s; `sequence` is exactly 20; a new write gets 21; both live
nodes expose the same book and event tape.

**Why.** Every existing failover test kills exactly one voter. Quorum loss is
the case where the engine must refuse service rather than guess.

---

### RAFT-E02 — Partition 1 vs 3, then heal (P0)

**Setup.** Four voters, no fsync, 20 committed orders. Requires the peer-filter
hook from §5 so a node's Raft gRPC traffic is dropped in one direction while
its HTTP admin endpoint stays reachable.

**Inject.** Isolate node 1 (the leader) from 2/3/4. The majority must elect a
new leader and commit more orders; node 1 must step down and reject writes.
Heal the partition.

**Invariants.**
- Exactly one leader at any time after convergence.
- Node 1's log after healing is a prefix of the majority's log; no entry it
  committed alone survives.
- PreVote is exercised: the healed node must not force a term bump that ousts
  the healthy leader. Assert `term` is stable across heal and that
  `/console/api/node` on the healthy leader still reports leadership.
- Final sequence and event tape equal on all four nodes.

**Why.** PreVote is enabled and has a dedicated RPC, but no runtime test proves
it prevents a partitioned node from disrupting the cluster.

---

### RAFT-E03 — Automatic log reversion recovery (P0)

**Setup.** Four voters, no fsync, 20 committed orders. Record node 2's acked
index from the leader's `metrics.replication[2]`.

**Inject.**
1. Keep node 2 in membership, pause only its Raft gRPC listener (§5) while
   leaving HTTP up.
2. Truncate several complete WAL frames from node 2's last segment using the
   existing `truncate_last_wal_records()` helper, so its persisted tail is
   below the leader's match index.
3. Resume its gRPC listener.

**Observe.** Leader logs for `follower log reverted; allowing one replication
reset` from `src/log_reversion.rs`; `data_metrics` on both nodes; `snapshot_sent`
and `append` counters.

**Invariants.**
- Within 30 s the leader's `matching` for node 2 is lowered and rebuilt.
- Node 2's `last_log`/`last_applied` reach the leader's commit.
- No other peer's progress is reset; no false reset is logged for healthy peers.
- Sequence and event tape match the leader.

**Why.** This is the exact incident that stalled the OrbStack rollout. The
watcher's helpers are unit-tested but `spawn()` is not.

---

### RAFT-E04 — Pure WAL index gap (P0)

**Setup.** Use `LogStore` directly with a tiny segment size and append 40
entries; snapshot the file bytes.

**Inject.** Rebuild one segment with a middle frame removed while keeping the
later frames intact — an index gap rather than a torn tail. (New helper:
`remove_wal_frames(path, range)`.)

**Invariants.** Reopen rejects the gap (strict) or truncates to the valid
prefix and deletes later segments (best-effort) per `docs/STORAGE.md`; it never
exposes a log with a hole. Appending after recovery assigns contiguous indexes.

---

### RAFT-E05 — Interrupted membership change (P0)

**Setup.** Three voters plus an added learner; 10 committed orders.

**Inject.** POST `/change-membership` to a four-voter set, then immediately
`node.raft.shutdown()` on the leader before the config commits. Wait for a new
election.

**Invariants.** The elected leader's membership is either the old three-voter
set or the new four-voter set — never a partial set; `add-learner` retries are
idempotent; writes commit; no orphan learner remains with a conflicting id.
Repeat with the change issued on a follower (must be forwarded or rejected).

---

### RAFT-E06 — Live four-voter floor (P1)

**Setup.** Real binary, no fsync, four voters, 10 writes.

**Inject.** `change-membership` down to three while writes are flowing.

**Invariants.** HTTP returns `400` with `min_voters`; membership still lists
four voters; writes keep committing; `/ready` stays `200`. Then stop one pod:
writes still commit (3/4 quorum) but membership is unchanged.

---

### RAFT-E07 — Transfer under load (P1)

**Setup.** Four voters, sustained `/write_batch` load from a client task.

**Inject.** Transfer leadership through the console action, then directly
through gRPC `TransferLeader` via `node.raft.trigger()`/`transfer_leader`.

**Invariants.** Every `Ok` response has a unique sequence and appears in
`/events` exactly once; in-flight retries return the original sequence; the
target becomes leader; a final read on every node is identical.

---

### RAFT-E08 — Stale lease fallback (P1)

**Setup.** Three voters, `local_lease` policy, 10 writes, record
`/read-policy/status` lease hits.

**Inject.** Use a debug clock-skew hook or a very short lease with a delayed
heartbeat so the next `/linearizable_read` finds the lease stale.

**Invariants.** The read falls back to ReadIndex; `read_index` counter
increments; no lease hit is recorded for that read; the read still observes the
latest committed sequence. Contrast with `--read-policy read_index`, which
must show zero lease attempts.

---

### RAFT-E09 — Snapshot install over an existing follower cursor (P1)

**Setup.** Three voters, 30 writes, take an `/events` cursor on a follower.

**Inject.** Snapshot + purge on the leader; force install on that follower.

**Invariants.** After install the follower's retained event window equals the
snapshot's; the old cursor either resumes at the snapshot boundary or reports
the eviction gap explicitly — never silently skips or duplicates an event.
Assert `oldest_event_id` on both nodes.

---

### RAFT-E10 — Concurrent writes across failover (P1)

**Setup.** Four voters, four client tasks each sending 200 keyed writes.

**Inject.** Kill the leader halfway through.

**Invariants.** Count all `Ok` responses: 800 distinct sequences, no gaps, no
duplicates; every sequence appears in each surviving node's `/events`; retries
of a timed-out key return its original sequence. This is the data-correctness
version of the existing `cluster_e2e` concurrency benchmark.

---

### RAFT-E11 — fsync failure at the commit marker (P1)

**Setup.** One node with fsync enabled; new `inject_fsync_error` hook on
`LogStore`.

**Inject.** Fail `sync_data` on the commit-marker flush (not on append).

**Invariants.** The node does not report the entry as committed; fail-fast
shutdown; after restart, `wait_for_recovery` blocks `/ready` until the state
machine covers the durable tail; no false ack was returned to the client.

---

### RAFT-E12 — Snapshot cancellation (P1)

**Setup.** Large (>100 KB) snapshot, debug install delay hook.

**Inject.** Cancel the receive future after the first chunk via the existing
`full_snapshot` cancel path.

**Invariants.** No `current.snap` is installed; the staging file is removed;
a subsequent install succeeds and is CRC-valid. Measure cancellation latency
as informational, per `docs/RAFT_REVIEW.md`.

---

### P2/P3 cases

- **E13** five voters with two stopped: quorum is 3, writes continue; stop a
  third: writes stop; restore any two: writes resume with no divergence.
- **E14** replay the same 200-order stream on two clusters; force a failover, a
  snapshot and a WAL truncation at different points; compare `/events` tapes
  byte for byte (excluding node-local metadata).
- **E15** issue `PurgeExpired` with a `day_close_ms` boundary exactly while
  transferring leadership; every replica expires the same orders once.
- **E16** inject tonic `Unavailable`, `DeadlineExceeded` and `Unknown` from the
  Raft server; assert `RPCError::Unreachable`/`Timeout`/`Network` mapping and
  bounded backoff, with no panic and eventual recovery.
- **E17** exhaust `ClientConfig` retries with no reachable member, then test
  `linearizable_read_auto_forward` from a follower.
- **E18** add two learners concurrently; both must install/catch up and be
  promotable one at a time.
- **E19** with `async-wal`, delay the fsync worker, kill the node, and assert a
  lost pending tail is not resurrected; `truncate_after`/`purge` must flush
  pending appends first.
- **E20** kill all four no-fsync nodes nearly simultaneously with different
  tails, then restart; assert the cluster converges to one history chosen by
  the quorum and that no previously acked response is lost.

## 5. Test infrastructure to add

All hooks must be `#[cfg(debug_assertions)]` or feature-gated and never linked
into the release binary.

| Hook | Location | Purpose |
|---|---|---|
| `GrpcNetworkFactory` peer filter | `src/network/grpc/client.rs` | Drop Raft traffic to/from selected peer ids for partition cases (E02). HTTP stays up so the watcher can still probe. |
| `MatchEngineNode::pause_raft_grpc()/resume_raft_grpc()` | `src/lib.rs`, `src/network/grpc/server.rs` | Stop only the tonic listener/task, keeping HTTP and Raft alive; deterministic reversion setup (E03). |
| `LogStore::inject_fsync_error()` | `src/store/log_store.rs` | Fail `sync_data` independently of `write_all` (E11). |
| `remove_wal_frames(path, range)` | test helper beside `truncate_last_wal_records()` | Inject a middle index gap (E04). |
| `DebugClock` offset for lease reads | `src/network/read_policy.rs` | Force stale-lease fallback without sleeping for wall-clock time (E08). |
| `ClientConfig::low_retries` test preset | `tests/cluster/common.rs` | Exhausted discovery/retry cases (E17). |
| Process grid helper | `tests/cluster/common.rs` | Start N real binaries with distinct `--id-offset`/ports for simultaneous crash cases (E20), reusing `start_node_process*` from `test_failure.rs`. |

Port allocation must stay disjoint from the ranges in `AGENTS.md`
(21001–21003, 21101…, 21941, 21611–21613 for late join). Proposed:
21711–21720 (`test_quorum`/`test_partition`), 21721–21730 (`test_leadership`/
`test_determinism`), 21731–21740 (`test_log_reversion`/`test_transport`),
all with the standard +1000 gRPC offset and `MATCHENGINE_TEST_PORT_OFFSET`
support.

## 6a. Implemented P0 cases (2026-11-27)

| ID | Test | Notes |
|---|---|---|
| E01 | `tests/cluster/test_quorum.rs::quorum_loss_rejects_writes_and_recovers_without_divergence` | 3 voters → 1; asserts both single and batch writes are rejected, applied index and sequence are frozen, then restart one voter and require sequence 21 with identical book and event tape. |
| E02 | `tests/cluster/test_partition.rs::restarted_former_leader_with_stale_log_cannot_disrupt_quorum` | Implemented as a full stop + restart of the former leader rather than an iptables partition; proves the PreVote-on-rejoin property (stable term, no divergent history, single leader). No new network hook was needed. |
| E03 | `tests/cluster/test_log_reversion.rs::automatic_reversion_watcher_repairs_a_truncated_follower_tail` | Full `spawn()` path. `MATCHENGINE_REVERSION_SAMPLE_MS` / `MATCHENGINE_REVERSION_REQUIRED_SAMPLES` (defaults 5 s / 2, bounds 10 ms–60 s and 1–10) let the test arm the watcher at 50 ms / 1 sample before a conflicting append. Asserts the log line is for `peer=2` only, then convergence and a further commit. |
| E04 | `tests/cluster/test_storage.rs::a_middle_index_gap_truncates_to_the_valid_prefix` | New `remove_middle_wal_frames()` helper; strict recovery rejects the gap, best-effort keeps a contiguous prefix and appends with contiguous indexes. |
| E05 | `tests/cluster/test_membership.rs::interrupted_membership_change_converges_to_a_valid_config` | Interrupted `/change-membership`; asserts one of the two valid final config sets, idempotent learner retry, further writes commit, follower change is not applied locally. |

Ports in use: 21711–21713 (quorum), 21714–21717 (partition/rejoin), 21721–21724
(membership), 21731–21734 (reversion). All are `TestAddr`-based and respect
`MATCHENGINE_TEST_PORT_OFFSET`.

The only production change is the two env-overridable watcher constants in
`src/log_reversion.rs`; defaults preserve the shipped behavior exactly.

## 6b. Implemented P1 cases (2026-11-27)

| ID | Test | Notes |
|---|---|---|
| E06 | `tests/cluster/test_min_voters.rs::live_no_fsync_cluster_rejects_shrink_below_four_voters` | In-process `StorageConfig::with_min_voters(4)`: typed and raw `change-membership` to three voters are rejected with 400, membership stays at four, all nodes Ready, writes keep committing; then one configured voter is stopped and the remaining three still commit. Uses ports 21751–21754. |
| E07 | `tests/cluster/test_leadership.rs::leader_transfer_under_sustained_write_load` | Transfers via the console action and directly through `Raft::trigger().transfer_leader()` under a continuous write-batch load; asserts unique sequences, one event per committed sequence, original responses on key retries, and identical final state on all nodes. Ports 21741–21744. |
| E08 | `tests/cluster/test_read_policy.rs::stale_lease_falls_back_to_read_index` | Debug-only `MATCHENGINE_TEST_LEASE_SKEW_MS` models an expired caller-side lease; asserts the `local_lease` path takes a real `ReadIndex` fallback with the expected counters and still observes the latest commit. Ports 21946–21948. |
| E09 | `tests/cluster/test_snapshot.rs::snapshot_install_preserves_a_followers_event_cursor_contract` | Follower takes an `/events` cursor, falls behind across snapshot/purge, installs the leader's snapshot, and the old cursor must either resume at the boundary or report an explicit eviction gap — never silently skip or duplicate. Ports 21104–21106. |
| E10 | `tests/cluster/test_concurrent.rs::concurrent_writes_across_failover_have_no_loss_or_duplicates` | Four tasks x 200 keyed writes with the leader killed halfway; 800 contiguous distinct sequences, identical event tapes on all survivors, and a timed-out key retries to its original sequence. Ports 21745–21748. |
| E11 | `tests/cluster/test_failure.rs::fsync_failure_at_the_commit_marker_is_fatal_without_a_false_ack` | Debug-only `LogStore::inject_fsync_error()` fails the next `sync_data` after the commit marker was appended; the write is not acked, the node fail-stops, and after reopen `/ready` stays 503 until recovery covers the durable tail. Port 21949. |
| E12 | `tests/cluster/test_snapshot.rs::cancelled_snapshot_install_removes_staging_and_retry_succeeds` | Debug-only `MATCHENGINE_TEST_SNAPSHOT_CANCEL_AFTER_CHUNKS` (+ optional target) drops the snapshot stream mid-transfer; asserts no `current.snap` and no staging file, then a clean retry installs and validates the snapshot. Ports 21107–21108. |

All new hooks are `#[cfg(debug_assertions)]` or test-env-only with inert
defaults. The only non-gated source edit is an equivalent `sync_data()?`
rewrite in `append_blocking_inner`. Production behavior is unchanged.

## 6c. Implemented P2 cases (2026-11-27)

| ID | Test | Notes |
|---|---|---|
| E13 | `tests/cluster/test_quorum.rs::five_voters_survive_two_then_lose_quorum_and_recover` | 5 voters, 3 needed: two stopped still commit, a third stops writes, restoring two voters recovers exactly the frozen prefix. Ports 21761–21765. |
| E14 | `tests/cluster/test_determinism.rs::identical_order_stream_yields_identical_event_tapes_after_failover_snapshot_and_truncation` | Two clusters run the same order stream with a failover in one and snapshot/purge plus a WAL truncation in the other; final event tapes, book, sequence and last trade are identical. Ports 21771–21776. |
| E15 | `tests/cluster/test_determinism.rs::day_close_across_a_leadership_transfer_expires_once_everywhere` | `PurgeExpired` around a leadership transfer expires exactly the same orders once on every replica. Ports 21777–21780. |
| E16 | `tests/cluster/test_transport.rs::grpc_statuses_map_to_raft_errors_and_recover` | Debug-only `MATCHENGINE_TEST_GRPC_STATUS_CODE`/`_TARGET` injects `Unavailable`, `DeadlineExceeded` and `Unknown` into unary/stream append for one follower; quorum survives, no panic, target catches up after clearing. Ports 21781–21783. |
| E17 | `tests/cluster/test_client.rs::exhausted_leader_discovery_returns_a_typed_error` and `follower_linearizable_read_auto_forwards_to_the_leader` | Typed error instead of a hang with only dead members; auto-forward read observes the leader's commit while the follower stays a follower. Ports 21785–21788. |
| E18 | `tests/cluster/test_membership.rs::two_concurrent_learners_catch_up_and_promote_one_at_a_time` | Two learners added before catch-up both install/promote sequentially; five voters, no joint config, identical state. Ports 21789–21793. |
| E19 | `tests/cluster/test_async_wal.rs::async_wal_pending_tail_is_not_resurrected_and_truncate_flushes_pending` | Feature `async-wal`: delayed fsync drops the pending tail on reopen; `truncate_after`/`purge` fire `IOFlushed` before structural changes. |
| E20 | `tests/cluster/test_quorum.rs::whole_cluster_restart_with_unsynced_tails_converges_to_one_history` | Four no-fsync nodes stopped at different write counts restart and converge to one committed history with no lost ack. Ports 21766–21769. |

The gRPC status hook is debug-only and inert by default (unset code, optional
target substring); the per-request injection on `AppendStream` matters because
rejecting only the handshake would leave an already-open stream healthy.

## 6. Implementation order and acceptance gates

1. **P0 batch** — E01, E02, E03, E04, E05 together with the hooks they need.
   Gate: `cargo test --test cluster` green, no new `debug_assert` panics, and
   each case runs in isolation and with `--test-threads=4`.
2. **P1 batch** — E06…E12. Gate: repeat each case 10 times to catch timing
   flakes; record p50/p99 of recovery time as informational in the test output.
3. **P2/P3 batch** — E13…E20. Gate: feature-gated cases run in CI only where
   their feature is already checked (`async-wal`, `profiling`).

Every case must:
- use its own `TempDir` and port range;
- shut down all nodes on success **and** panic (existing `catch_unwind` pattern);
- assert on the durable state (`/events`, `/read orderbook_sequence`, WAL
  files), not only on log lines;
- print the observed indices/invariants on failure so a flake is diagnosable.

## 7. Open questions

- Is the four-voter floor a *library* contract or only a binary contract? E06
  assumes the binary; `StorageConfig::min_voters` defaults to 1 for callers.
- Should E02's partition hook drop traffic symmetrically or one-way? One-way
  is enough to force a deposed leader but asymmetric drops can mask PreVote.
- For E11, is a failed `sync_data` fatal, or should the node retry? The current
  fail-fast contract says fatal; the test will pin that decision.
- E20 may be inherently non-deterministic; it should assert safety (one chosen
  history, no lost ack) and treat which node wins as informational.

## 8. Relationship to existing suites

This matrix complements, and does not replace, `docs/TESTING.md`:
single-node storage corruption, snapshot framing, order-book property tests,
replay matrix and fuzz targets remain unchanged. The new cases are all
multi-node `tests/cluster` targets and run under plain `cargo test`.
