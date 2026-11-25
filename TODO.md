# TODO

Last reviewed: 2026-11-26. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

**2026-11-25 review and fix pass.** `docs/CODE_REVIEW.md` was reduced to the two unresolved
documentation/security items (lab Grafana default credentials, lab TDengine REST exposure); every
concrete bug and performance defect from the full report was fixed, verified with all crate tests,
Clippy/fmt, a terminal build and a Kustomize build, and rolled onto OrbStack. The roll covered new
engine, market-data (Aeron + history), order-system, control-plane and terminal images. The order
system now caches each target engine's full all-user snapshot for 1 Hz and filters by `owner_id`
per client, so the terminal's book stays market-data-fed and client polls never fan out to the
engine. `gRPC StreamEvents` pages are now postcard-encoded through an explicit positional wire
mirror (`POST /events` stays JSON). TDengine debug flags, monitor and taosAdapter INFO logging were
disabled in the lab. All remaining review findings are folded into the single list below.

**Current baseline.** The engine runs OpenRaft 0.10.0 with a segmented WAL, snapshot v11 and exact
18-scale `Fixed` values. It supports the full order-type set (GTC/IOC/FOK/GTD/Day, market,
post-only, iceberg, stop, market-to-limit, trailing and peg) plus STP, maker/taker fees with
asset-aware policies, a replicated event log (`/events`, gRPC streaming), local-lease reads,
bounded write admission and the market-data feed with Aeron multicast or direct NATS egress. One
process/cluster trades one symbol; multi-symbol isolation is deployment-level. The reference fleet
also runs four fast-only
shard market-data services and one global terminal service that consumes every shard feed and
serves one multi-symbol WebSocket.

**Control plane.** The CRD-backed manager and Lease-elected operator manage containers, supervisor,
reserved and migrated shards. The operator bootstraps Raft, configures symbols, rolls one pod at a
time with leader transfer and per-process quorum checks, blocks incompatible releases and
over-capacity renders, retains PVCs and performs cross-shard cutover/rollback. The manager GUI has
live refresh, release/capacity banners, a cluster image picker and a read-only preview plan. The
market-data tier lives under one **market data** top-level view with **terminal** and **fast**
sub-tabs, and images roll through `POST /api/v1/market-data-services/{name}/upgrade` with a
source-shard wire-version check. The `OrderSystem` CRD renders **one stateless** Deployment/Service/
PDB/HPA (min 1) with a base-host Ingress (`orders.<ingress>`) plus per-shard alias Ingresses; every
instance serves every user, and `maxUnavailable:0` image rolls replace the old four-shard layout.
Engine leader discovery is client-side (`MatchEngineClient::with_members`), validated by a hard
leader kill and a rolling upgrade on OrbStack. The `match-eu-1` reference fleet
records image digests in its CRDs, but the demo has no registry: images live only in the node's
local image store, so a `DiskPressure` image GC can leave a roll with nothing to pull. The live
acceptance harness (`scripts/operator-acceptance.sh`) covers nine phases: containers, leader, roll,
stop/start, supervisor, reserved, migration, adoption and NATS. The eight-phase run before the NATS
phase was added passed in 889s; containers+NATS passed in 248s on `operator-v82`. The scheduled
backup path creates, drills, signs and uploads a bundle daily and publishes the verified result; an
hourly job re-verifies it and fails when the newest bundle or drill is stale, and the shard page
shows the same record in a **backup** tab. There are no known P0 defects.

Priority key: **P0** = known correctness/security defect, do next; **P1** = production safety and
operability; **P2** = evidence, scale and important non-blocking work; **P3** = later/optional.

## Top priorities

Last prioritised 2026-11-26. Priority key: **P0** = known correctness/security defect, do next;
**P1** = production safety and operability; **P2** = evidence, scale and important non-blocking
work; **P3** = later/optional. Ranking rule: impact first, then urgency (is the window for cheap
prevention open now?), then effort as a tiebreaker. Items blocked by another host, cluster or
provider stay in this list but are marked as production gates and do not count as queue progress.
The fan-out/slow-consumer soak and the historical-bar P1 work (per-symbol Lease failover and
REST-tail gap recovery) are now closed in History; the remaining ranks were re-checked against the
current OrbStack lab and those results. Nothing is P0: the engine, client and control plane are
green on tests and the live lab.

The 2026-11-25 review fix pass closed every concrete bug and performance defect in
`docs/CODE_REVIEW.md`; the two remaining review findings (lab Grafana default credentials and lab
TDengine REST exposure) are folded into item 1. All images were rebuilt and rolled on OrbStack with
`VERIFY_OK`. The public gateway remains out of the queue (client-side leader routing, no user-ID
routing); its design stays in `docs/GATEWAY_ROUTING.md` behind the stated triggers. The local-lease
semantic risk stays covered by at-least-once retries: `client_order_id` dedupes only `Place`, so
`Cancel`/`Amend` must not be blind-retried until item 15 lands. `POST /events` consumers resume from
`(log_index, ordinal)` and dedupe on `event_id`.

1. **P1 — Control-plane, engine-admin and public-edge security (#1).** Manager OIDC/mTLS,
   authorization, per-route permission bits, test/live mode isolation with a `409` cross-mode
   refusal, console-proxy authorization, destructive-action approvals, idempotency keys,
   namespace-qualified paths, dry-run diffs and audit. Add TLS/client authentication, rate limits,
   origin checks, NetworkPolicy and Pod Security for the order-system, market-data and NATS edges.
   Include the engine's unauthenticated `/init`, `/add-learner`, `/change-membership` and gRPC
   listener (mTLS/shared secret, bootstrap-only membership, per-peer limits), session-bound
   `user_id` for the order desk, per-IP WebSocket quotas, and production TDengine security:
   Secret-backed credentials, TLS, a NetworkPolicy admitting only market-data pods and no public
   REST Ingress. This also closes the two unresolved review findings: Grafana `admin`/`admin` and
   the lab plaintext TDengine REST endpoint.
2. **P1 — Supply-chain and namespace hardening (#20).** Pin GitHub Actions to commit SHAs and Docker
   base images to digests, set `permissions: contents: read` on every workflow, label `match-eu-1`
   restricted with default-deny ingress/egress plus a TDengine NetworkPolicy, and narrow the bridge
   ClusterRole and backup pod-create RBAC to the namespaces that need them.
3. **P1 (production gate, blocked here) — Backup, registry and storage close-out.** One live
   object-storage bucket cycle to confirm rclone upload, retention and alerting, a real container
   registry for digest-pinned images, and one CSI volume-snapshot drill once a snapshot driver
   exists. The local-PVC path, `docker save`/`load` and signed CRD drill are already validated.
4. **P1 (production gate, blocked here) — Multi-node NATS.** Credentials/NKey support in source and
   sink, a multi-node NATS cluster and optional JetStream retention. The single-node Core NATS path
   and the acceptance `nats` phase pass.
5. **P1 (production gate, blocked here) — Multi-host fabric evidence.** Rerun read/write and lease
   A/B tests on a second host and a Linux four-voter cluster, force a stale lease to observe the
   `ReadIndex` fallback, and validate Aeron multicast across hosts rather than one OrbStack node.
6. **P1 (production gate, blocked here) — Linux supervisor isolation and bare-metal benchmarks.**
   Validate opt-in cgroup v2 delegation and log rotation on a node that delegates a writable subtree,
   and dispatch the pinned-core `bench-linux` workflow on the bare-metal runner.
7. **P2 — Rollout invariants and acceptance-harness automation (#4).** Extract the rollout candidate
   decision into a pure function with tests for one terminating pod, replacement Ready before the
   next deletion, and no skip when `currentRevision == updateRevision` while old pods remain. Then
   run the nine-phase `scripts/operator-acceptance.sh` in a disposable CI cluster or nightly.
8. **P2 — Historical bar store, remaining phases (#16).** P0 and all of P1 are shipped and
    validated (see History): TDengine as system of record, exact 18-decimal text I/O,
    `1s`/`1m`/`5m` materialization, read-time rollups, `GET /klines`/`GET /bars?since=<seq>`, the
    rolling trade spool with cursor replay, `bar_state`/`bar_gaps`/`bar_revisions`/`bar_imports`,
    revision audit, per-symbol Lease failover and REST-tail gap recovery. Remaining: archive
    importer and reconciler with gap-gated reads (P2); three-dnode `REPLICA 3`, reader HPAs, backup
    and offset-anchored intervals (P3). Depends on item 1 for Secret-backed DSN, NetworkPolicy and
    no plaintext edge.
9. **P2 — Stop and GTD index refactor (#21).** Replace the linear parked-stop `Vec` scans with a
    trigger-ordered index plus sequence map, and the GTD full-book rescan with a min-heap/`BTreeSet`
    keyed by expiry. The 2026-11-25 trailing-stop counter and narrower invalidation are the interim
    mitigation; add conformance tests for cascades and expiry.
10. **P2 — Pre-trade edge policy pipeline (#5).** A fixed ordered first-failure-wins gate with typed
    codes — halt, blacklist, notional cap, per-account and per-market flow windows, cancel-ratio and
    cancelled-amount-ratio, then self-trade screening — at the edge/gateway, not in the matching
    core or the order desk's durable state.
11. **P2 — Terminal CRD integration (#17).** `DNS_UPSTREAM` is now resolved from
    `/etc/resolv.conf`, the image `arm64-v5` is in the supply chain and the ladder is market-data
    only. Remaining: the `TradingTerminal` CRD/renderer integration. Never call engine `/write`.
12. **P2 — Docs synchronization (#8).** Keep `AGENTS.md`, READMEs, `docs/PERFORMANCE.md`,
    `docs/API.md`, `docs/ETCD_STORAGE.md`, `TODO.md` and the runbooks aligned with storage, wire,
    postcard event pages, BFF caching and control-plane changes. Fix the known Profile A/NATS drift
    first; add a generated-doc diff guard and paired EN/CN updates when the surface exists.
13. **P2 — Engine bar emission for sparse markets (#11).** `BarAgg::new` seeds `last_live_ms` with
    the trade time, suppressing the opening emit; seed it with `0` and test a single-fill bar. The
    terminal no longer depends on it, but the engine feed is authoritative for other consumers.
14. **P2 — Safe retries for `Cancel`/`Amend` and typed leader redirect (#7).** Add a request key for
    cancel/amend and a typed `503` carrying the leader address for followers/gateways; until then,
    document and enforce the no-blind-retry rule.
15. **P2 — Stop-the-world incompatible upgrade execution (#6).** Automate the coordinated cutover
    (backup gate, disable routes, stop all symbols, apply image/release, restart, verify, re-enable)
    and record a drill; the release gate and manual runbook already exist.
16. **P2 — Market-data manager controls (#9).** Add GUI create/edit/scale and a read-only preview
    plan for `MarketDataService`, plus a rollback action restoring the previous image. The upsert
    API, image/release update and picker are implemented.
17. **P2 — Profile A feed hardening (#12).** Characterize the Aeron image-registration race and
    single-node multi-subscriber behavior, make source startup readiness-gated (or add a per-source
    generation), and keep the 5–30 s re-subscribe watchdog without restart loops.
18. **P2 — CRD scale and soak (#13).** Thousands of symbols/CRs across shards and cells, status
    churn, API-server/etcd size, informer memory and operator workqueue latency.
19. **P2 — Defence-in-depth hardening (#19).** Nonce-valued CSRF guards for console/desk actions,
    a manager GUI CSP, correlation IDs instead of internal error strings on 500s, and an admission
    gate for concurrent `/events` long-polls. Keep separate from the P1 auth workstream.
20. **P3 — Review micro-optimisations.** `/klines` sequential TDengine query coalescing, combined
    WebSocket frame precomputation, catalog snapshot reuse and `bar_quote.retain` tuning. Defer until
    a measured bottleneck; the 2026-11-25 fixes already bounded the hot paths.
21. **P3 — Lab-only hardening.** Remove root usage and iptables from
    `match_engine/docker/linux.Dockerfile`, and scope the ingress-nginx ClusterRole's Secrets access.
    Keep both out of production overlays.
22. **P3 — GUI follow-ups.** Order-system create/edit and image-rollback actions, migration/promote
    forms, approvals, filters/pagination, diff/rollback and SSE/WebSocket updates.
23. **P3 — Automation surface.** CLI `create-shard`/`create-symbol`/`migrate`, an OpenAPI client and
    admission validation/defaulting beyond the CRD YAML.
24. **P3 — Multi-cell federation.** Cell registration/capacity, CR projection and cross-cell
    migration; the demo has one cell.
25. **P3 — Engine optional work.** Wire-version negotiation, bounded-admission backpressure,
    snapshot-policy purge knobs, `STREAM_APPEND_CHANNEL_SIZE` benchmark, leader-change callbacks,
    peer preconnect/larger frames/snapshot compression, async-WAL productionization, Aeron Raft
    transport, binary order entry, ITCH replay, binary/compressed WebSocket payloads only after a
    measured JSON bottleneck, derivative fields, reserve replenishment/collars, protobuf/Avro,
    module renaming and AON/order-mask coverage.
26. **P3 — Operator documentation.** Metrics, troubleshooting, capacity planning and
    disaster-recovery runbooks.

The `P0`–`P5` phases inside the historical-bar design are implementation phases, not this file's
P0 defect priority; the historical-bar store (item 8) is P2 here.

## Open work

### Control plane

- [ ] **P1** Production manager and control-plane security: OIDC/RBAC, console-proxy
  authorization, destructive-action approvals, idempotency keys, namespace-qualified paths,
  pagination, dry-run diffs, NetworkPolicy tests and Pod Security Standards for the manager, the
  order-system and market-data edges and the NATS endpoint. Desired state stays in CRDs; use
  Postgres/object storage only for audit, a query/read model or long-term history, not as a second
  source of truth. The demo manager has no auth. The 2026-11-21 pitlane review suggests two
  concrete shapes for this workstream: one named permission bit checked per API route (viewer/
  trader/risk/ops/admin style) and **test/live mode isolation at the session layer** with a `409`
  cross-mode refusal, so a mode-filtered client cannot silently read the other mode's data. Apply
  the same Secret/NetworkPolicy pattern to the future TDengine 6041 endpoint. Audit every control
  action and alarm on stale/jump/link/risk events.
- [ ] **P1 (#2)** Supply-chain and namespace hardening: pin CI actions to SHAs and base images to
  digests, add `permissions: contents: read`, label `match-eu-1` restricted with default-deny
  ingress/egress and a TDengine NetworkPolicy, and narrow the bridge ClusterRole and backup
  pod-create RBAC. Closes the lowest-effort 2026-11-25 security findings.
- [ ] **P2 (#19)** Defence-in-depth: nonce-valued CSRF guards, manager GUI CSP, correlation IDs on
  500s, and an admission gate for `/events` long-poll concurrency.
- [ ] **P1 (deferred)** Gateway integration: leader-aware routing from `SymbolRoute.status`,
  shadow/cutover/drain, retries and a canary path. Deferred by the 2026-11-20 decision in
  `docs/GATEWAY_ROUTING.md` — no public gateway is built until TLS/auth and packaging land, a
  second client that cannot use the Rust client appears, or cutover needs automating. Retry `Place`
  with `client_order_id`; do not blind-retry `Cancel`/`Amend` until the request-key/typed-redirect
  item lands (P2).
- [ ] **P1** Order-system public edge: TLS, authentication, rate limits and NetworkPolicy for the
  base-host order desk (`orders.<ingress>/demo/`) and `/demo/api`; the per-shard hosts are migration
  aliases. The demo currently exposes plaintext HTTP with no auth. Keep order entry in the order
  system and the manager's role to linking only.
- [ ] **P1 (explicit production gate)** Backup close-out: the `backup`/`backup-verify`/`restore
  --drill` CLI, `deploy/backup.yaml` (daily create + drill + HMAC signature + `rclone` upload +
  retention, hourly `backup-status` freshness check), `deploy/backup-alerts.yaml` and the shard
  **backup** tab are implemented and tested. The full path ran live on `fx-eu-01` on 2026-11-19
  (32-object bundle, signed drill, publish, stale and not-configured states) through the
  `scripts/backup-local-lab.sh` PVC path. Remaining: one cycle against a real object-storage
  bucket, confirming retention and alerting, plus the CSI volume drill once a snapshot-capable
  cluster is available.
- [ ] **P2 (Top priorities #10)** Ordered pre-trade rule pipeline at the edge/gateway (pitlane
  review): a fixed first-failure-wins gate with typed rejection codes — halt, open/close blacklist,
  notional cap by class, per-account and per-market order flow windows, cancel-ratio and
  cancelled-amount-ratio gates, then self-trade screening — with zero meaning "rule off" and
  fail-closed behaviour when its fixed cover table is full. Keep it policy-side: do not put user
  state into the stateless order desk or the matching core, and do not replace our replicated
  `owner_id`/STP semantics. This is the natural home for the per-user limits left open by
  solution G.
- [ ] **P2** Automate the acceptance harness and rollout invariants: first add pure-function tests
  for the rollout candidate rules (one terminating pod at a time, replacement Ready before the next
  deletion, and no skip when `currentRevision == updateRevision` while pods still run the old
  template), then run the nine-phase `scripts/operator-acceptance.sh` in a disposable CI cluster or
  nightly. The manual run found the two defects fixed on 2026-11-17; the unit tests are the cheap
  regression guard.
- [ ] **P2** Stop-the-world incompatible upgrade execution: the release gate correctly blocks
  mixed rolls and supervisor rolling is implemented; automate the coordinated stop/upgrade/
  restart cutover (backup gate, disable routes, stop all symbols, apply image/release, restart,
  verify, re-enable) and record a drill.
- [ ] **P2** Scale and soak the CRD path: thousands of symbols/CRs across shards and cells, status
  churn, API-server/etcd size and backup/restore, manager informer memory and operator workqueue
  latency. No real engine load is required.
- [ ] **P2 (blocked on OrbStack)** Validate opt-in supervisor cgroups and rotating logs on a Linux
  node that delegates a writable cgroup v2 subtree; OrbStack does not, so the supervisor reports
  `isolationError` and fails open.
- [ ] **P3** GUI: the overview is live, the upgrade tab is a single action, the preview plan tab is
  a read-only simulator and the order-system/market-data image pickers are implemented. Add
  order-system create/edit and image-rollback actions, migration/promote forms, approvals,
  filters/pagination, diff/rollback and SSE/WebSocket updates.
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

- [ ] **P1** Public market-data edge: ingress TLS/authentication, rate limits, origin checks,
  connection/stream caps, NetworkPolicy and no public Aeron exposure; document the
  gateway/Ingress/manager boundary as part of the security review.
- [ ] **P1** NATS production hardening (partly done): the single-node `deploy/nats.yaml` lab server
  and the `NatsSink`/`NatsSource` round-trip are validated. TCP 4222 egress is now scoped to the
  NATS pods instead of `0.0.0.0/0`, and a `tls://` URL enables TLS with platform roots. Still needed:
  credentials/NKey support in both source and sink, a multi-node NATS cluster (blocked on this
  host), and optional JetStream retention (needs source support plus a stream provisioner).
- [ ] **P1 (blocked on this host)** Validate Profile A across a multi-node multicast-capable
  CNI/VPC (Cilium/Isovalent multicast, or Multus plus AWS Transit Gateway multicast). The default
  EKS VPC CNI cannot run it. Record CNI/MTU, cross-node `/ready` and leader-transfer reset behavior.
  Profile N (`NatsSink` -> NATS Core/JetStream -> `NatsSource`) is the live single-node fallback;
  benchmark it before deciding multicast is required.
- [ ] **P2** Optional Aeron profiles only if the NATS benchmark misses the latency target: direct
  multicast where the multi-node CNI/VPC test passes, or the per-pod IPC bridge for existing Aeron
  deployments. Keep unique stream bases per engine process (`2001 + 7 * slot`), a cell-unique
  `symbol_id`, distinct per-process Aeron directories and no shared `/dev/shm`.
- [ ] **P2** Market-data profile extras: all-market aggregate streams (`!miniTicker@arr`), a
  per-connection terminal frame budget, per-profile connection/lag metrics, explicit WebSocket close
  reasons and a ServiceMonitor.
- [ ] **P2** Manager controls for the market-data tier: add GUI create/edit/scale and a read-only
  preview plan; the upsert API, image/release update and the per-service image picker are
  implemented. Add a rollback action that restores the previous image.
- [ ] **P2 (Top priorities #11)** Trading terminal hardening: the pure-React client is deployed as
  `arm64-v5` and `DNS_UPSTREAM` is resolved from `/etc/resolv.conf` at pod start. Remaining: add the
  `TradingTerminal` CRD/renderer integration. Keep order entry on the order-system BFF and never
  call engine `/write` directly.
- [ ] **P2** Rollout and availability: canary policy, PDB verification, drain-aware image upgrades
  and a tested rollback path that restores subscriber capacity without touching matching. HPA CPU
  metrics are live (#3); the completed fan-out/slow-consumer soak (see History) measured
  real scaling.
- [ ] **P2** Aeron feed hardening (Profile A only): characterize the image-registration race and the
  single-node multi-subscriber behavior; make source startup readiness-gated (or add a per-source
  generation) so a stale feed is detected and rebuilt without log spam. Keep the 5-30 s
  re-subscribe watchdog as the fallback and avoid restart loops when an engine feed is genuinely
  down.
- [ ] **P2** Observability extras: the JSON→text bridge now exposes market-data messages, `GAP`s,
  `resync`s, snapshot requests, slow closes, per-symbol readiness/last-frame/subscribers and
  terminal conflation/throttling; Prometheus alerts cover gaps and not-ready symbols and the
  Grafana dashboard shows message rate. The soak added queue-depth, encode-CPU and close-reason
  counters; remaining: expose those series in Prometheus/Grafana and surface alert state in the
  manager status view. The core stack is done (#3 in History).
- [ ] **P3** Optional archive and replay: persist feed archives to object storage or a PVC with
  retention/checksums and replay through the same assembler for incident analysis; archives are
  data, not desired state, and must never go into etcd.
- [ ] **P3** Evaluate binary or compressed WebSocket payloads only after profiling shows JSON
  encoding/fan-out is the bottleneck; the third-party reviews are a caution against claiming
  unmeasured wire-format gains.

### Engine and transport

- [ ] **P2 (#9)** Stop and GTD index refactor: replace the parked-stop `Vec` scans with a trigger
  index and sequence map, and the GTD earliest-expiry rescan with a min-heap/`BTreeSet`. Keep the
  trailing-stop counter and narrow invalidation shipped on 2026-11-25 as the interim mitigation.
- [ ] **P2** Make gateway retries safe: add a request key for `Cancel`/`Amend` (the current
  `client_order_id` dedupes only `Place`) and a typed `503` with the leader address so a follower
  `/write` can be retried deterministically instead of today's `500`. The Rust
  `MatchEngineClient::with_members` path already re-discovers the leader from `/metrics` on
  transport failure, so this is for gateways/non-Rust clients. Until then, document and enforce the
  no-blind-retry rule so the gateway never duplicates a cancel or amend.
- [ ] **P2 (blocked on this host)** Validate the default `local_lease` path on a second host and a
  Linux multi-host four-voter cluster: rerun `read_e2e --policy lease|local_lease` plus the
  e2e/cluster write A/B, force a stale lease or cache miss so the `ReadIndex` fallback is observed,
  and check `/read-policy/status` counters under bounded clock skew. The OrbStack four-container
  lab proves the Linux path but is still one host.
- [ ] **P2 (blocked on this host)** Validate Aeron multicast across a real multi-host fabric:
  separate hosts/drivers on a non-loopback NIC, independent service instances and no shared
  `/dev/shm`. The single-host `lo` path is covered by `scripts/aeron-linux-lab.sh`.
- [ ] **P2 (blocked on the bare-metal runner)** Dispatch `.github/workflows/bench-linux.yml` for the
  bare-metal pinned short and sustained `e2e` rows. The OrbStack container run covers the short
  pinned check in both fsync modes; this is the remaining bare-metal evidence.
- [ ] **P3** Productionize async WAL only after the second-host Linux multi-node A/B and pipelining
  work: add periodic-mode support if useful, bound the pending cache with backpressure, extend
  crash/failover coverage and test whether more append concurrency exposes a real gain. Keep the
  synchronous WAL as the default until then.
- [ ] **P3** Add wire-version negotiation so wire-compatible releases can roll instead of using the
  stop-the-world cutover; keep the release gate until then.
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
  storage/wire changes. Fix the known drift first: `k8s_control_plane/README.md` still says Profile
  A is the only rendered transport, and the control-plane design doc still lists NATS as a
  fallback, while Profile N is rendered and live on OrbStack. The pitlane review shows a mechanical
  guard: regenerate derived docs and fail CI on a diff, and require paired EN/CN doc updates in the
  same change. Add the equivalent check once the generated-doc surface exists; until then, keep the
  drift list explicit here.
- [ ] **P3** Decide whether to rename the remaining mixed-style modules before 1.0
  (`src/match_result/` → `match_events/`). This is import-breaking and offers no functional benefit
  today.

## History

Completed work is summarised here; detailed evidence, measurements and implementation notes remain
in git history and the linked docs. For the full history, check out `TODO.md` from an earlier commit
(`git log -- TODO.md`).

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
  checklist item is closed here; its remaining phases stay in Top priorities #8.

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
