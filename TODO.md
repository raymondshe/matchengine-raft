# TODO

Last reviewed: 2026-11-17. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

**Current baseline.** The engine runs OpenRaft 0.10.0 with a segmented WAL, snapshot v11 and exact
18-scale `Fixed` values. It supports the full order-type set (GTC/IOC/FOK/GTD/Day, market,
post-only, iceberg, stop, market-to-limit, trailing and peg) plus STP, maker/taker fees with
asset-aware policies, a replicated event log (`/events`, gRPC streaming), local-lease reads,
bounded write admission and the market-data feed with Aeron egress. One process/cluster trades one
symbol; multi-symbol isolation is deployment-level. The reference fleet also runs four fast-only
shard market-data services and one global terminal service that consumes every shard feed and
serves one multi-symbol WebSocket.

**Control plane.** The CRD-backed manager and Lease-elected operator manage containers, supervisor,
reserved and migrated shards. The operator bootstraps Raft, configures symbols, rolls one pod at a
time with leader transfer and per-process quorum checks, blocks incompatible releases and
over-capacity renders, retains PVCs and performs cross-shard cutover/rollback. The manager GUI has
live refresh, release/capacity banners, a cluster image picker and a read-only preview plan. The
market-data tier lives under one **market data** top-level view with **terminal** and **fast**
sub-tabs, and images roll through `POST /api/v1/market-data-services/{name}/upgrade` with a
source-shard wire-version check. The `match-eu-1` reference fleet runs digest-pinned images. There are no known P0 defects.

Priority key: **P0** = known correctness/security defect, do next; **P1** = production safety and
operability; **P2** = evidence, scale and important non-blocking work; **P3** = later/optional.

## Top priorities

Last prioritised 2026-11-17, after the native arm64 market-data rollout, the terminal/fast tab
split and the stateless market-data image upgrade. The local-lease semantic risk stays covered by
the delivery contract:
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
5. **Observability and HPA metrics.** Add a Prometheus text bridge for the engine's JSON
   `/metrics`, operator reconcile/rollout metrics, alert rules, dashboards and log shipping. Install
   a metrics-server or Prometheus adapter so the market-data HPAs can report CPU and actually
   scale; today the targets show `<unknown>` and only the min-1 floor is enforced.
6. **Gateway and public-edge integration.** Route from `SymbolRoute.status` with shadow/cutover/
   drain, retries and a canary path, then run a tested client cutover. Terminate TLS for the
   terminal market-data host, authenticate and rate-limit WebSocket clients, keep Aeron internal,
   and document the gateway/Ingress boundary. No gateway is deployed in the demo.
7. **Market-data production readiness.** Validate Profile A on a multi-node multicast-capable
   CNI/VPC (Cilium/Isovalent or Multus plus AWS Transit Gateway); the default EKS VPC CNI cannot run
   it. Define the topology rule for multiple replicas/subscribers on one node: two pods joining the
   same group on the single-node lab can transiently starve one feed, so prefer one subscriber per
   group per node or add dual-publish/relay support. Couple symbol create/migrate/stop to both the
   fast and global symbol sets, wait for the new feed before publishing, and drain old WebSocket
   sessions. Add ServiceMonitor, canary and tested rollback around the stateless image update.

**P2 - evidence and scale, in execution order**

8. **Market-data manager controls.** Add create/edit/scale and a read-only preview plan for
   `MarketDataService` in the market-data view; image/release update and the per-service picker
   are implemented. Add a rollback button that restores the previous image.
9. **Market-data fan-out and slow-consumer evidence.** Soak many symbols, streams, connections and
   slow consumers through the Kubernetes deployment; measure per-instance encode/fan-out CPU, queue
   depth, gap/resync behavior and p99, and prove the engine's matching path is unaffected.
10. **Market-data feed hardening.** Characterize the Aeron image-registration race and the
    single-node multi-subscriber behavior. Make source startup readiness-gated (or add a
    per-source generation) so a stale feed is detected and rebuilt without log spam, and keep the
    5-30 s re-subscribe watchdog as the fallback. Avoid restart loops when an engine feed is
    genuinely down.
11. **Multi-host evidence.** Rerun the read/write A/B on a second host and a Linux multi-host
    four-voter cluster, force a stale lease so the `ReadIndex` fallback is observed, validate Aeron
    multicast across hosts, and dispatch the bare-metal pinned-core `e2e` workflow.
12. **Linux supervisor isolation.** Validate opt-in cgroup v2 delegation and rotating logs on a node
    that delegates a writable subtree; OrbStack cannot, so the supervisor reports `isolationError`
    and fails open.
13. **Stop-the-world incompatible upgrade execution.** The release gate blocks mixed rolls; add the
    coordinated stop/upgrade/restart path so an incompatible release is not a dead end.
14. **CRD scale and soak.** Thousands of symbols/CRs across shards and cells, status churn,
    API-server/etcd size, informer memory and operator workqueue latency.
15. **Docs synchronization.** Keep `AGENTS.md`, READMEs, `docs/PERFORMANCE.md`, `docs/API.md`,
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
   entry and ITCH-style replay, binary/compressed WebSocket market-data payloads only after a
   measured JSON fan-out bottleneck, derivative fields, reserve replenishment/collars and
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
  dashboards and log shipping, plus a metrics-server/Prometheus adapter so the market-data HPAs can
  report CPU and scale; the console's peer view is covered by the NetworkPolicy rule.
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
- [ ] **P2** Order-system user-routing edge: map user IDs to their shard range behind one host so clients do not choose a shard URL manually, and add auth/rate limits per user. The shards themselves and per-shard APIs are live.
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

### Market data and Kubernetes

The `MarketDataService` CRD, renderer, operator lifecycle, fast/terminal profiles, four fast-only
shard services and the global terminal service are live; see History. Remaining work:

- [ ] **P1** Validate Profile A across a multi-node multicast-capable CNI/VPC (Cilium/Isovalent
  multicast, or Multus plus AWS Transit Gateway multicast). The default EKS VPC CNI cannot run it.
  Record CNI/MTU, cross-node `/ready`, leader-transfer reset behavior, and benchmark direct NATS
  (`NatsSink` -> NATS Core/JetStream -> `NatsSource`) as the fallback when multicast is unavailable
  or misses the p99 target.
- [ ] **P1** Market-data topology and symbol lifecycle coupling. Define a one-subscriber-per-group
  per-node placement rule (or add dual-publish/relay support): two pods joining the same group on
  the single-node lab can transiently starve one feed. Creating, migrating or stopping a symbol
  must update both the fast and global symbol sets and `SymbolRoute` in one guarded flow, wait for
  the new feed to be ready before publishing traffic, and drain the old WebSocket sessions.
- [ ] **P1** Public market-data edge: ingress TLS/authentication, rate limits, origin checks,
  connection/stream caps, NetworkPolicy and no public Aeron exposure; document the
  gateway/Ingress/manager boundary as part of the security review.
- [ ] **P2** Optional Aeron profiles only if the NATS benchmark misses the latency target: direct
  multicast where the multi-node CNI/VPC test passes, or the per-pod IPC bridge for existing Aeron
  deployments. Keep unique stream bases per engine process (`2001 + 7 * slot`), a cell-unique
  `symbol_id`, distinct per-process Aeron directories and no shared `/dev/shm`.
- [ ] **P2** Market-data profile extras: all-market aggregate streams (`!miniTicker@arr`), a
  per-connection terminal frame budget, per-profile connection/lag metrics, explicit WebSocket close
  reasons and a ServiceMonitor.
- [ ] **P2** Manager controls for the market-data tier: create/edit/scale and a read-only preview
  plan in the market-data view; image/release update and the per-service image picker are
  implemented. Add a rollback action that restores the previous image.
- [ ] **P2** Rollout and availability: canary policy, PDB verification, drain-aware image upgrades
  and a tested rollback path that restores subscriber capacity without touching matching. HPA CPU
  metrics need a metrics server or Prometheus adapter; this is tracked as a P1 in Top priorities.
- [ ] **P2** Aeron feed hardening: characterize the image-registration race and the single-node
  multi-subscriber behavior; make source startup readiness-gated (or add a per-source generation)
  so a stale feed is detected and rebuilt without log spam. Keep the 5-30 s re-subscribe watchdog as
  the fallback and avoid restart loops when an engine feed is genuinely down.
- [ ] **P2** Observability: a Prometheus text endpoint (or bridge for the JSON metrics), per-symbol
  readiness and last-frame age, feed lag, `GAP`/drop counts, queue depth, subscribers per stream,
  conflation ratio, encode CPU and WebSocket close reasons; alerts and the manager status view.
- [ ] **P2** Fan-out and slow-consumer soak: many symbols, 1,024-stream connections, mixed
  `SUBSCRIBE`/`UNSUBSCRIBE`, snapshot-request storms, slow clients and reconnect bursts. Measure
  per-instance encode/fan-out CPU, queue depth, resync time and p99, and prove the engine's
  `apply`/Raft path is unaffected. Do not repeat the shallow-benchmark claims reviewed in
  `docs/THIRD_PARTY_INVESTIGATION.md`.
- [ ] **P3** Optional archive and replay: persist feed archives to object storage or a PVC with
  retention/checksums and replay through the same assembler for incident analysis; archives are
  data, not desired state, and must never go into etcd.
- [ ] **P3** Evaluate binary or compressed WebSocket payloads only after profiling shows JSON
  encoding/fan-out is the bottleneck; the third-party reviews are a caution against claiming
  unmeasured wire-format gains.

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

- **2026-11-17** — Order-system multi-target/user sharding: one arm64 image, `--targets` JSON for
  every engine/symbol, user-ID ranges, `OrderSystem` CRD/renderer/operator/status, four live
  `users-*` shards in Kubernetes, control-plane **order systems** tab, and verified cross-symbol
  orders plus cross-shard rejection and owner checks.
- **2026-11-17** — Order-system HPA and rolling updates: per-shard `autoscaling/v2` HPA with a
  one-replica floor, Deployment replica count left to the HPA, paused scale-to-zero,
  `POST /api/v1/order-systems/{name}/upgrade` with a GUI **upgrade** action, `maxUnavailable: 0`
  one-surge shard rolls, and per-shard links to the order-system WebUI (order entry stays in the
  order system; the manager no longer proxies orders).
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
