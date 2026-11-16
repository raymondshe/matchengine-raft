# TODO

Last reviewed: 2026-11-16. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

**Current baseline.** The engine runs OpenRaft 0.10.0 with a segmented WAL, snapshot v11 and exact
18-scale `Fixed` values. It supports the full order-type set (GTC/IOC/FOK/GTD/Day, market,
post-only, iceberg, stop, market-to-limit, trailing and peg) plus STP, maker/taker fees with
asset-aware policies, a replicated event log (`/events`, gRPC streaming), local-lease reads,
bounded write admission and the market-data feed with Aeron egress. One process/cluster trades one
symbol; multi-symbol isolation is deployment-level.

**Control plane.** The CRD-backed manager and Lease-elected operator manage containers, supervisor,
reserved and migrated shards. The operator bootstraps Raft, configures symbols, rolls one pod at a
time with leader transfer and per-process quorum checks, blocks incompatible releases and
over-capacity renders, retains PVCs and performs cross-shard cutover/rollback. The manager GUI has
live refresh, release/capacity banners, a cluster image picker and a read-only preview plan. The
`match-eu-1` reference fleet runs digest-pinned images. There are no known P0 defects.

Priority key: **P0** = known correctness/security defect, do next; **P1** = production safety and
operability; **P2** = evidence, scale and important non-blocking work; **P3** = later/optional.

## Top priorities

Last prioritised 2026-11-16, after the cluster image inventory, the split upgrade/preview-plan tabs
and the operator-v35 rollout. The local-lease semantic risk stays covered by the delivery contract:
client retries are at-least-once and use `client_order_id` for replicated dedupe, while downstream
event readers resume from `(log_index, ordinal)` and dedupe on `event_id`. A local-lease cache hit
may overtake an unfinished write, so read-after-write is not a guarantee; clients that need it must
await the write response.

**P1 - production safety and operability, in execution order**

1. **Backup and restore execution.** Run `VolumeSnapshot`/`VolumeGroupSnapshot` for engine PVCs plus
   CRD/etcd backup and selective restore, with checksums and a restore drill. The demo plans
   backups but has never restored one: this is the largest data-loss exposure.
2. **Manager identity, authorization and approvals.** One auth model covering OIDC/RBAC,
   console-proxy authorization, destructive-action approvals, namespace-qualified paths,
   idempotency keys, pagination and dry-run diffs, plus NetworkPolicy tests and Pod Security
   Standards. The demo manager currently has no auth.
3. **Repeatable operator acceptance harness.** Script the containers/supervisor/reserved/migration/
   adoption/digest-pinning/stop-start/rollback flows with assertions for the rendered template,
   rollout percentage, leader transfer, readiness and data continuity.
4. **Manager deployment packaging.** Check in Deployment/Service/RBAC/NetworkPolicy/Pod Security,
   Helm/Kustomize, digest-pinned image publication and CRD/etcd backup integration. The live demo
   still depends on a local image, an `emptyDir` and a port-forward.
5. **Observability.** Add a Prometheus text bridge for the engine's JSON `/metrics`, operator
   reconcile/rollout metrics, alert rules, dashboards and log shipping. JSON metrics are not
   scrapable today.
6. **Gateway integration.** Route from `SymbolRoute.status` with shadow/cutover/drain, retries and a
   canary path, then run a tested client cutover. No gateway is deployed in the demo.

**P2 - evidence and scale, in execution order**

7. **Multi-host evidence.** Rerun the read/write A/B on a second host and a Linux multi-host
   four-voter cluster, force a stale lease so the `ReadIndex` fallback is observed, validate Aeron
   multicast across hosts, and dispatch the bare-metal pinned-core `e2e` workflow.
8. **Linux supervisor isolation.** Validate opt-in cgroup v2 delegation and rotating logs on a node
   that delegates a writable subtree; OrbStack cannot, so the supervisor reports `isolationError`
   and fails open.
9. **Stop-the-world incompatible upgrade execution.** The release gate blocks mixed rolls; add the
   coordinated stop/upgrade/restart path so an incompatible release is not a dead end.
10. **CRD scale and soak.** Thousands of symbols/CRs across shards and cells, status churn,
    API-server/etcd size, informer memory and operator workqueue latency.
11. **Docs synchronization.** Keep `AGENTS.md`, READMEs, `docs/PERFORMANCE.md`, `docs/API.md`,
    `docs/ETCD_STORAGE.md` and the runbooks moving with storage, wire, read-policy and control-plane
    changes.

**P3 - later, in impact order**

1. **GUI follow-ups:** migration/promote forms, approvals, filters/pagination, diff/rollback and
   SSE/WebSocket updates.
2. **Automation surface:** CLI `create-shard`/`create-symbol`/`migrate`, an OpenAPI client and
   admission validation/defaulting beyond the CRD YAML.
3. **Multi-cell federation:** cell registration/capacity, CR projection and cross-cell migration.
4. **Engine optional work:** bounded-admission backpressure, snapshot-policy purge knobs,
   `STREAM_APPEND_CHANNEL_SIZE` benchmark, leader-change callbacks, peer preconnect/larger
   frames/snapshot compression, async-WAL productionization, Aeron Raft transport, binary order
   entry and ITCH-style replay, derivative fields, reserve replenishment/collars and
   protobuf/Avro, module renaming and AON/order-mask coverage.
5. **Operator documentation:** metrics, troubleshooting, capacity planning and disaster-recovery
   runbooks.

## Open work

### Control plane

- [ ] **P1** Backup/restore execution: `VolumeGroupSnapshot`/`VolumeSnapshot` for engine PVCs, plus
  etcd/CRD backup and selective restore; a backup agent, checksums and a restore test.
- [ ] **P1** Production manager: OIDC/RBAC, approvals, idempotency keys, namespace-qualified paths,
  pagination and dry-run diffs. Desired state stays in CRDs; use Postgres/object storage only for
  audit, a query/read model or long-term history, not as a second source of truth.
- [ ] **P1** Manager deployment packaging: checked-in Deployment/Service/RBAC/Pod Security/
  NetworkPolicy, Helm/Kustomize, image digest pinning/publishing and etcd/CRD backup integration.
  No PVC is needed for desired state; an optional volume is only for audit/export caches.
- [ ] **P1** Gateway integration: leader-aware routing from `SymbolRoute.status`, shadow/cutover/
  drain, retries and a canary path. No gateway is deployed in the demo.
- [ ] **P1** Observability: a Prometheus text bridge for the engine's JSON `/metrics`, alert rules,
  dashboards and log shipping; the console's peer view is covered by the NetworkPolicy rule.
- [ ] **P1** Security: manager OIDC and console-proxy authorization, approvals for destructive
  actions, NetworkPolicy tests and Pod Security Standards. The demo manager has no auth.
- [ ] **P1** End-to-end operator smoke script/runbook: containers, supervisor, reserved, migration,
  brownfield adoption, digest pinning, stop/start and rollback flows are live-validated; make them
  repeatable with assertions for the rendered template, rollout percentage, leader transfer,
  readiness and data continuity.
- [ ] **P2** Stop-the-world incompatible upgrade execution: the release gate blocks mixed rolls and
  supervisor rolling is implemented; executing the coordinated stop-the-world cutover remains.
- [ ] **P2** Scale and soak the CRD path: thousands of symbols/CRs across shards and cells, status
  churn, API-server/etcd size and backup/restore, manager informer memory and operator workqueue
  latency. No real engine load is required.
- [ ] **P2** Validate opt-in supervisor cgroups and rotating logs on a Linux node that delegates a
  writable cgroup v2 subtree; OrbStack does not, so the supervisor reports `isolationError` and
  fails open.
- [ ] **P3** GUI: the overview is live, the upgrade tab is a single action and the preview plan tab
  is a read-only simulator. Add migration/promote forms, approvals, filters/pagination,
  diff/rollback and SSE/WebSocket updates.
- [ ] **P3** CLI `create-shard`, `create-symbol` and `migrate` commands for automation; the API/GUI
  are implemented today.
- [ ] **P3** OpenAPI client, admission validation/defaulting and schema tests beyond the CRD YAML
  delivered with the store.
- [ ] **P3** Multi-cell federation: cell registration/capacity, CR projection and cross-cell
  migration. The demo has one cell.
- [ ] **P3** Documentation: operator metrics, troubleshooting, capacity planning and disaster
  recovery; keep the design, etcd map and local test plan aligned as the operator lands.

### Engine and transport

- [ ] **P2** Validate the default `local_lease` path on a second host and a Linux multi-host
  four-voter cluster: rerun `read_e2e --policy lease|local_lease` plus the e2e/cluster write A/B,
  force a stale lease or cache miss so the `ReadIndex` fallback is observed, and check
  `/read-policy/status` counters under bounded clock skew. The OrbStack four-container lab proves
  the Linux path but is still one host.
- [ ] **P2** Validate Aeron multicast across a real multi-host fabric: separate hosts/drivers on a
  non-loopback NIC, independent service instances and no shared `/dev/shm`. The single-host `lo`
  path is covered by `scripts/aeron-linux-lab.sh`.
- [ ] **P2** Dispatch `.github/workflows/bench-linux.yml` for the bare-metal pinned short and
  sustained `e2e` rows. The OrbStack container run covers the short pinned check in both fsync
  modes; this is the remaining bare-metal evidence.
- [ ] **P3** Productionize async WAL only after the second-host Linux multi-node A/B and pipelining
  work: add periodic-mode support if useful, bound the pending cache with backpressure, extend
  crash/failover coverage and test whether more append concurrency exposes a real gain. Keep the
  synchronous WAL as the default until then.
- [ ] **P3** When exposing the snapshot policy, expose `purge_batch_size` and
  `max_in_snapshot_log_to_keep` together; a lower policy would otherwise cause frequent small
  purges.
- [ ] **P3** Benchmark `STREAM_APPEND_CHANNEL_SIZE` separately; it is a compile-time transport
  constant, so the runtime sweep does not cover it.
- [ ] **P3** Replace metrics polling in the market-data leader watcher with OpenRaft's
  `on_leader_change` callbacks.
- [ ] **P3** Optional transport work: preconnect peers after startup/membership changes, larger
  HTTP/2 frames and snapshot compression. Persistent `AppendStream` sessions across OpenRaft
  replication sessions need an upstream contract change; heartbeat suppression is the practical
  way to reduce stream churn.
- [ ] **P3** Evaluate bounded waiting/backpressure as an alternative to immediate `429` when clients
  cannot retry promptly; keep the current fail-fast gate as the default until measured.
- [ ] **P3** The Aeron Raft transport remains gated on a measured bottleneck; the market-data Aeron
  egress is implemented.
- [ ] **P3** Add custom reserve replenishment and stop-protection collars only if the product
  requires them; the current iceberg display tranche covers existing reserve behaviour.
- [ ] **P3** Consider protobuf/Avro on the public HTTP API. Raft RPCs already use postcard over
  gRPC.
- [ ] **P3** Evaluate a binary order-entry path (gRPC/protobuf or compact codec) and feature-gated
  latency histograms only if a latency target requires them.
- [ ] **P3** Evaluate real ITCH-style replay data and custom MPMC/SIMD/io_uring work only after a
  measured bottleneck and a Linux-only deployment decision. `wire-mpmc` is not async-aware: use
  Tokio `mpsc` for task-to-task communication and keep spin-based queues off Tokio workers.
- [ ] **P3** Engine-level derivative support (instrument kind, leverage/margin fields, contract
  multiplier, funding/PnL/liquidation) is out of scope for the cash/spot engine. The demo perpetual
  overlay is a UI/economics estimate only.
- [ ] **P3** Add AON and order-mask conformance cases when those behaviours land. STP already has
  `SelfTradePrevented` conformance and integration coverage; multi-symbol remains deployment-level
  isolation.

### Docs and hygiene

- [ ] **P2** Keep `AGENTS.md`, READMEs, `docs/DECIMAL.md` and this file in sync after future
  storage/wire changes.
- [ ] **P3** Decide whether to rename the remaining mixed-style modules before 1.0
  (`src/match_result/` → `match_events/`). This is import-breaking and offers no functional benefit
  today.

## History

Completed work is summarised here; detailed evidence, measurements and implementation notes remain
in git history and the linked docs. For the full history, check out `TODO.md` from an earlier commit
(`git log -- TODO.md`).

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
