# TODO

Last reviewed: 2026-11-27. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

**Current baseline.** The engine runs OpenRaft 0.10.0 with a segmented WAL, snapshot v11, exact
18-scale `Fixed` values, the full v11 order-type set with STP and asset-aware fees, a replicated
event log, local-lease reads, bounded admission and Aeron/NATS market-data egress. One process or
cluster trades one symbol; multi-symbol isolation is deployment-level. The control plane manages
seven `matching.io/v1` CRDs through the API server, with a Lease-elected operator, one-pod-at-a-time
rolls, release/capacity gates, cross-shard migration, scheduled signed backups and a manager GUI
with create/edit/scale/rollback and read-only preview plans for both shards and market-data
services. Four fast-only market-data services and one global terminal service feed the React
terminal. The live lab currently has the `match-observability` stack intentionally scaled to zero
to save memory requests; restore it before relying on Grafana, Prometheus or Loki.

Priority key: **P0** = known correctness/security defect, do next; **P1** = production safety and
operability; **P2** = evidence, scale and important non-blocking work; **P3** = later/optional.
Ranking rule: impact, then whether the cheap prevention window is open now, then effort. Items
marked **blocked** are production gates: they stay in the queue but do not count as progress in
this lab.

## Ranked queue (authoritative)

Nothing is P0. The engine, client and control plane are green on tests and the live lab; the five
P0 consensus edge cases from `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` are closed in History. This is the
single ranked list; the detail below only explains caveats and deferral decisions.

| # | Pri | Item | Status | Notes |
|---:|---|---|---|---|
| 1 | P1 | Control-plane, engine-admin and public-edge security (#1) | open | Manager OIDC/mTLS, authorization, per-route permission bits, test/live isolation with a `409` cross-mode refusal, console-proxy auth, destructive-action approvals, idempotency keys, namespace-qualified paths, dry-run diffs and audit. Add TLS/auth, rate limits, origin checks, NetworkPolicy and Pod Security to the order-system, market-data and NATS edges; mTLS/shared-secret for engine `/init`, `/add-learner`, `/change-membership` and gRPC; session-bound `user_id` for the order desk; Secret-backed TDengine credentials and no public REST Ingress. Closes Grafana `admin`/`admin` and the plaintext lab TDengine endpoint. Highest impact and the cheapest prevention window. |
| 2 | P1 | Supply-chain and namespace hardening (#20) | open | Pin Actions to commit SHAs and base images to digests, `permissions: contents: read`, `match-eu-1` restricted with default-deny ingress/egress and a TDengine NetworkPolicy, narrow the bridge ClusterRole and backup pod-create RBAC. Small and self-contained; ride alongside #1. |
| 3 | P1 | Raft P1 edge cases E06–E12 | done 2026-11-27 | Closed in History; the matrix records each test and hook. E13–E20 remain open at #8. |
| 4 | P1 | Gateway integration | deferred by decision | Leader-aware routing from `SymbolRoute.status`, shadow/cutover/drain, retries and a canary path. Deferred by `docs/GATEWAY_ROUTING.md`: revisit only when TLS/auth and packaging land, a second client that cannot use the Rust client appears, or cutover needs automating. Retry `Place` with `client_order_id`; include `client_request_id` on `Cancel`/`Amend` retries. |
| 5 | P1 | Backup, registry and storage close-out | blocked here | One cycle against a real object-storage bucket (upload, retention, alerts), a real registry for digest-pinned images, and one CSI volume-snapshot drill. The local-PVC path, `docker save`/`load` and signed CRD drill are validated. A local `DiskPressure` image GC can otherwise leave a roll with nothing to pull. |
| 6 | P1 | Multi-node NATS | blocked here | Credentials/NKey in source and sink, a multi-node NATS cluster and optional JetStream retention. The single-node Core NATS path and acceptance `nats` phase pass. |
| 7 | P1 | Multi-host fabric and bare-metal evidence | blocked here | Second host and Linux four-voter cluster for read/write and lease A/B, forced stale lease to observe the `ReadIndex` fallback, Aeron multicast across hosts, opt-in cgroup v2 supervisor isolation, and the pinned-core `bench-linux` workflow on the bare-metal runner. OrbStack proves the Linux code path but is one host without cgroup delegation. |
| 8 | P2 | Raft P2 edge cases E13–E20 | done 2026-11-27 | Closed in History; all 20 cases in `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` are now covered. |
| 9 | P2 | Rollout invariants and acceptance automation (#4) | open | Extract the rollout candidate decision into a pure function (one terminating pod, replacement Ready before the next deletion, no skip while old pods remain), then run the nine-phase `scripts/operator-acceptance.sh` in a disposable CI cluster or nightly. Cheap unit tests first. |
| 10 | P2 | Ordered pre-trade rule pipeline (#5) | open | Fixed first-failure-wins edge/gateway gate with typed codes: halt, blacklist, notional cap, per-account/per-market flow windows, cancel-ratio, cancelled-amount-ratio, then self-trade screening. Policy-side only; never put user state in the stateless desk or matching core. |
| 11 | P2 | Historical bar store, remaining phases (#16) | open | P0 and all of P1 are shipped (History). Next: archive importer and reconciler with gap-gated reads; then three-dnode `REPLICA 3`, reader HPAs, backup and offset-anchored intervals. Depends on #1 for Secret-backed DSN and NetworkPolicy. |
| 12 | P2 | Trading terminal CRD integration (#17) | done 2026-11-27 | Closed in History. The operator renders the terminal from a `TradingTerminal` CR; the static `deploy/terminal.yaml` is now a legacy fallback. Never call engine `/write` directly. |
| 13 | P2 | Docs synchronization (#8) | open | Keep `AGENTS.md`, READMEs, `docs/API.md`, `docs/ETCD_STORAGE.md`, `docs/TESTING.md`, `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` and runbooks aligned. Fix the known Profile A/NATS drift first (`k8s_control_plane/README.md` still says Profile A is the only rendered transport); add a generated-doc diff guard and paired EN/CN updates when that surface exists. |
| 14 | P2 | Engine bar emission for sparse markets (#11) | open | `BarAgg::new` seeds `last_live_ms` with the trade time, suppressing the opening emit; seed it with `0` and test a single-fill bar. The terminal no longer depends on it, but the engine feed is authoritative for other consumers. |
| 15 | P2 | Stop-the-world incompatible upgrade execution (#6) | open | Automate the coordinated cutover (backup gate, disable routes, stop all symbols, apply image/release, restart, verify, re-enable) and record a drill. The release gate and manual runbook exist. |
| 16 | P2 | Profile A feed hardening (#12) | open | Characterize the Aeron image-registration race and single-node multi-subscriber behavior, make source startup readiness-gated (or add a per-source generation), keep the 5–30 s re-subscribe watchdog without restart loops. |
| 17 | P2 | CRD scale and soak (#13) | open | Thousands of symbols/CRs across shards and cells, status churn, API-server/etcd size, informer memory and operator workqueue latency. No real engine load required. |
| 18 | P2 | Defence-in-depth hardening (#19) | open | Nonce-valued CSRF guards for console/desk actions, manager GUI CSP, correlation IDs instead of internal error strings on 500s, and an admission gate for concurrent `/events` long-polls. Keep separate from the #1 auth workstream. |
| 19 | P2 | Observability extras | open | Expose the soak's queue-depth, encode-CPU and close-reason counters in Prometheus/Grafana and surface alert state in the manager status view. The JSON→text bridge, alerts and dashboard exist; the lab stack is currently scaled to zero (restore first). |
| 20 | P2 | Optional Aeron profiles | blocked on benchmark | Only if the NATS benchmark misses the latency target: direct multicast where the multi-node CNI/VPC test passes, or the per-pod IPC bridge. Keep unique stream bases per process, a cell-unique `symbol_id`, distinct Aeron directories and no shared `/dev/shm`. |
| 21 | P3 | Deferred performance experiments | open | `docs/ORDER_BOOK_PERFORMANCE_REVIEW.md`: depth aggregates (F4b), per-fill copy/fill-buffer reduction and FOK preflight fusion (F6), snapshot-copy/validation micro-costs (F7–F8), `/klines` query coalescing, WebSocket frame precomputation and catalog snapshot reuse. Targeted bench first. Apply-path remainders are in `docs/APPLY_PATH_PERFORMANCE_REVIEW.md`. |
| 22 | P3 | GUI follow-ups | open | Market-data create/edit/scale/plan/rollback is **done**. Remaining: order-system create/edit and image-rollback actions, migration/promote forms, approvals, filters/pagination, diff/rollback and SSE/WebSocket updates. |
| 23 | P3 | Lab-only hardening | open | Remove root usage and iptables from `match_engine/docker/linux.Dockerfile`; scope the ingress-nginx ClusterRole's Secrets access. Keep both out of production overlays. |
| 24 | P3 | Automation surface | open | CLI `create-shard`/`create-symbol`/`migrate`, an OpenAPI client, and admission validation/defaulting beyond the CRD YAML. |
| 25 | P3 | Multi-cell federation | open | Cell registration/capacity, CR projection and cross-cell migration. The demo has one cell. |
| 26 | P3 | Engine optional work | open | Wire-version negotiation (keep the release gate until then), exposing `purge_batch_size` with `max_in_snapshot_log_to_keep`, a standalone `STREAM_APPEND_CHANNEL_SIZE` benchmark, `on_leader_change` callbacks, peer preconnect/larger frames/snapshot compression, bounded-admission backpressure, async-WAL productionization, Aeron Raft transport, binary order entry, ITCH replay, binary/compressed WebSocket payloads only after a measured JSON bottleneck, derivative fields, reserve replenishment/collars, protobuf/Avro, and AON/order-mask conformance cases. |
| 27 | P3 | Optional archive and replay | open | Persist feed archives to object storage or a PVC with retention/checksums and replay through the same assembler. Archives are data, never desired state in etcd. |
| 28 | P3 | Operator documentation | open | Metrics, troubleshooting, capacity planning and disaster-recovery runbooks. |
| 29 | P3 | Module renaming | open | Decide whether to rename mixed-style modules (`src/match_result/` → `match_events/`) before 1.0. Import-breaking, no functional benefit today. |

The `P0`–`P5` phases inside the historical-bar design are implementation phases, not this file's
P0 defect priority; the historical-bar store (#11 in this queue) is P2 here.

## Caveats and deferral decisions

- **Gateway (#4):** no public gateway is built until TLS/auth and packaging land, a second client
  that cannot use the Rust client appears, or cutover needs automating. The design stays in
  `docs/GATEWAY_ROUTING.md`.
- **Local-lease semantics (#3, #7):** idempotency keys cover retries; `Place.client_order_id` and
  `Cancel`/`Amend.client_request_id` share one dedupe namespace, and a follower write returns a typed
  503 with the leader address. Unkeyed mutations must not be blind-retried. `POST /events` consumers
  resume from `(log_index, ordinal)` and dedupe on `event_id`.
- **Control-plane truth (#1, #5, #24):** desired state stays in CRDs through the API server; never
  connect to etcd directly and never put WAL/snapshot/order-book/events/market data in etcd. Use
  Postgres/object storage only for audit, a query/read model or long-term history.
- **Observability:** the lab stack is intentionally off to save memory requests; restore it before
  starting observability work or relying on alerts/dashboards.
- **Raft queue (#3, #8):** the design, hooks, invariants and proposed port ranges are in
  `docs/RAFT_EDGE_CASE_TEST_MATRIX.md`; P0 cases are already closed in History.

## History

- **2026-11-27** — P2 Raft edge cases E13–E20 from `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` implemented,
  closing all 20 cases. `test_quorum.rs` covers five voters losing quorum and a whole-cluster
  no-fsync restart with different tails; `test_determinism.rs` compares identical event tapes across
  failover, snapshot/truncation and a Day close during transfer; `test_transport.rs` injects tonic
  statuses into unary and stream append; `test_client.rs`, `test_membership.rs` and `test_async_wal.rs`
  cover exhausted discovery, read auto-forward, concurrent learners and pending-tail loss. Engine
  suite: 236 lib + 106 cluster tests, plus 3 async-WAL tests, fmt and clippy pass.
- **2026-11-27** — Trading terminal CRD integration (#17) shipped. `TradingTerminal` is the eighth
  `matching.io/v1` CRD, with model/catalog/CRD plumbing, a renderer emitting the nginx Deployment,
  Service, Ingress, PDB, HPA and both NetworkPolicies (inline `DNS_UPSTREAM` resolver, pinned md/desk
  upstreams, `maxUnavailable:0`), operator observe/status/prune wiring, backup/reconcile/CLI support,
  manager create/list/detail/manifests/upgrade endpoints with an image picker, validation, and a
  fourth manager view linking to the WebUI. The browser still reaches order entry only through the
  order-system BFF and market data only through the market-data service — never engine `/write`.
  `deploy/terminal.yaml` is now a legacy fallback and the production overlay no longer includes it.
  Control-plane suite: 245 lib + 7 bin + 8 integration tests, fmt and clippy pass.
- **2026-11-27** — P1 Raft edge cases E06–E12 from `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` implemented.
  `test_min_voters.rs` rejects a live shrink below four voters while writes keep committing;
  `test_leadership.rs` transfers leadership through both the console action and `Raft::trigger()`
  under continuous write-batch load; `test_read_policy.rs` drives the real `ReadIndex` fallback from
  `local_lease` with a debug clock-skew hook; `test_snapshot.rs` covers a follower's pre-install event
  cursor contract and snapshot-stream cancellation with retry; `test_concurrent.rs` proves 800 keyed
  writes across a leader kill produce contiguous unique sequences with no duplicates and identical
  tapes; `test_failure.rs` injects an fsync failure at the commit marker and verifies no false ack
  plus a `/ready` gate until recovery. All hooks are debug-only/inert by default. Engine suite: 236
  lib + 98 cluster tests, fmt and clippy pass. E13–E20 remain open.

Completed work is summarised here; detailed evidence, measurements and implementation notes remain
in git history and the linked docs. For the full history, check out `TODO.md` from an earlier commit
(`git log -- TODO.md`).

- **2026-11-26** — Stop and GTD index refactor (#21) shipped. Parked stops moved from a scanned
  `Vec` to a sequence-keyed `BTreeMap` with a trigger-ordered index and an elected set; election
  keeps admission order via the sequence-ordered elected set, `observe_trade` walks only trailing
  stops, and cancel/amend lookup is by sequence. GTD/Day deadlines moved to expiry-keyed maps
  (`match_engine/src/order_book/indexes.rs`), so purges split off only due entries and no longer
  rescan bids, asks and stops; the legacy purge output order is preserved by sorting the due set.
  Derived indexes are `#[serde(skip)]` and rebuilt after snapshot install; `StateMachineData` and
  snapshot v11 are unchanged. Added five conformance cases (cascade election order, deadline
  advancement past cancelled/parked orders, Day/GTD purge ordering, expiry bookkeeping across
  fills/amends, multi-stop trailing updates) plus a test-only index consistency assertion after
  every randomized operation. Full engine `cargo test`, Clippy and fmt pass. A paired `replay_mix`
  A/B measured ~5x (170 ms → 33.6 ms warm p50); remaining order-book opportunities are in
  `docs/ORDER_BOOK_PERFORMANCE_REVIEW.md`, with measurements in `docs/PERFORMANCE.md`.
- **2026-11-26** — Order-book review findings implemented (measurements in `docs/PERFORMANCE.md`;
  `SymbolConfig` is borrowed instead of cloned on every apply (rest 3.17 → 1.17 allocs/entry and the
  `apply_path --check` ceilings were tightened); `PegIndex` replaces the full-ladder peg discovery
  scan (zero-peg apply 19 µs → ~0.5 µs, 32-peg apply ~16 µs → 4.3 µs) and `peg_target` computes only
  the BBO each peg needs; `observe_trade` reuses a trailing-stop sequence buffer (3.00 → 2.00
  allocs/entry); `depth`/FOK preflight no longer box their iterators. Two informational harness modes
  (`apply_path --peg`, `--trail`) and a peg-index conformance test were added. Full engine tests,
  Clippy, fmt and all three CI bench gates pass. F4b depth aggregates and F6–F8 remain open.
- **2026-11-26** — Apply-path review findings implemented (measurements in `docs/PERFORMANCE.md`;
  Responses are built only when openraft supplies a responder (follower/replay entries now pay no
  sequence string or event copy), batch events are published with one sink call and incremental
  growth, and the market-data frame's event copy happens after the state-machine lock is released.
  `apply_path` rest/cancel/fill/batch-rest allocations fell from 1.17/1.00/2.00/1.19 to
  0.17/0.00/1.00/0.21 per entry and the ceilings were tightened; batch-rest bytes fell from 1,875 to
  1,141 per entry. New `--sink` and `--tap` modes close the sink/tap measurement gaps, and
  `batch_publishes_one_market_frame_per_sub_request` plus
  `configured_symbol_id_mirror_follows_apply_and_restore` pin the frame and symbol-mirror semantics.
  The HTTP leader symbol guard also reads a lock-free mirror instead of taking the state-machine
  lock. Full engine tests and all three CI bench gates pass.
- **2026-11-26** — Apply-path follow-ups: idempotency keys are shared as `Arc<str>` with
  `Arc<SubmissionRecord>` in both dedupe collections (`apply_path --ids` 3.32 → 2.32 allocs/entry),
  and snapshot `to_content()` reserves the order vector once instead of building two temporary
  Vecs. New `apply_path --ids` and `--batch-peg` modes close two more measurement gaps: a
  128-request batch over 32 pegged orders costs 2.21 µs/entry against 0.22 µs/entry without pegs.
  A4 stays open — removing the per-sub-request peg pass changes which response owns the peg events.
- **2026-11-26** — OrbStack validation of the index/apply-path changes. Docker Hub was unreachable
  from the development host, so the engine was cross-compiled for `aarch64-unknown-linux-musl` with
  `cargo zigbuild` and the static binaries were layered onto the cached runtime image
  (`matchengine-raft@sha256:eb561d18…`). The manager upgrade API rolled all four shards
  (`fx-sup-01`, `fx-eu-01`, `fx-eu-02`, `me-btcusdt`) one at a time; every shard returned to Ready
  with 4/4 updated pods and zero restarts. A live smoke test on `fx-sup-01` verified idempotent
  `Place` retry, a 2-request `/write_batch` with continuous ordinals, `/read`, `/events`,
  `/admission/status` and a gap-free market-data feed; terminal and manager still return 200.
  Inside an OrbStack Linux container the cross-compiled test binaries passed 230 lib + 84 cluster
  tests, `replay_mix --check` and `book_replay --check`. `apply_path --check` passes on Linux and
  macOS. A cold-process blank-row allocation flake on Linux CI (1 call / 15 bytes) is now covered
  by a second uncounted warm-up and a fixed 4-call/128-byte harness allowance; the engine rows keep
  their tight ceilings.
- **2026-11-26** — Safe retries for Cancel/Amend and typed leader redirect (#7). `client_request_id`
  on `Cancel` and `Amend` shares the replicated 262,144-record dedupe namespace with
  `Place.client_order_id`, so a retry returns the original sequence and events without a second
  mutation. Follower `/write` and `/write_batch` now return `503` with a typed JSON body carrying
  `leader_id`, `leader_addr` and the forward reason; `MatchEngineClient` follows it through
  `/metrics` discovery, preserving the `NoLeader` contract before initialization. Edge validation
  bounds all three key fields and rejects duplicate keys across a batch. New unit and cluster
  tests cover the keys and the raw follower redirect.
- **2026-11-26** — OrbStack rollout of the request-key/typed-redirect engine and the order desk.
  Engine `matchengine-raft@sha256:b67b54a3…` and order-system `matchengine-order-system:arm64-v7`
  are live; all four shards and the desk are Ready. The roll briefly stalled because the driver
  shell exited mid-loop, rolling several shards at once: one replica per affected Raft group kept a
  WAL behind the leader's commit while the leader's stale replication match index prevented log
  re-replication, so `/ready` (and therefore the operator's one-pod-at-a-time gate) stayed false.
  Recovery used OpenRaft's own snapshot path only: trigger a snapshot on the lagging group's leader
  and the intended next leader, purge both logs to that index, then transfer leadership — the new
  leader's empty progress selects snapshot replication and the lagging node installs it over gRPC.
  No custom snapshot pull was added. Live checks: follower `/write` returns 503 with
  `leader_addr`; a Cancel retried with `client_request_id` returns the same sequence and events.
  Terminal, desk and market-data feed remain healthy with zero gaps.
- **2026-11-26** — Leader-side automatic recovery for a reverted follower log. `src/log_reversion.rs`
  samples each peer's `/console/api/node` log tail every 5 s on the leader; two consecutive samples
  below the leader's replication match index call OpenRaft's one-shot
  `Raft::trigger().allow_next_revert(peer, true)` and a probe heartbeat. OpenRaft then resets that
  peer's progress and repairs it with normal append/snapshot replication — no custom pull endpoint,
  no manual install and no membership change. Opt out with `MATCHENGINE_AUTO_RECOVER_REVERSION=0`.
  Unit tests cover the detection predicate; full engine tests, fmt and clippy pass. Live pod deletion
  and forced restarts on `fx-sup-01` recovered through the normal path without triggering a reset.
- **2026-11-26** — Race hardening for the leader-side reversion watcher. The first version sampled the leader's `matched` before probing peers and compared it to a peer's later view, which is subject to cross-node read skew. It now probes first and re-reads matched, leadership and membership from a single fresh metrics read after the round-trip, re-checks leadership just before the one-shot trigger, and requires two *identical* consecutive observations so a catching-up follower is never treated as reverted. `matched` only moves forward, so a stale leader read is conservative; the action itself is a speculative one-shot hint that OpenRaft consumes only on a real conflict, so a false arm is a no-op and cannot lose data. Four unit tests cover detection and the repeat-count rule; full engine suite, fmt and clippy pass.
- **2026-11-26** — Lab TDengine moved from `emptyDir` to an 8 Gi `local-path` PVC in
  `k8s_control_plane/deploy/tdengine.yaml`. Host disk reached 99%, the node raised DiskPressure and
  evicted the TDengine pod 192 times in 35 s; because its data was on `emptyDir`, every eviction
  wiped `market_bars`, and the already-running market-data services kept logging "Database not
  exist" / "Table does not exist". Recovered by pruning 3.4 GiB of Docker build cache, deleting the
  evicted pods, and restarting each writer so `ensure_schema` recreated the database, 7 super tables
  and 57 per-symbol child tables. The PVC change was verified by deleting the TDengine pod again:
  `bars_1s` stayed at 38 -> 57 (grew, did not reset) and no service logged a missing-table error.
- **2026-11-27** — P0 Raft edge cases from `docs/RAFT_EDGE_CASE_TEST_MATRIX.md` implemented.
  `test_quorum.rs` proves a 3→1 cluster rejects writes and recovers without divergence;
  `test_partition.rs` proves a restarted former leader with a stale log cannot disrupt the healthy
  quorum (PreVote-on-rejoin); `test_log_reversion.rs` drives the `log_reversion::spawn()` watcher end
  to end using test-only env overrides (5 s / 2 samples by default, 50 ms / 1 in the test) and
  verifies only the lagging peer is reset; `test_storage.rs` injects a pure middle WAL frame gap;
  `test_membership.rs` interrupts a membership change and requires convergence to one valid voter
  set. Engine suite: 236 lib + 91 cluster tests, fmt and clippy pass.
- **2026-11-27** — Late-join cluster coverage. `tests/cluster/test_late_join.rs` now reads the
  stopped node's WAL segments directly (`read_wal_file`, extracting `WalRecord::Entry.log_id.index`
  rather than the byte offset) and asserts the index lag against the leader's commit. The new
  `late_started_voter_with_persistent_wal_joins_quorum` case keeps the node in the three-voter
  membership while it is down, restarts the same id on its old snapshot/WAL, verifies its live log
  and applied index close the observed lag, and proves it is a voting quorum member by committing a
  further write with a third voter stopped. Full engine suite (236 lib + 86 cluster), fmt and
  clippy pass.
- **2026-11-27** — Market-data manager controls (#9) completed. New endpoints: `PUT
  /api/v1/market-data-services/{name}` (edit, image/release preserved), `POST .../scale` (replicas
  and HPA bounds), `POST .../rollback` (restores the image/release saved by the last update), and
  read-only `GET,POST .../plan` mirroring the shard preview plan. `MarketDataServiceStatus` now
  records `previousImage`/`previousRelease`, written by every image update and preserved by the
  operator's status reconcile; rollback swaps them so the action is reversible. The GUI adds
  create/edit and scale modals, row edit/scale/rollback buttons and a third "preview plan" tab with
  scenario select. Verified with 229 lib + 9 other control-plane tests, clippy/fmt, inline JS syntax
  check and a local file-backed manager smoke test covering create/edit/scale, plan scenarios,
  upgrade→rollback→rollback and invalid-scale rejection.
- **2026-11-26** — Historical bars P1 completed and validated. `--history-lease-prefix` creates one
  Kubernetes Lease per symbol (`<prefix>-<symbol>`) and gates flushes, `bar_state` writes and REST
  backfill per symbol; `--history-rest-tail` backfills missing closed windows from a Binance-compatible
  `/api/v3/klines` endpoint with a 750 weight/min budget, `Retry-After`/`418` handling, `source=rest`
  rows and open/closed `bar_gaps` brackets, while feed-position jumps record gaps that `/bars`
  surfaces as `X-Bar-Open-Gaps`. Added `history/lease.rs`, `history/rest.rs`, CLI and renderer flags
  (`spec.history.leasePrefix`), a service-account/Role for writer pods, metrics and unit coverage.
  Rolled to OrbStack with fast writers on `matchengine-marketdata:aeron-arm64-v14`, global read-only
  querier on `history-arm64-v9` and operator/manager on `matchengine-k8s-control-plane:operator-v104`
  (superseding the earlier `aeron-arm64-v11`/`history-arm64-v8` foundation roll). The rollout fixed
  three deployment bugs: a service-account token only on writer pods, an unqualified API-server
  egress rule because this CNI evaluates NetworkPolicy after service DNAT, and six fractional digits
  in Lease timestamps for Kubernetes `metav1.Time`. Exit criteria exercised live: pausing `fx-eu-02`
  moved `historical-bars-eurchf` to `me-btcusdt` within one renewal tick and the returned writer
  stayed paused; deleting a `1m` bar and pointing a writer at a mock klines endpoint restored it with
  exact values and `source=2` while closing the gap. The mock and test row were removed; all five
  pods are Ready with zero restarts, eight Leases held, `/md/bars` and `/klines` 200.
- **2026-11-25** — Fan-out and slow-consumer soak shipped: `market_data_system/benches/ws_soak.rs`
  and `scripts/market-data-soak.sh` drive local and deployed phases (steady, churn, snapshot storm,
  slow consumers, reconnect) with per-second service/engine metric sampling. Added queue-depth/peak,
  encode CPU, broadcast-lag and close-reason counters to `/metrics`. The deployed run (32 clients ×
  16 streams, 8 symbols) passed all phases: 0 depth gaps, 0 driver errors, `queue_peak` 444 < 1,024,
  no engine tap drops and no restarts. Design in `docs/MARKET_DATA_SOAK_DESIGN.md`, results in
  `docs/PERFORMANCE.md`.
- **2026-11-25** — Review fix pass and OrbStack rollout: all concrete bugs and performance defects
  from `docs/CODE_REVIEW.md` were fixed and the report reduced to two unresolved documentation/security
  items. Highlights: postcard `StreamEvents` pages with an explicit wire mirror, 1 Hz full-book cache
  in the order system with per-user filtering, binary-search event paging, bounded console-proxy body
  and snapshot streams, zero-interval/overflow guards, terminal event/leverage/closed-bar fixes,
  portable `DNS_UPSTREAM`, single-source bridge ConfigMap, bundle/drill pairing, and TDengine debug/
  monitor/INFO log suppression. All six images were rebuilt and rolled on OrbStack; verification
  passed (`VERIFY_OK`): 4 shards / 8 symbols / 5 market-data services, 43 acceptance-render checks,
  observability checks, market feed across 8 symbols and per-user BFF filtering. All Rust and npm
  components were bumped to `0.0.15`. The global market-data service was switched to a read-only
  history querier (`--history-read-only`) and the per-symbol fast services now own the TDengine bar
  writers, removing duplicate upserts from the terminal-facing replica. All remaining review findings are folded into the single list
  in **Top priorities**.
- **2026-11-24** — Manager packaging and image supply (#2 closed): checked in a Kustomize base
  (`k8s_control_plane/deploy/kustomization.yaml`) with CRDs, RBAC, the scheduled backup path,
  ingress and observability, plus `overlays/lab` (single-node TDengine, NATS, terminal, local
  tags) and `overlays/production` (three-dnode `deploy/tdengine-ha.yaml` with per-dnode PVCs,
  REPLICA 3, PDB and NetworkPolicy, restricted Pod Security and digest-pinned images). The manager
  now has a checked-in Deployment/Service/NetworkPolicy with probes, Pod Security and the `http`
  Service port the Ingress expects. `scripts/install-control-plane.sh` applies either overlay and
  wires the templated backup Secrets; `scripts/publish-images.sh` builds all five images with
  version `0.0.14`, writes `docker save` tarballs for the image-GC failure mode, or pushes to a
  registry and renders the production digest patch. CI validates all three builds. Production
  evidence (real registry, object-storage backup cycle, CSI snapshot drill) remains gated.
- **2026-11-24** — Observability and HPA metrics (Top priorities #3 closed):
  `k8s_control_plane/deploy/metrics-server.yaml` registers `metrics.k8s.io` so the five market-data
  HPAs and the one order-system HPA report CPU and scale (verified by driving the order-system
  Deployment to two replicas at a 1% target and back to the floor at 70%). A small Prometheus
  server scrapes the JSON→text bridge for engine/market-data `/metrics`, the operator's per-pod
  reconcile/rollout counters and seven alert rules; Grafana has a provisioned dashboard and
  Promtail ships container logs to single-node Loki. Screenshots live in
  `docs/grafana-observability.png`, `docs/prometheus-targets.png` and `docs/prometheus-alerts.png`;
  install and verification are in `docs/OBSERVABILITY.md` and `scripts/check-observability.sh`.
  Production HA, Secrets and retention remain in the production gates.
- **2026-11-23** — Terminal history, 5m/30m rollups and replay evidence (Top priorities #18 closed):
  the pure-React terminal seeds 500 bars from TDengine `/md/klines` and was deployed as
  `matchengine-trading-terminal:arm64-v4`. `market_data_system` materialises `5m` bars and rolls
  `30m` from `5m` children with a `1m` fallback, checked field-by-field in unit tests and live via
  `scripts/check-30m-rollup.py` (18/18 traded windows). The open `30m` bar's `x` now comes only
  from the event-time watermark. `scripts/replay-continuous.sh` drove a continuous GBPUSD tape
  through the BFF, and the lab seeder gave each symbol its own price scale. The historical-bar P0
  checklist item is closed here; its remaining phases stay in Top priorities #9 (ranked #8 at the
  time of that entry).

- **2026-11-22** — Market-data lifecycle/topology work: `place_symbol` now attaches a new symbol
  to every non-paused `MarketDataService` in its cell (per-symbol NATS group or shard-scoped
  multicast group), with unit tests for both transports. Migration plans carry a
  `wait-for-market-data-feed` step and the operator blocks cutover completion until every
  declaring service reports the symbol ready. NATS 4222 egress is scoped to the NATS pods in both
  engine and service NetworkPolicies, and market-data Deployments spread across hostname/zone.
  The wildcard subscriber bind means no one-per-group-per-node restriction is needed. Live
  validation on OrbStack: `eurchf` (EUR/CHF, id 8) was created through the manager GUI onto
  supervisor shard `fx-sup-01`, all five cell services picked it up, the fast/global feeds
  reported it ready after the one-shot rollout, and
  `ws://fast-ws-fx-sup-01…/ws/eurchf@bookTicker` delivered a real bookTicker frame from the
  seeded book. The test exposed a latent defect — a NATS service declaring plain `symbols` (one
  implicit group) would have had its existing symbols silently dropped by the attachment — fixed
  by expanding the implicit group into per-symbol groups first (`operator-v87`). Remaining
  production gates: multi-node NATS + credentials/NKey + JetStream, and multi-node Aeron validation.
- **2026-11-21** — Added [zzxscodes/pitlane](https://github.com/zzxscodes/pitlane) (`ece9c8a`,
  MIT, single commit) to `docs/THIRD_PARTY_INVESTIGATION.md` as Part VI. Verified against source:
  the `FanRing` fail-closed single-producer/64-reader ring, `ReplicaLog` (a capped in-memory quorum
  match-index log with host-driven leadership — not consensus), and `PreTradeGuard` (ordered
  first-failure-wins rule pipeline with typed codes and fail-closed cover). Three findings entered
  the actionable queue: test/live mode isolation and one-permission-bit-per-route in the P1 security
  workstream, an edge/gateway pre-trade rule pipeline as P2, and a generated-docs diff guard in the
  P2 docs-sync item. No code was copied; no latency claims were reproduced.
- **2026-11-20** — Stateless order system (solution G): removed user-range enforcement from the
  BFF (`--user-id-min/--user-id-max` and `DemoApp::require_user` are gone) and collapsed the four
  order-system shards into one Deployment/Service/PDB/HPA. The renderer emits a base-host Ingress
  (`orders.<ingress>`) plus per-shard alias Ingresses, all targeting that Service; the operator
  reports one observation, the manager API/GUI link to the base host, and the stale static
  `orders-index` redirect was removed from `web-ingress.yaml`. Clients now use one order-desk URL
  with plain Ingress load balancing; the public gateway is deferred (decision matrix and full A/F
  design in `docs/GATEWAY_ROUTING.md`).
- **2026-11-20** — Bounded gRPC drain on shutdown: the live roll exposed engine pods stuck in
  `Terminating` past the 60 s grace period. Instrumentation traced it to `grpc_task.await` —
  tonic's graceful shutdown waits for every HTTP/2 connection to close, and peer `AppendStream`
  replication connections stay open on a live cluster. `MatchEngineNode::stop_background` now caps
  the drain at 5 s and aborts the server task on expiry (safe: the node is leaving the cluster and
  peers retry against the remaining voters). A four-voter roll-under-load regression test failed
  before the fix and passes after; live on OrbStack each pod now terminates in ~6 s and the whole
  roll completes in ~1 minute with every order served.
- **2026-11-20** — Raft-native leader discovery and failover: `MatchEngineClient::with_members`
  re-discovers the current leader from `GET /metrics` on transport failure (the cached leader is a
  hint, not a pin); `TargetSpec.engineMembers` and the control-plane renderer wire the full member
  set into order-system `targets.json`. Validated live on OrbStack: a hard leader kill recovered
  after the election with no client restart, and 1,480/1,480 orders succeeded through a full
  one-pod-at-a-time rolling upgrade. The same live roll exposed the graceful-shutdown defect now
  tracked as P1 #2. Design, alternatives and results are in `docs/LEADER_ENDPOINT_ROUTING.md`.
- **2026-11-19** — Backup view in the control plane: the scheduled jobs publish the verified
  result to the `match-backup-status` ConfigMap, the manager serves
  `GET /api/v1/backup-status?shard={name}`, and the shard page gained a **backup** tab showing the
  newest bundle id/age/checksum, the signed drill kind/age/signer, the configured age limits and
  the exact stale reasons (`not configured` until a record exists). The upgrade gate's
  `lastBackupAt` now uses the real published bundle timestamp in CRD mode. A live run against
  OrbStack exercised the whole path: the daily Job created/verified/drilled/signed a 32-object
  `fx-eu-01` bundle, uploaded it, pruned and published; the hourly status Job re-downloaded,
  re-verified the signature and republished; a synthetic old timestamp produced the `stale` state
  with human-readable reasons. `scripts/backup-local-lab.sh` wires both CronJobs to a shared PVC
  (rclone local path) for labs without object storage, and `deploy/rbac.yaml` gained the two
  namespaced Roles for the publish/read path.
- **2026-11-19** — Scheduled backups and signed drills: `deploy/backup.yaml` adds a daily
  `match-backup` CronJob that creates and verifies a bundle, runs `restore --drill`, signs the
  result, uploads the bundle and drill through `rclone`, prunes objects older than
  `BACKUP_RETENTION`, and fails when the newest bundle or drill is stale. An hourly
  `match-backup-status` CronJob repeats the freshness check from object storage. `restore --drill
  --attest-key/--attest-out` writes an HMAC-SHA256 `<bundle-id>.drill.json` + `.drill.sig`, and the
  new `backup-status` command verifies the signature, shard/namespace, bundle checksums and age
  limits. `deploy/backup-alerts.yaml` covers failed and stale jobs; the control-plane runtime image
  now includes `rclone`. The CronJob script was exercised against the live `fx-eu-01` CRDs with a
  local rclone sink: it created a 32-object CRD-only bundle, drilled and signed it, and
  `backup-status` verified the catalog and signature. The manifest defaults to `BACKUP_MODE:
  crd-only` because the local lab has no CSI snapshot driver; the CSI volume drill remains blocked.
- **2026-11-18** — Order-system image inventory and demo replay:
  `GET /api/v1/images?orderSystem={name}` scopes the picker to the system's repository and merges
  node-cached, running and catalog images; the GUI uses an upgrade modal instead of
  `window.prompt`, and the same current-image selection applies to the market-data picker.
  `scripts/replay-order-system.py` translates the checked-in replay fixtures into demo tickets and
  drives the BFF, user-ID shard routing, leader forwarding and the engine together (`--dry-run`,
  `--keep-book`). README/RUNBOOK examples use the OrbStack ingress hosts and the screenshots were
  refreshed.
- **2026-11-18** — Market-data symbol lifecycle: stopping a `SymbolDeployment` removes it from
  every `MarketDataService` that declares it (rendered args, ConfigMap and per-symbol status);
  starting it re-adds the symbol and rolls the service, and a service with no active symbols scales
  to zero and drops its HPA.
- **2026-11-18** — New-symbol rollout fix and NATS acceptance: adding a symbol to an existing
  containers-mode shard no longer deadlocks the operator. A new Raft group has no leader or ready
  replica until every pod carries its container, so the rollout now treats a zero-ready/no-leader
  deployment as unformed, continues while its new containers are running, and skips its quorum
  gate until it has something to protect. A crash-looping new container still blocks the roll.
  `eurusd-g1` was re-enabled and rolled onto all four `fx-eu-01` pods; `eurusd` and `gbpusd` are
  both Ready and the shard rollout is complete. The acceptance harness gained a `nats` phase
  (containers+nats passed in 248s) and the acceptance engine image default moved to
  `matchengine-raft:k8s-nats-arm64-v1`.
- **2026-11-18** — Profile N direct NATS market-data transport: the engine has a `NatsSink`
  (`market-data-nats` feature, `--market-data-nats-url`/`--market-data-nats-subject-prefix`/
  `--market-data-nats-stream-base`), `market_data_system` has a `NatsSource` (`--nats-url`,
  repeatable `--nats-subject-prefix`/`--nats-stream-base`), and the control plane renders
  `transport.mode: nats` with a canonical `md.<cell>.<feed-group>` subject shared by every service
  that covers a symbol. Snapshot requests use `<prefix>.snapshot.request`; Core NATS plus reset
  snapshots is the lab contract. `k8s_control_plane/deploy/nats.yaml` runs a single-node NATS Core
  server. The local engine/service NATS round-trip and the OrbStack deployment are validated; TLS,
  auth, multi-node NATS and JetStream retention remain production follow-ups.
- **2026-11-17** — Market-data feed hardening and supervisor child restart: the source requests a
  snapshot at startup and retries until `snapshot_seen`; each symbol owns an embedded media driver
  and restarts its driver/client after an idle period; the supervisor now restarts a child when
  only its `marketData` configuration changes (new integration test). The market-data images v5/v6
  were rolled and reverted: on the single-node lab the fast and global subscribers still flip the
  Aeron image, so the services are back on v4 and the fast `fx-sup-01` row can still show waiting
  until a relay/dual-publish or one-subscriber-per-group placement is implemented. nzdusd was
  re-added to both market-data service specs and uses its own group `239.1.2.69:24032`.
- **2026-11-17** — Market-data WebUI links and gateway reachability: the market-data tables
  now link to `spec.websocket.ingress` (for example
  `http://fast-ws-fx-eu-01.match-eu-1.example/`) instead of `http://127.0.0.1:<port>`, and use
  `ws://`/`wss://` consistently. The front door labels the `ingress-nginx` namespace
  `matching.io/role=gateway` so the rendered market-data and engine NetworkPolicies admit the
  controller, and the demo terminal service keeps `websocket.tlsSecret` empty because the front
  door publishes HTTP only. Manager `operator-v80`.
- **2026-11-17** — Control-plane, engine-console and order-selector domains:
  `k8s_control_plane/deploy/web-ingress.yaml` publishes `console.match-eu-1.example` (manager GUI,
  API and per-pod engine-console proxy), `engine.match-eu-1.example` (stable engine-console origin;
  the root redirects to `console.match-eu-1.example/?view=engine`) and
  `orders.match-eu-1.example` (order-system selector; the root redirects to
  `?view=ordersystem`), while each order shard keeps its `users-<n>.orders.match-eu-1.example`
  WebUI. The GUI accepts `?view=engine|ordersystem|marketdata-terminal|marketdata-fast`, so the
  redirects open the intended view. Manager and operator are on `operator-v79`.
- **2026-11-17** — Repeatable operator acceptance harness:
  `scripts/operator-acceptance.sh` drives `run`/`render`/`cleanup` against the live OrbStack fleet
  and asserts rendered templates, readiness, order write/read, explicit leadership transfer,
  digest-pinned image roll and rollback (one-pod-at-a-time replacements plus manager rollout
  percentages), stop/start PVC and book continuity, supervisor zero-impact add/stop/start PID
  continuity, reserved create/drop, cross-shard migration with a non-overlapping `raftIdOffset` and
  rollback, and brownfield adoption without recreating the StatefulSet, PVCs or pods. `render` also
  checks the order-system Deployments/HPAs (one-replica floor, 70% CPU target, `maxUnavailable: 0`
  one-surge rollout and no Deployment `spec.replicas`). The full eight-phase run passed in 889s on
  `operator-v78`; `cleanup` also removes retained acceptance PVCs after the catalog record is gone.
  The harness found and fixed two rollout defects: terminating pods still counted as healthy, so the
  operator could delete a second voter before the first replacement was ready, and the
  `currentRevision == updateRevision` fast path could skip a rollback after an `OnDelete` template
  change. The operator now serializes deletions on the in-flight pod and its Ready replacement, and
  decides from pod revisions/images rather than the status fast path. The control-plane Dockerfile
  fetches dependencies in a cached layer.

- **2026-11-17** — Local ingress path for the demo hosts: `k8s_control_plane/deploy/ingress-nginx.yaml`
  installs one class-less ingress-nginx controller so the rendered order-system and market-data
  Ingresses get an address, and `scripts/ingress-frontdoor.sh` publishes its ClusterIP on the Mac's
  port 80 through an `nginx:alpine` Docker front door (macOS needs root to bind 80; NodePorts are not
  host-reachable). The four **Demo order desk** links, shard health and market-data hosts now load
  directly; the controller is plaintext demo plumbing, not a production gateway.
- **2026-11-17** — Executable shard backup and restore: `backup` captures a `VolumeGroupSnapshot`
  or per-PVC `VolumeSnapshot`s, writes `catalog.json`/`backup.json`/`SHA256SUMS` plus an optional
  etcd snapshot, verifies by restoring and hashing every file, and `restore --drill` repeats the
  check. `restore --apply-crds` performs dependency-aware selective CRD merge without deleting
  unrelated objects; `--restore-volumes` creates prefixed PVCs and verifies checksums. Adds the
  `match-backup` RBAC and runbook.
- **2026-11-17** — Order-system multi-target/user sharding: one arm64 image, `--targets` JSON for
  every engine/symbol, user-ID ranges, `OrderSystem` CRD/renderer/operator/status, four live
  `users-*` shards in Kubernetes, control-plane **order systems** tab, and verified cross-symbol
  orders plus cross-shard rejection and owner checks.
- **2026-11-17** — Order-system HPA and rolling updates: per-shard `autoscaling/v2` HPA with a
  one-replica floor, Deployment replica count left to the HPA, paused scale-to-zero,
  `POST /api/v1/order-systems/{name}/upgrade` with a GUI **upgrade** action, `maxUnavailable: 0`
  one-surge shard rolls, per-shard links to the order-system WebUI (order entry stays in the order
  system; the manager no longer proxies orders), and an architecture diagram that includes the
  order-system tier.
- **2026-11-17** — Native arm64 engine and market-data images; four fast-only shard services plus
  a global terminal service spanning every shard feed; per-feed-group channels and wildcard
  market-data receiver binds; one **market data** GUI view with **terminal** and **fast** sub-tabs;
  stateless `POST /api/v1/market-data-services/{name}/upgrade` with source-shard wire-version
  validation and `GET /api/v1/images?marketDataService={name}`; 5-30 s Aeron re-subscribe watchdog;
  control-plane `operator-v61`, market-data `aeron-arm64-v4`, all services Ready.
- **2026-11-16** — Cluster image inventory and upgrade picker (`GET /api/v1/images`), read-only
  nodes/pods RBAC, simplified **upgrade** tab and separate read-only **preview plan** tab,
  `checked_div` rollout percentage, control-plane CI/MSRV checks, `operator-v35` deployment, and
  the Apex-Matching-Engine review in `docs/THIRD_PARTY_INVESTIGATION.md`.
- **2026-11-14/15** — CRD-backed manager and Lease-elected operator; `match-eu-1` adopted with
  digest-pinned images; containers/supervisor/reserved shards; bootstrap, symbol config and
  membership; one-pod-at-a-time rollouts with leader transfer and per-child quorum; release and
  capacity gates; PVC retention; cross-shard cutover/rollback; live GUI and operational console.
- **2026-11-09/10** — Schema-v11 order types (market-to-limit, trailing, peg, STP), snapshot v11 and
  wire v7; OpenRaft 0.10.0 stable; benchmark regression gate; public conformance/property suites;
  event-cursor and restart coverage.
- **2026-11-06/08** — Dedicated Raft runtime (`RAFT_CORE_WORKERS`), WAL fsync modes and byte-bounded
  appends, async-WAL prototype, local-lease reads as default, write admission control, transport
  metrics, production-configuration decision record.
- **2026-10-29/11-04** — Exact 18-scale `Fixed` migration end-to-end; segmented WAL and snapshot
  storage; gRPC-only Raft transport with PreVote; replicated event log, client dedupe and market-data
  feed/Aeron egress; fee/asset model and third-party review.
- **Earlier 2026** — Order book and matching lifecycle, conformance and randomized tests, fee and
  asset semantics, benchmark baselines, demos (order system, market-data service, Kubernetes control
  plane) and documentation.

Design decisions held: one 18-decimal `Fixed` scale; one symbol per process; plaintext internal
HTTP/gRPC with TLS and authentication at the gateway; event order by `(log_index, ordinal)` only;
no arena allocator on the apply path.
