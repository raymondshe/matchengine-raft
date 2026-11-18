# TODO

Last reviewed: 2026-11-18. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

**Current baseline.** The engine runs OpenRaft 0.10.0 with a segmented WAL, snapshot v11 and exact
18-scale `Fixed` values. It supports the full order-type set (GTC/IOC/FOK/GTD/Day, market,
post-only, iceberg, stop, market-to-limit, trailing and peg) plus STP, maker/taker fees with
asset-aware policies, a replicated event log (`/events`, gRPC streaming), local-lease reads,
bounded write admission and the market-data feed with Aeron multicast or direct NATS egress. One process/cluster trades one
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
source-shard wire-version check. The `OrderSystem` CRD renders four user-ID shards, each with its
own Deployment/Service/PDB/Ingress/HPA (min 1) and `maxUnavailable:0` image rolls; the manager links
each shard to its own order desk instead of proxying order entry. The `match-eu-1` reference fleet
runs digest-pinned images. The live acceptance harness (`scripts/operator-acceptance.sh`) covers
containers, supervisor, reserved, migration, adoption, digest-pinned roll/rollback and stop/start
flows; the full eight-phase OrbStack run passes in about 15 minutes. There are no known P0 defects.

Priority key: **P0** = known correctness/security defect, do next; **P1** = production safety and
operability; **P2** = evidence, scale and important non-blocking work; **P3** = later/optional.

## Top priorities

Last prioritised 2026-11-17, after the operator acceptance harness passed all eight phases and
found two rollout defects that are now fixed. Ordering weighs data loss, security exposure and
production blockers first, then work that is actionable in the current single-node lab;
environment-blocked validation is called out explicitly rather than mixed into the actionable
queue.

The local-lease semantic risk stays covered by the delivery contract: client retries are
at-least-once and use `client_order_id` for replicated dedupe, while downstream event readers
resume from `(log_index, ordinal)` and dedupe on `event_id`. A local-lease cache hit may overtake
an unfinished write, so read-after-write is not a guarantee; clients that need it must await the
write response.

**P1 - production safety and operability, in execution order**

1. **Backup and restore validation.** Data loss is the highest-consequence risk. The CLI and
   checksummed CRD/etcd bundle path are implemented and tested. Finish the actionable part now:
   run `backup` from a CronJob with `match-backup`, upload bundles to object storage, enforce
   retention, alert when the newest bundle or restore drill is stale, and record a signed
   CRD-bundle drill. The CSI `VolumeSnapshot`/`VolumeGroupSnapshot` drill stays blocked until a
   snapshot-capable cluster is available; keep it an explicit production gate, not a silent gap.
2. **Observability and HPA metrics.** Install a metrics-server or Prometheus adapter so the four
   order-system and five market-data HPAs can report CPU and actually scale, then add the
   Prometheus text bridge for the engine's JSON `/metrics`, operator reconcile/rollout metrics,
   alerts, dashboards and log shipping. This is the largest actionable unblock in the current lab:
   every HPA currently shows `<unknown>` and only the min-1 floor applies, and the same metrics
   are needed before any load or soak test.
3. **Control-plane and public-edge security.** One workstream for manager OIDC/RBAC,
   console-proxy authorization, destructive-action approvals, idempotency keys,
   namespace-qualified paths and dry-run diffs, plus TLS, client authentication, rate limits,
   origin checks and NetworkPolicy/Pod Security for the order-system and market-data edges. The
   manager and both demo edges are still plaintext and unauthenticated.
4. **Gateway routing and user-ID shard routing.** Route from `SymbolRoute.status` with
   shadow/cutover/drain, retries and a canary path, then run a tested client cutover. Add one host
   that maps user IDs to their order-system shard range so clients stop choosing shard URLs
   manually. Migrations and the four order shards already exist; this is the missing production
   data path.
5. **Manager deployment packaging.** Check in Deployment/Service/RBAC/NetworkPolicy/Pod Security,
   Helm/Kustomize, digest-pinned image publication and CRD/etcd backup integration. The demo still
   depends on local-only image tags, an `emptyDir` and a port-forward, which blocks a clean install;
   a `DiskPressure` image GC removed those tags during the 2026-11-18 rollout, so publish to a local
   registry or add an explicit `docker save`/`load` step before the next fleet roll.
6. **Market-data production readiness.** Couple symbol create/migrate/stop to both the fast and
   global symbol sets and the `SymbolRoute`, wait for the new feed before publishing and drain old
   WebSocket sessions. Profile N (direct NATS) is now the single-node fallback and removes the
   Aeron image-registration constraint; harden it for production with a multi-node NATS cluster,
   TLS/credentials, strict NetworkPolicy and optional JetStream retention. Validate Profile A on a
   multi-node multicast-capable CNI/VPC; that half is blocked on this single-node host, but the
   lifecycle and topology code is not.

**Blocked on this host.** CSI volume snapshots (no snapshot CRDs or driver), multi-host
lease/`ReadIndex` and Aeron multicast evidence (one OrbStack node), and the bare-metal pinned-core
`bench-linux` dispatch. The lists below keep those halves visible; do not treat a green single-node
run as production evidence for them.

**P2 - evidence and scale, in execution order**

7. **Acceptance-harness automation and rollout invariants.** The manual harness found two real
   rollout defects. Run `scripts/operator-acceptance.sh` in a disposable CI cluster or nightly,
   and extract the rollout candidate decision into a pure function with unit tests for: at most
   one terminating pod, the replacement must be Ready before the next deletion, and no roll may be
   skipped when `currentRevision == updateRevision` while pods still run the old template.
8. **Stop-the-world incompatible upgrade execution.** The release gate correctly blocks mixed
   rolls, and the runbook has a manual stop/upgrade/restart path. Automate the coordinated cutover
   (backup gate, disable routes, stop all symbols, apply image/release, restart, verify, re-enable)
   and record a drill.
9. **Market-data manager controls.** Add create/edit/scale and a read-only preview plan for
   `MarketDataService`; image/release update and the per-service picker are implemented. Add a
   rollback action that restores the previous image.
10. **Market-data fan-out and slow-consumer evidence.** Soak many symbols, streams, connections
    and slow consumers through the Kubernetes deployment; measure per-instance encode/fan-out
    CPU, queue depth, gap/resync behavior and p99, and prove the engine's matching path is
    unaffected.
11. **Market-data feed hardening.** Source startup requests a snapshot immediately and retries
    until one is seen; the supervisor restarts a child when its `marketData` configuration changes;
    Profile N direct NATS is live on OrbStack; and `scripts/operator-acceptance.sh` now has a `nats`
    phase that proves sink=`nats`, feed readiness, and service-pod re-subscribe/snapshot recovery.
    A live NATS restart also preserved all symbol readiness and re-established all 36 client
    connections. Remaining work: add TLS/auth and JetStream retention, run a multi-node NATS
    cluster, and keep the Aeron 5-30 s watchdog for clusters that still use multicast.
12. **Multi-host evidence.** Rerun the read/write A/B on a second host and a Linux multi-host
    four-voter cluster, force a stale lease so the `ReadIndex` fallback is observed, validate
    Aeron multicast across hosts, and dispatch the bare-metal pinned-core `e2e` workflow.
13. **Linux supervisor isolation.** Validate opt-in cgroup v2 delegation and rotating logs on a
    node that delegates a writable subtree; OrbStack cannot, so the supervisor reports
    `isolationError` and fails open.
14. **CRD scale and soak.** Thousands of symbols/CRs across shards and cells, status churn,
    API-server/etcd size, informer memory and operator workqueue latency.
15. **Docs synchronization.** Keep `AGENTS.md`, READMEs, `docs/PERFORMANCE.md`, `docs/API.md`,
    `docs/ETCD_STORAGE.md` and the runbooks moving with storage, wire, read-policy and
    control-plane changes.

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

- [ ] **P1** Backup/restore validation: the `backup`/`backup-verify`/`restore --drill` CLI is
  implemented in `src/backup.rs` with `VolumeGroupSnapshot`/`VolumeSnapshot`, checksummed CRD/etcd
  bundles, selective `--apply-crds` and prefixed PVC restore. Finish the actionable part: run
  `backup` from a CronJob with the `match-backup` ServiceAccount, upload the bundle to object
  storage, enforce a retention window, alert when the newest bundle or restore drill is stale and
  record a signed CRD-bundle drill. The CSI volume drill is blocked until a snapshot-capable
  cluster is available.
- [ ] **P1** Observability: install a metrics-server/Prometheus adapter so the four order-system and
  five market-data HPAs report CPU and scale, then add a Prometheus text bridge for the engine's
  JSON `/metrics`, operator reconcile/rollout metrics, alert rules, dashboards and log shipping.
  The console peer view is already covered by the NetworkPolicy rule.
- [ ] **P1** Production manager and control-plane security: OIDC/RBAC, console-proxy
  authorization, destructive-action approvals, idempotency keys, namespace-qualified paths,
  pagination, dry-run diffs, NetworkPolicy tests and Pod Security Standards. Desired state stays
  in CRDs; use Postgres/object storage only for audit, a query/read model or long-term history,
  not as a second source of truth. The demo manager has no auth.
- [ ] **P1** Gateway integration: leader-aware routing from `SymbolRoute.status`, shadow/cutover/
  drain, retries and a canary path. No gateway is deployed in the demo.
- [ ] **P1** Manager deployment packaging: checked-in Deployment/Service/RBAC/Pod Security/
  NetworkPolicy, Helm/Kustomize, image digest pinning/publishing and etcd/CRD backup integration.
  No PVC is needed for desired state; an optional volume is only for audit/export caches. The
  console, engine-console proxy and order selector now have checked-in Ingress hosts
  (`deploy/web-ingress.yaml`); the workload and packaging manifests remain.
- [ ] **P1** Order-system public edge: TLS, authentication, rate limits and NetworkPolicy for the
  per-shard WebUI and `/demo/api`; the demo currently exposes plaintext HTTP with no auth. Keep
  order entry in the order system and the manager's role to linking only.
- [ ] **P1** Order-system user-routing edge: map user IDs to their shard range behind one host so
  clients do not choose a shard URL manually, and add per-user auth/rate limits. The four shards,
  per-shard APIs/WebUI links, HPAs and rolling image updates are live; the shared host and per-user
  authorization remain.
- [ ] **P2** Automate the acceptance harness and rollout invariants: run
  `scripts/operator-acceptance.sh` in a disposable CI cluster or nightly, and add pure-function
  tests for the rollout candidate rules (one terminating pod at a time, replacement Ready before
  the next deletion, and no skip when `currentRevision == updateRevision` while pods still run the
  old template). The manual run found the two defects fixed on 2026-11-17.
- [ ] **P2** Stop-the-world incompatible upgrade execution: the release gate correctly blocks
  mixed rolls and supervisor rolling is implemented; automate the coordinated stop/upgrade/
  restart cutover (backup gate, disable routes, stop all symbols, apply image/release, restart,
  verify, re-enable) and record a drill.
- [ ] **P2** Scale and soak the CRD path: thousands of symbols/CRs across shards and cells, status
  churn, API-server/etcd size and backup/restore, manager informer memory and operator workqueue
  latency. No real engine load is required.
- [ ] **P2** Validate opt-in supervisor cgroups and rotating logs on a Linux node that delegates a
  writable cgroup v2 subtree; OrbStack does not, so the supervisor reports `isolationError` and
  fails open.
- [ ] **P3** GUI: the overview is live, the upgrade tab is a single action and the preview plan tab
  is a read-only simulator. Add order-system create/edit and image-rollback actions,
  migration/promote forms, approvals, filters/pagination, diff/rollback and SSE/WebSocket updates.
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
  metrics for the market-data and order-system HPAs need a metrics server or Prometheus adapter;
  this is tracked as a P1 in Top priorities.
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
