# TODO

Last reviewed: 2026-11-23. Working roadmap only; completed detail lives in the History section and
git history — check out an earlier `TODO.md` from git for the full record
(`git log -- TODO.md`, then `git show <commit>:TODO.md`). Measurements remain in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md). See `AGENTS.md` for commands and the current branch
state.

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

Last prioritised 2026-11-23. Since the last pass four items were closed: Raft-native leader
discovery/failover in `MatchEngineClient` (validated by a hard leader kill and a rolling upgrade),
the bounded gRPC drain (pods now terminate in ~6 s instead of hanging until SIGKILL), the
stateless order desk (solution G — one Deployment, no user-ID routing), and the market-data
lifecycle/topology coupling (symbol create attaches it to every non-paused service in the cell,
migration cutover waits for per-symbol feed readiness — validated live by creating `eurchf`
through the GUI and watching the fast feed deliver real bookTicker frames). The public gateway is
therefore **out of the execution queue**: leader routing is client-side and user-ID routing was
removed rather than built. Its full design stays in `docs/GATEWAY_ROUTING.md` behind the stated
triggers. The pitlane review's concrete findings are folded into the P1 security item (mode
isolation, per-route permission bits) and a P2 pre-trade policy pipeline.

Backup construction is complete, so its remaining work — a real object-storage cycle and the CSI
volume drill — stays in the explicit production gates. The market-data remainder (multi-node NATS
with credentials/NKey, optional JetStream retention, and multi-node Profile A multicast) is
likewise infrastructure-gated, not actionable on this host. Ordering weighs security exposure,
image supply and production blockers first, then cheap regression guards, then evidence/scale;
environment-blocked validation is called out explicitly rather than mixed into the actionable
queue. Nothing is P0: the engine, client and control plane are green on tests and the live lab.

The local-lease semantic risk stays covered by the delivery contract: client retries are
at-least-once. `client_order_id` makes `Place` retries idempotent; `Cancel`/`Amend` have no request
key yet, so clients and any future gateway must not blind-retry them. Downstream event readers
resume from `(log_index, ordinal)` and dedupe on `event_id`. A local-lease cache hit may overtake
an unfinished write, so read-after-write is not a guarantee; clients that need it must await the
write response.

**Priority assessment (2026-11-23).** Two things changed the tail of the queue since the last pass.
The Binance-style trading terminal (`trading_terminal/`) is built and deployed as a pure frontend
third client: it derives candles from the trade tape, the market-data service replays retained bars
on subscribe, and order entry goes through the order-system BFF. That removes the "empty chart"
urgency from engine bar emission. The historical-bar tier's **P0 is now implemented** in `market_data_system/`: TDengine schema,
trade-derived `1s`/`1m`/`5m` writer, exact-decimal text I/O, `GET /klines` with read-time rollups
(`30m` from `5m`) and a TDengine-seeded WS replay. Lab evidence now includes a continuous order
replay feeding the tape, a 5m↔30m comparison script and a watermark-based finality fix for the open
30m bar. The remaining phases (trade spool, Lease failover, revisions/gaps, `/bars`, importers,
time-zone anchors and the three-dnode deployment) are still design, with an HA contract for one
writer and stateless queriers
([`docs/HISTORICAL_BARS.md`](./docs/HISTORICAL_BARS.md) §§1, 4.2.1). They rank below P1:
the new stateful component's DSN, Secret and NetworkPolicy must follow the P1 security/packaging
pattern rather than adding another plaintext edge.

Ranking rule: impact first, then urgency (is the window for cheap prevention open now?), then
effort as a tiebreaker; work blocked by another host, cluster or provider stays in the explicit
production gates and is not counted as queue progress.

| Rank | Item | P | Impact | Effort | Gate / reason for this rank |
|---|---|---|---|---|---|
| 1 | Control-plane and public-edge security (#1) | P1 | High | High | None — manager, both demo edges, NATS and the lab TDengine REST ingress are plaintext; exposure compounds with every new service |
| 2 | Packaging and image supply (#2) | P1 | High | Medium | None — already broke a real rollout via `DiskPressure` image GC; must include the TDengine deployment/Secret before history ships |
| 3 | Observability and HPA metrics (#3) | P1 | High | Medium | None — HPAs are pinned at min 1, so all scale/soak evidence waits on this |
| 4 | Rollout invariants + acceptance automation (#4) | P2 | Medium | Low | None — cheap regression guard for P1 rollout changes; two real defects were found manually |
| 5 | Historical bar store, remaining phases (#16) | P2 | High | High | P0 shipped with 5m/30m rollups and lab evidence; spool/failover, revisions/gaps, importers and HA deployment need #1/#2 and a three-dnode TDengine cluster |
| 6 | Terminal deployment hardening (#17) | P2 | Medium | Low | Deployed with a hardcoded ClusterIP; also needs the CRD integration follow-up and #2 for image supply |
| 7 | Docs synchronization (#8) | P2 | Medium | Low | Cheap guard; keep this file, `AGENTS.md` and the design docs aligned with the TDengine decision |
| 8 | Pre-trade policy pipeline (#5) | P2 | Medium | Medium | Security-adjacent but not blocking; belongs at the edge/gateway |
| 9 | Engine bar emission (#11) | P2 | Low | Low | Feed correctness, but the terminal no longer depends on it; needs a four-voter engine image roll |
| 10 | Fan-out and slow-consumer soak (#10) | P2 | Medium | High | Needs the HPAs from #3 before its results mean anything |
| 11 | Continuous order-replay evidence (#18) | P2 | Low | Low | Harness exists; useful now as a cheap tape/kline soaker while HPA metrics (#3) are unavailable |

The `P0`–`P5` phases inside the historical-bar design are implementation phases, not this file's
P0 defect priority; item #16 is P2 here. Environment-blocked items (multi-host, CSI, bare-metal,
real object storage, three-dnode TDengine) stay visible in the production gates and are not counted
as queue progress.

**P1 - production safety and operability, in execution order**

1. **Control-plane and public-edge security.** One workstream for manager OIDC/RBAC,
   console-proxy authorization, destructive-action approvals, idempotency keys,
   namespace-qualified paths and dry-run diffs, plus TLS, client authentication, rate limits,
   origin checks and NetworkPolicy/Pod Security for the order-system, market-data and NATS edges.
   It leads the queue because the manager, both demo edges and the NATS broker endpoint are all
   plaintext and unauthenticated: exposure is the largest remaining production gate and no amount
   of scaling or backup evidence mitigates it. The pitlane review suggests the concrete shape:
   one named permission bit per route (viewer/trader/risk/ops/admin) and test/live mode isolation
   at the session layer with a `409` cross-mode refusal. Include TDengine in this workstream: the
   lab currently maps `taosAdapter` REST through a plaintext Ingress with default credentials, so
   production needs Secret-backed credentials, a NetworkPolicy admitting only market-data pods,
   TLS and no public exposure — the historical-bar design must not ship a fourth plaintext edge.
2. **Manager packaging and image supply.** Check in Deployment/Service/RBAC/NetworkPolicy/Pod
   Security, Helm/Kustomize, digest-pinned image publication and CRD/etcd backup integration. The
   demo still depends on local-only images, an `emptyDir` and a port-forward, which blocks a clean
   install; a `DiskPressure` image GC evicted them during the 2026-11-18 rollout, so publish to a
   local registry or add an explicit `docker save`/`load` step before the next fleet roll. It
   follows security because it has already broken a rollout. Package the TDengine deployment with
   it — one dnode for the lab, three dnodes/mnodes with `REPLICA 3` for production — because the
   historical bar store is not shippable without a database to write to.
3. **Observability and HPA metrics.** Install a metrics-server or Prometheus adapter so the one
   order-system and five market-data HPAs can report CPU and actually scale, then add the
   Prometheus text bridge for the engine's JSON `/metrics` (or a native `/metrics/prometheus`),
   operator reconcile/rollout metrics, alerts, dashboards and log shipping. This is the largest
   scale and evidence unblock: every HPA currently shows `<unknown>` and only the min-1 floor
   applies, and load/soak work depends on it. It follows packaging because the current min-1 floor
   still serves traffic and no known defect depends on autoscaling.

**Explicit production gates not exercised in this lab.** Backup close-out: one live
object-storage bucket cycle to confirm rclone upload, retention and alerting (the local-PVC path
and the signed CRD drill are validated), and the CSI volume-snapshot drill, which needs
snapshot CRDs and a driver this host does not have. Also multi-host lease/`ReadIndex` and Aeron
multicast evidence (one OrbStack node), a multi-node NATS cluster with credentials/NKey and
optional JetStream retention (needs source work plus infrastructure), Linux supervisor cgroup
delegation (OrbStack), and the bare-metal pinned-core `bench-linux` dispatch. The lists below
keep those halves visible; do not treat a green single-node run as production evidence for them.

**P2 - policy, evidence and scale, in execution order**

4. **Rollout invariants and acceptance-harness automation.** The manual harness found two real
   rollout defects, now fixed. First extract the rollout candidate decision into a pure function
   with unit tests for: at most one terminating pod, the replacement must be Ready before the next
   deletion, and no roll may be skipped when `currentRevision == updateRevision` while pods still
   run the old template. Then run the nine-phase `scripts/operator-acceptance.sh` in a disposable
   CI cluster or nightly. The unit tests are the cheap regression guard; land them before the CI
   plumbing.
5. **Pre-trade edge policy pipeline (pitlane-derived).** The stateless order desk removed user
   ranges but has no per-user flow limits, blacklist or cancel-ratio gate — the policy half of P1
   item 1. Implement a fixed, ordered first-failure-wins gate with typed codes: halt,
   open/close blacklist, notional cap by class, per-account and per-market flow windows,
   cancel-ratio and cancelled-amount-ratio, then self-trade screening; zero means "rule off" and a
   full cover table fails closed. Keep it at the edge/gateway, not in the matching core or the
   order desk's durable state; it does not replace our replicated `owner_id`/STP semantics.
6. **Stop-the-world incompatible upgrade execution.** The release gate correctly blocks mixed
   rolls, and the runbook has a manual stop/upgrade/restart path. Automate the coordinated cutover
   (backup gate, disable routes, stop all symbols, apply image/release, restart, verify, re-enable)
   and record a drill. The backup gate now uses the real published bundle timestamp.
7. **Safe retries for `Cancel`/`Amend` and typed leader redirect.** The 2026-11-20 failover work
   closed the Rust-client half: `MatchEngineClient::with_members` re-discovers the leader from
   `/metrics` on transport failure, so the order system needs no typed redirect. Remaining: a
   request key for cancel/amend (`client_order_id` still dedupes only `Place`) and a typed `503`
   with the leader address so a gateway or non-Rust client can refresh and retry deterministically;
   a follower `/write` still returns `500` with no leader hint. Until then, document and enforce
   the no-blind-retry rule before the gateway can safely retry those requests.
8. **Docs synchronization.** Keep `AGENTS.md`, READMEs, `docs/PERFORMANCE.md`, `docs/API.md`,
    `docs/ETCD_STORAGE.md` and the runbooks moving with storage, wire, read-policy and
    control-plane changes. Fix the known drift first: `k8s_control_plane/README.md` still says
    Profile A is the only rendered transport, and the control-plane design doc still lists NATS as
    a fallback, while Profile N is rendered and live on OrbStack. The pitlane review gives the
    mechanical guard: regenerate derived docs and fail CI on a diff, plus paired EN/CN updates in
    the same change.
9. **Market-data manager controls.** Add GUI create/edit/scale and a read-only preview plan for
    `MarketDataService`; the upsert API, image/release update and the per-service picker are
    implemented. Add a rollback action that restores the previous image.
10. **Market-data fan-out and slow-consumer evidence.** Soak many symbols, streams, connections
    and slow consumers through the Kubernetes deployment; measure per-instance encode/fan-out
    CPU, queue depth, gap/resync behavior and p99, and prove the engine's matching path is
    unaffected.
11. **Engine bar emission for sparse markets.** `MarketDataProcessor::update_bars` never publishes
    the first bar of a window: `BarAgg::new` seeds `last_live_ms = ts_ms`, so the 1 Hz partial-bar
    gate suppresses the opening emit, and a bar closes only when a later fill lands in a newer
    window. A quiet market therefore publishes no kline frames and the newest candle always lags
    one trade. Seed `last_live_ms` with `0` (emit once per fill, then at most 1 Hz) and add a case
    to `src/market_data/tests.rs` asserting a single-fill bar is emitted; optionally close bars from
    a feed timer so the last candle rolls without a successor. The trading terminal no longer
    depends on it (it derives bars from the trade tape and the service replays retained bars), but
    the engine feed is still the authoritative source and the market-data test page only shows
    candles through it. Needs an engine image roll (four voters per shard).
12. **Profile A feed hardening.** Startup snapshot/retry, supervisor restarts on configuration
    changes, Profile N direct NATS and the acceptance `nats` phase are live. For clusters still on
    multicast, characterize the Aeron image-registration race and single-node multi-subscriber
    behavior, make source startup readiness-gated (or add a per-source generation), and keep the
    5-30 s re-subscribe watchdog without restart loops.
13. **CRD scale and soak.** Thousands of symbols/CRs across shards and cells, status churn,
    API-server/etcd size, informer memory and operator workqueue latency.
14. **Multi-host evidence (blocked on this host).** Rerun the read/write A/B on a second host and a
    Linux multi-host four-voter cluster, force a stale lease so the `ReadIndex` fallback is
    observed, validate Aeron multicast across hosts, and dispatch the bare-metal pinned-core `e2e`
    workflow.
15. **Linux supervisor isolation (blocked on this host).** Validate opt-in cgroup v2 delegation
    and rotating logs on a node that delegates a writable subtree; OrbStack cannot, so the
    supervisor reports `isolationError` and fails open.
16. **Historical bar store (P0 implemented, P1+ designed).** [`docs/HISTORICAL_BARS.md`](./docs/HISTORICAL_BARS.md)
    extends `market_data_system` rather than adding a service. **TDengine is the system of record**:
    sparse traded `1s` rows (`DECIMAL(38,18)`, `PRECISION 'us'`) plus materialized `1m`/`5m` rollups;
    `3m` and `15m`…`1M` are read-time `INTERVAL` rollups, with `30m` generated from `5m` children when
    present and from `1m` otherwise (field-by-field 1m→5m→30m unit test). Measured `1s`→`1m` exact
    1,440/1,440 vs the Binance archive, 755 qps on eight concurrent readers, 166k-259k rows/s bulk
    ingest. The open-bar `x` flag now comes only from the event-time watermark, and
    `scripts/check-30m-rollup.py` compares live 30m bars with their traded 5m children (18/18 clean
    windows after the replay soak). No segment format, catalog, manifest, checkpoint or reader
    cache; one Lease-guarded writer with a local trade spool, stateless readers querying TDengine
    (HA/failover contract in §4.2.1 of the design).
    Exact decimals: canonical 18-dp text on write, `CAST … AS VARCHAR` on read — the `taos`
    connector's `rust_decimal` is 96-bit and loses digits. Binance-compatible `GET /klines` + native
    `GET /bars` + WS `seq`/`rev` repair on the existing routes, and a `data.binance.vision` importer.
    **Ranked #5, top of P2 feature work — not an engine P0:** the terminal already avoids empty charts
    by deriving bars from the tape and replaying retained windows; this item buys durable history,
    HA queriers and exact klines. **P0 is implemented** in `market_data_system/` (`--history`,
    `src/history/`, `tests/history.rs` gated on `TDENGINE_URL`): the schema, trade-derived
    `1s`/`1m`/`5m` writer, canonical 18-decimal text I/O, `GET /klines` (including `5m`-first rollups
    for `30m`) and a TDengine-seeded WS replay; it runs on one dnode with no HA yet. Remaining: P1 = trade spool,
    Lease failover, revisions, `bar_gaps`/`bar_state`, restart/catch-up and `/bars`; P2 = importer,
    REST tail, reconciler; P3+ = three-dnode `REPLICA 3` cluster, HPA readers, backup and
    offset-anchored intervals. Production phases depend on P1 #1/#2 (Secret-backed DSN,
    NetworkPolicy, packaged TDengine), the six live-API verification items in
    `docs/THIRD_PARTY_INVESTIGATION.md` Part VII §6.6, and `VGROUPS`/`REPLICA` sizing measured on a
    real three-dnode cluster.
17. **Trading terminal deployment hardening.** The React/TS terminal is built and deployed as
    `matchengine-trading-terminal:arm64-v4`; it is a pure frontend third client (market data over the
    global service WS, order entry through the order-system BFF `/demo/api`) and nginx is deployment
    plumbing only. Remaining: replace the hardcoded `DNS_UPSTREAM: 192.168.194.138` in
    `k8s_control_plane/deploy/terminal.yaml` with a runtime-resolved ClusterIP or Service DNS entry,
    add the `TradingTerminal` CRD/renderer integration, and route its image through the P1 #2 image
    supply. Do not add engine `/write` calls or embed the frontend in a crate.
18. **Continuous order-replay evidence.** `scripts/replay-continuous.sh` loops
    `scripts/replay-order-system.py` against one BFF target (default `gbpusd`) with a configurable
    pause; each pass cancels leftover orders first, so it exercises the tape and kline path without
    growing the book unbounded. Use it as a cheap soak while HPA metrics (#3) are unavailable, then
    replace it with a controlled load test once autoscaling reports CPU. Keep it out of CI — it writes
    to the live lab engine.

**P3 - later, in impact order**

1. **GUI follow-ups:** the order-system image picker and upgrade modal are implemented; add
   order-system create/edit and image-rollback actions, migration/promote forms, approvals,
   filters/pagination, diff/rollback and SSE/WebSocket updates.
2. **Automation surface:** CLI `create-shard`/`create-symbol`/`migrate`, an OpenAPI client and
   admission validation/defaulting beyond the CRD YAML.
3. **Multi-cell federation:** cell registration/capacity, CR projection and cross-cell migration.
4. **Engine optional work:** wire-version negotiation for compatible rolls, bounded-admission
   backpressure, snapshot-policy purge knobs, `STREAM_APPEND_CHANNEL_SIZE` benchmark,
   leader-change callbacks, peer preconnect/larger frames/snapshot compression, async-WAL
   productionization, Aeron Raft transport, binary order entry and ITCH-style replay,
   binary/compressed WebSocket market-data payloads only after a measured JSON fan-out bottleneck,
   derivative fields, reserve replenishment/collars and protobuf/Avro, module renaming and
   AON/order-mask coverage.
5. **Operator documentation:** metrics, troubleshooting, capacity planning and disaster-recovery
   runbooks.

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
- [ ] **P1** Manager deployment packaging: checked-in Deployment/Service/RBAC/Pod Security/
  NetworkPolicy, Helm/Kustomize, image digest pinning/publishing and etcd/CRD backup integration.
  No PVC is needed for desired state; an optional volume is only for audit/export caches. The
  console, engine-console proxy and order selector now have checked-in Ingress hosts
  (`deploy/web-ingress.yaml`); the workload and packaging manifests remain. The demo images exist
  only in the node's local store, so add a registry (or an explicit `docker save`/`load` step)
  before the next fleet roll; a `DiskPressure` image GC evicted local images on 2026-11-18.
- [ ] **P1** Observability: install a metrics-server/Prometheus adapter so the one order-system and
  five market-data HPAs report CPU and scale, then add a Prometheus text bridge for the engine's
  JSON `/metrics`, operator reconcile/rollout metrics, alert rules, dashboards and log shipping.
  The console peer view is already covered by the NetworkPolicy rule.
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
- [x] **P1** Order-system user-routing edge — resolved: the BFF no longer enforces user ranges and
  one stateless Deployment/Service serves every user on the base `orders.<ingress>` host; the
  per-shard hosts remain as migration aliases. Remaining work is per-user auth/rate limits, which
  belongs to the P1 security item and the deferred gateway.
- [ ] **P1 (explicit production gate)** Backup close-out: the `backup`/`backup-verify`/`restore
  --drill` CLI, `deploy/backup.yaml` (daily create + drill + HMAC signature + `rclone` upload +
  retention, hourly `backup-status` freshness check), `deploy/backup-alerts.yaml` and the shard
  **backup** tab are implemented and tested. The full path ran live on `fx-eu-01` on 2026-11-19
  (32-object bundle, signed drill, publish, stale and not-configured states) through the
  `scripts/backup-local-lab.sh` PVC path. Remaining: one cycle against a real object-storage
  bucket, confirming retention and alerting, plus the CSI volume drill once a snapshot-capable
  cluster is available.
- [ ] **P2 (Top priorities #5)** Ordered pre-trade rule pipeline at the edge/gateway (pitlane
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

- [x] **P1** Market-data topology and symbol lifecycle coupling — done. Creating a symbol attaches
  it to every non-paused service in the cell; stopping/starting is derived from deployment state;
  migration waits for per-symbol feed readiness before finishing the route cutover. Subscriber
  channels bind the wildcard address, so multiple pods may consume one group on one node and no
  one-per-node placement rule is required. Deployments add hostname/zone topology spread for
  availability. WebSocket sessions drain via the existing `preStop` hook on service rolls.
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
- [x] **P2 (Top priorities #16)** Historical bar store P0: TDengine schema, `1s`/`1m` builder and
  upserts, `GET /klines`, WS snapshot/replay and exact `DECIMAL(38,18)` text I/O are implemented in
  `market_data_system/` (`--history`, `src/history/`, `tests/history.rs`). Remaining phases are
  tracked in the Top priorities item: trade spool/Lease failover, revisions/gaps, importer/REST
  tail, three-dnode `REPLICA 3` cluster and HPA readers are gated on P1 security/packaging and a real
  TDengine cluster. The OrbStack lab is wired (`deploy/tdengine.yaml`, `MarketDataService.history`,
  `history-arm64-v4` + terminal `arm64-v4`) and the chart now seeds 500 bars from TDengine;
  close the six live-API verification items before freezing the schema.
- [ ] **P2 (Top priorities #17)** Trading terminal hardening: the pure-React client is deployed but
  `k8s_control_plane/deploy/terminal.yaml` hardcodes `DNS_UPSTREAM: 192.168.194.138`; resolve it at
  runtime, add the `TradingTerminal` CRD/renderer integration, and route the image through the P1
  image supply. Keep order entry on the order-system BFF and never call engine `/write` directly.
- [ ] **P2 (Top priorities #18)** Continuous order-replay evidence: keep `scripts/replay-continuous.sh`
  as a lab-only tape/kline soaker (default target `gbpusd`, 5 s pause, cleanup each pass) until HPA
  metrics (#3) land; then replace it with a controlled load test. Never run it in CI.
- [ ] **P2** Rollout and availability: canary policy, PDB verification, drain-aware image upgrades
  and a tested rollback path that restores subscriber capacity without touching matching. HPA CPU
  metrics for the market-data and order-system HPAs need a metrics server or Prometheus adapter;
  this is tracked as a P1 in Top priorities.
- [ ] **P2** Aeron feed hardening (Profile A only): characterize the image-registration race and the
  single-node multi-subscriber behavior; make source startup readiness-gated (or add a per-source
  generation) so a stale feed is detected and rebuilt without log spam. Keep the 5-30 s
  re-subscribe watchdog as the fallback and avoid restart loops when an engine feed is genuinely
  down.
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
