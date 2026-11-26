# Gateway routing and user-ID shard routing (design)

Status: **G adopted and implemented** — the order system is now a single stateless Deployment and
the public gateway is deferred per §9. The full A/F design remains documented for the triggers in
§9. Date: 2026-11-20.

## 1. Problem

Two data-path gaps remain after the control plane and the four order-system shards landed:

1. **Trading clients have no stable, leader-aware public entry point.** The engine's HTTP API is
   plaintext and internal-only. `SymbolDeployment.status.leaderAddr` names the current Raft leader
   per process, but nothing fronts it: a client must know the leader address and retry on
   `NotLeader` itself. The reference design already assigns the responsibilities — the manager
   writes `SymbolRoute.spec`, the operator reports `status`, and "the gateway watches it"
   (`docs/KUBERNETES_CONTROL_PLANE.md` §6.7, §10) — but the gateway component does not exist.
2. **Clients must pick an order-system shard URL manually.** `OrderSystem.spec.shards` defines
   inclusive `userIdMin`/`userIdMax` ranges and the renderer gives each shard its own Ingress host
   (`users-0.orders.<ingress>`, …) plus the selector host `orders.<ingress>`. Today the manager
   only links to each shard's WebUI; there is no host that accepts a user ID and routes to the
   owning shard, so every client has to know the range map. **This half may disappear entirely**
   if the order system becomes stateless with per-user state in a sharded database (§4-F) — that
   is the key alternative this document now weighs.

The engine now has request keys and a typed leader redirect (P2 item 7): `Place.client_order_id`
and `Cancel`/`Amend.client_request_id` share one idempotency namespace, and a follower `/write`
answers `503` with `leader_addr`. A gateway may retry a redirected request **only when it carries one
of those keys**; unkeyed `Cancel`/`Amend` retries must still be refused.

## 2. Current state (evidence)

- `SymbolRoute` (`k8s_control_plane/src/model.rs:833–882`): `spec.symbol_ref`, `cell_ref`,
  `active_deployment_ref`, optional `previous_deployment_ref`, `state`; `status.phase`,
  `leader_addr`, `last_cutover_at`. `RouteState` = `Active | Shadow | Cutover | Draining |
  Rollback | Disabled` (serde `PascalCase`, default `Active`). CRD `symbolroutes.matching.io`
  (`deploy/crds.yaml:145–179`) is `x-kubernetes-preserve-unknown-fields` with a status subresource.
  **`last_cutover_at` is declared but never written anywhere** — the gateway must infer cutover
  from `spec.state`/`activeDeploymentRef`/`previousDeploymentRef`, not this field.
- `SymbolDeployment.status.leaderAddr` is written by the operator (`operator.rs::route_status`
  3187–3216, `deployment_status` 3134–3185) from `/metrics` per process; `SymbolShard.status.symbols[]`
  is a projection. The design doc explicitly says the gateway resolves
  `activeDeploymentRef → deployment.status.leaderAddr` and must retry on `NotLeader` (§6.7, §5.4).
  `RouteStatus.leader_addr` is `None` unless the active deployment is Ready.
- Manager API already exposes `GET /api/v1/routes` and `GET /api/v1/order-systems/{name}`; the
  latter returns each shard's `userIdMin`, `userIdMax`, `service` and `url`.
- Order-system renderer emits one Deployment/Service/Ingress/PDB/HPA per user shard and a selector
  Ingress at `orders.<ingress>` (`render.rs:2348–2382`); the GUI lists WebUI links per shard
  (`static/index.html`, `api.rs::order_system_detail`). Each instance enforces its range in-process
  (`DemoApp::require_user`, `state.rs:604–614`) via `--user-id-min/--user-id-max`
  (`main.rs:50, 112`) and rejects a foreign user with `409 user_shard`.
- **The order system is nearly stateless today.** Real order state (book, sequence, events) lives in
  the engine; the only process-local state in the BFF is the demo-only
  `leverages: Mutex<HashMap<u64, u32>>` keyed by engine sequence (`state.rs:115, 578`). There is no
  database, no user balance/freeze store and no session state. That makes the stateless direction
  (§4-F) cheap *now*, before real per-user state is added, and expensive later.
- Migration/cutover: `provision.rs` produces `RouteState::Cutover` plans with
  `previousDeploymentRef` and `drain`/`rollback` steps (`provision.rs:753–788`); the operator's
  `execute_migrations()` patches `activeDeploymentRef`→shadow, `previousDeploymentRef`→source and
  `state=Cutover` (`operator.rs:1676–1696`), and `finish_cutovers()` moves target to `Active`, source
  to `Draining` and route to `Active` (`operator.rs:1847–1877`). `rollback_migrations()` reverses it
  (`operator.rs:1946–1986`). `RouteState::Disabled` is the gateway "stop new writes" signal
  (`provision.rs:911`, `reconcile.rs:698–707`). No gateway consumes any of it today. Cross-cell
  migration is explicitly deferred to a gateway cutover (`provision.rs:683`).
- `CellSpec.gateway: GatewayRef { url }` (`model.rs:911, 932–937`) is a **dead field**: seeded from
  the demo bundle (`bundle.rs:147–149`) but never rendered, read or exposed via the API. There is no
  Gateway CRD — the seven CRDs are Cell, SymbolShard, TradingSymbol, SymbolDeployment, SymbolRoute,
  MarketDataService and OrderSystem (`deploy/crds.yaml:2–251`). The only `gateway`-labelled objects
  are the demo ingress-nginx namespace and the NetworkPolicy selectors that admit it
  (`render.rs:649–674, 1761–1779`).
- Raft-native client failover is already implemented and live (`MatchEngineClient::with_members`,
  [docs/LEADER_ENDPOINT_ROUTING.md](./LEADER_ENDPOINT_ROUTING.md)): on a transport error it
  resolves the leader from `GET /metrics` on the member set. The gateway can reuse the same idea
  for freshness instead of relying solely on a possibly-stale CR status.
- The demo's public hosts are served by ingress-nginx plus a Docker port-80 front door
  (`scripts/ingress-frontdoor.sh`); all plaintext. TLS/auth/rate limits are P1 security work and
  are explicitly **out of scope** for this design except where the gateway must not make them
  worse. `deploy/web-ingress.yaml:80–105` defines `orders.<base>` as a selector that redirects to
  the manager GUI's order-systems view — it does not route orders.

**Implementation gaps (verified):** no gateway workload/Service/Ingress is rendered from any CRD;
no user-ID→shard lookup component exists (clients pick `<shard>.<ingress>` manually and the order
system enforces the range with `409 user_shard` at `state.rs:597–614`); no API reads or sets a
gateway traffic disposition beyond raw route CR patches; the renderer emits `<shard>.<ingress>`
Ingresses (`render.rs:2348–2382`) but no test asserts the host string (only the count). The
acceptance harness already has a `migration` phase with `wait_route_cutover()` /
`wait_route_rollback()` (`scripts/operator-acceptance.sh:1606–1716`) that the gateway phase can
reuse.

## 3. Requirements and constraints

1. One stable host for trading: route only allowlisted symbols, resolve the current leader, retry
   safely, never expose `/init`, `/console`, `/metrics`, `/change-membership` or Raft RPCs.
2. Honor `SymbolRoute.spec.state` and `previousDeploymentRef`: `Shadow` gets no traffic;
   `Disabled` gets no new writes; `Cutover`/`Draining` have explicit canary and drain semantics.
3. One stable host for order entry: in phase 1, map a user ID to the owning order-system shard by
   range and keep the existing per-shard hosts for direct access; in phase 2 (F), a single
   Deployment/Service serves all users and no user-ID routing is needed.
4. No blind retries of `Cancel`/`Amend`; `Place` retries only with `client_order_id`.
5. No new CRDs. Reuse `SymbolRoute`, `SymbolDeployment`, `OrderSystem`.
6. The gateway must not become a second source of truth: CR status/spec remain authoritative; the
   gateway keeps only a cache with a bounded staleness window.
7. Work in the single-node OrbStack lab; multi-host/TLS evidence stays an explicit gate.

## 4. Alternatives

### A. New `match-gateway` binary in the `k8s_control_plane` crate, watching CRDs

**How it works.** Add `src/bin/match_gateway.rs` plus a `gateway` module. Use `kube::runtime::watcher`
on `symbolroutes`, `symboldeployments` and `ordersystems` to build an in-memory routing table. Serve
Axum routes that proxy to `status.leaderAddr` (leader routing) and to order-system shard Services
(user-ID routing). Render a Deployment/Service/Ingress/PDB for it in the control plane.

**Pros.** Matches the documented ownership split exactly; no dependency on the manager in the
data path; reuses the crate's `model.rs`, `crd.rs`, axum, reqwest and watcher patterns; RBAC already
grants read/watch on the CRDs. Testable as a pure router plus an integration proxy test.

**Cons.** New binary and deployment to operate; watcher/cache code; must handle CR read permission
and watch restarts. Effort: medium–large.

### B. Gateway that polls the manager API instead of watching CRDs

**How it works.** The gateway periodically calls `GET /api/v1/routes` and
`GET /api/v1/order-systems/{name}` (and deployment detail) and builds the same table.

**Pros.** Much less code; no extra RBAC; easy to test against a mocked manager.

**Cons.** Puts the single-replica manager (with an `emptyDir` catalog) in the data path and adds a
poll-staleness window; contradicts the design doc's "gateway watches the CRD". Effort: small.

### C. Ingress-nginx annotations / Lua for user-ID routing only

**How it works.** Use a Lua snippet or `nginx.ingress.kubernetes.io/configuration-snippet` to parse
the JSON body's `userId` and rewrite to a shard Service.

**Pros.** No new service for the order-entry half.

**Cons.** Fragile body parsing, no CRD-aware leader routing, no canary/drain state machine, no typed
retries; couples routing to a specific Ingress controller. Effort: small but incomplete.

### D. Service mesh (Envoy/Linkerd) with a control-plane xDS/route generator

**Pros.** Centralized retries, circuit breaking, mTLS and metrics; no proxy code.

**Cons.** Large operational footprint for the lab; leader changes are still CR-driven, so a custom
control loop is needed anyway; overkill until P1 security/gateway work lands. Effort: large.

### E. Extend the manager with a proxy endpoint

**Pros.** No new binary.

**Cons.** Mixes control plane and data path; the manager is not redundant and is unauthenticated;
conflates audit/CRD writes with order traffic. Rejected.

### F. Stateless order system + user-ID-sharded database

**How it works.** Remove `userIdMin`/`userIdMax` from the order-system instances: every pod
accepts every user. Render **one** Deployment/Service/Ingress for the order system instead of four,
so the Ingress just round-robins across identical pods. Move all per-user state (future balances,
freezes, positions, idempotency keys, the demo leverage map) into a **sharded database keyed by
user ID**; the order-system pod becomes a stateless compute tier that routes by the same key at the
data layer (client-side sharding, or a proxy such as Vitess/Citus/ProxySQL, or a KV store with
built-in consistent hashing). Kubernetes desired state stays in CRDs; user account data is **data**,
never etcd.

**Pros.**
- **Eliminates the user-ID routing half of this item entirely.** No header parsing, no
  `X-User-Id`, no user→shard map in the gateway or Ingress, no `409 user_shard` errors, no
  client-side shard URL selection.
- One Deployment/Service/Ingress scales horizontally with an ordinary HPA; any replica serves any
  user, so rolling restarts and autoscaling no longer need shard affinity.
- Aligns with a standard stateless-web-tier + sharded-state architecture; per-user auth and rate
  limits become a pure edge/auth concern (P1 security workstream) rather than a topology concern.
- The BFF is almost stateless today (§2), so the migration is cheap *now*.

**Cons / risks.**
- Introduces a **new production dependency**: a database tier with schema, migrations, HA, backup,
  credentials, NetworkPolicy and encryption. That is a bigger commitment than the gateway itself
  and belongs to the P1 security/packaging workstreams.
- Every order request gains a DB round trip for user state (freeze/auth), adding latency and a new
  failure domain; needs connection pooling, timeouts, caching and a defined failure policy.
- **Atomicity**: the engine owns the book and `client_order_id` dedupe; the DB owns user state.
  A freeze plus an order placement cannot share a transaction — needs a saga/outbox and clear
  ownership of the idempotency key. The current demo sidesteps this because freeze is only an
  estimate (`docs/FEE_DEDUCTION.md` §6.6).
- **Resharding**: a `hash(user_id) % N` layout must survive user growth; needs consistent hashing
  or slot ranges and a rollout procedure.
- The demo has no DB today; proving this alternative live means adding one to the OrbStack lab
  (possible with a local container, but it is new infrastructure).

### G. Stateless order tier + Raft-native client discovery, gateway deferred (**pragmatic now**)

**How it works.** Do the cheap, low-risk part of F **without** a database: remove user-range
enforcement from the BFF, collapse the four order-system shards into one Deployment/Service/Ingress,
and let plain Ingress load balancing spread requests across identical pods. For engine leader
routing, keep the **already implemented and live** Raft-native discovery in
`MatchEngineClient::with_members` — the order system resolves the leader from `/metrics` on
transport failure, so it never needs a gateway to find a leader. No new binary, no CRD watcher, no
user-ID header/hash, no DB. Defer A's full cutover/canary/drain gateway until a real need exists.

**Pros.** Smallest change; reuses tested code paths; removes an entire class of failures
(`409 user_shard`, manual shard URL selection); one Deployment to roll and scale; no new failure
domain; testable in the lab immediately.

**Cons.** Does not implement `SymbolRoute`-driven shadow/cutover/drain or a public trading API for
non-Rust clients; the demo `leverages` map is per-pod, so with more than one replica an amend/cancel
can land on a pod without that metadata (keep HPA at 1 replica or move it behind a shared store).

## 5. Full design when the gateway is needed — A for engine routing, F for order entry

> **Current decision (2026-11-20):** §9 recommends adopting **G now** — stateless order tier plus
> the already-live Raft-native client discovery, with the gateway deferred. This section documents
> the full A/F design to build when a trigger from §9 arrives; it is not the immediate work item.

The item has two independent halves and they need not share a solution:

1. **Engine leader routing (SymbolRoute)** → **A**: a `match-gateway` binary watching the CRDs.
   Nothing else in the alternatives addresses shadow/cutover/drain or leader freshness.
2. **Order entry (user → shard)** → **F** as the target, with **A's user-ID routing as an interim
   step** if a sharded database is not in scope for this release. F removes the requirement instead
   of implementing it; A+header is the cheaper near-term answer.

Recommended sequence:

- **Phase 1 (interim):** deploy A. It proxies both `/gateway/{symbol}/...` (engine leader routing)
  and `/orders/...` (user-ID → existing shard Service), so clients stop choosing shard URLs while
  the data tier is decided.
- **Phase 2 (target):** collapse the four order-system shards into one stateless Deployment
  (alternative F), introduce the user-ID-sharded store, and drop the `/orders` user-ID routing from
  the gateway — the Ingress then round-robins across identical pods. The gateway's engine half is
  unchanged.
- **Phase 3:** retire the range fields from `OrderSystemShard` once no client relies on them
  (keep them for capacity accounting if useful).

A single `match-gateway` binary therefore has **two front doors on one listener** in phase 1,
backed by one CRD watch cache:

- `/gateway/{symbol}/...` — trading API proxy (allowlisted symbols only).
- `/orders/...` — order-entry proxy (user-ID → shard Service); unused for routing in phase 2.

### 5.1 Routing table

From the CRD watch:

| Route field | Use |
|---|---|
| `spec.activeDeploymentRef` | generation that owns new traffic in `Active` (and the new generation being validated during `Cutover`) |
| `spec.previousDeploymentRef` | old generation that still owns writes during `Cutover`/`Rollback`, then drains |
| `spec.state` | `Shadow` → no traffic; `Disabled` → no new writes; `Cutover`/`Draining` → canary/drain |
| `status.leaderAddr` | preferred leader address for the active generation |
| deployment `status.leaderAddr` | fallback when route status is empty/stale |

Leader freshness: on `5xx`/`NotLeader`/`ForwardToLeader` the gateway re-queries `GET /metrics` on
the cached member addresses (the Raft-native path already used by `MatchEngineClient`) and updates
its cache entry; bounded retries with exponential backoff. Reads and `/events` are safe to retry;
`Place` is retried only when the request carries `client_order_id`; **`Cancel`/`Amend` are never
retried blindly** — the gateway surfaces a typed `409 retry_required` with the leader hint and lets
the caller decide.

### 5.2 Route state semantics

The operator sets `activeDeploymentRef` to the **new** (shadow) generation when it patches the
route to `Cutover` (`operator.rs:1676–1696`) and only flips `state=Active` after the target is
Active and the source is Draining (`operator.rs:1847–1877`). So `previousDeploymentRef` is the
**old** generation during cutover/rollback. The gateway must read both refs, not assume "active"
means "currently serving writes".

| Route state | New writes | Reads | Canary | Drain |
|---|---|---|---|---|
| `Active` | `active` gen | `active` gen | none | n/a |
| `Shadow` | reject 503 | reject 503 | none | n/a |
| `Cutover` | **`previous` (old)** gen | `previous` gen + mirrored read-only canary to `active` (new) gen | deterministic, configurable % (default 5%) by `client_order_id`/user hash | in-flight on old gen finish |
| `Draining` | `active` gen only | `active` gen; old gen only for in-flight | none | old gen accepts no new writes; existing in-flight completes |
| `Rollback` | `previous` (old source) gen | `previous` gen | none | new gen drained |
| `Disabled` | reject 503 | allow reads | none | n/a |

**Canary is read-only.** The two generations are independent Raft groups with separate books;
splitting *writes* across them would divide state. The canary mirrors reads (`/read`,
`/linearizable_read`, `/events`) from the old generation to the new one, compares sequence/depth,
and promotes only when the new one is caught up. All writes flip atomically when the operator
moves the route to `Active`. This is why the gateway reads `previousDeploymentRef` during
`Cutover`/`Rollback` even though `activeDeploymentRef` already points at the new generation.

### 5.3 User-ID shard routing (phase 1 interim; removed by phase 2/F)

- The gateway reads `userId` from the POST body (`/demo/api/orders`, `orders/cancel`, `orders/amend`)
  or `X-User-Id` for GET/other calls. A client-supplied header must be overwritten by the auth
  edge from the verified identity — the order system re-checks the range against the body's
  `user_id`, so cross-user orders fail with `409`, but routing churn is still possible.
- A one-way **hash** of the user ID cannot be compared to the deployed `userIdMin`/`userIdMax`
  ranges; use the raw ID (or a signed token) while ranges are the sharding contract. Hashes become
  useful only if sharding itself moves to hash buckets (part of F's database design).
- Select the `OrderSystem` shard whose `[userIdMin, userIdMax]` contains the ID; proxy to
  `http://<service>.<namespace>.svc.cluster.local:<port>` (cluster DNS, not the public Ingress host,
  so the gateway does not hair-pin through itself).
- Missing/ambiguous/out-of-range ID → `400`; no default shard.
- Keep per-shard Ingress hosts unchanged for direct access and debugging.

### 5.4 Safety / allowlist

- Only paths belonging to the engine's public API (`write`, `write_batch`, `read`,
  `linearizable_read`, `follower_read`, `events`, `events/status`) are proxied to engines.
  `/init`, `/console*`, `/metrics`, `/get_linearizer`, `/read-policy/status`, `/admission/status`,
  `/health`, `/ready`, `/pretrade/status` and all `/console/api/actions/*` are never routed.
- To order systems only `/demo/api/*` and `/health`/`/ready` are proxied.
- Request/response size limits mirror the engine (1 MiB JSON body, 128-byte `client_order_id`).
- TLS, auth and rate limits remain the P1 security workstream; the gateway exposes plaintext in the
  lab and document that it must sit behind the secured edge in production.

### 5.5 Manifests and RBAC

- Checked-in `deploy/gateway.yaml`: ServiceAccount, namespaced Role (`get/list/watch` on
  `symbolroutes`, `symboldeployments`, `ordersystems`), Deployment (2 replicas,
  `maxUnavailable: 0`, readiness on `/healthz`), ClusterIP Service and an Ingress for the two hosts.
  Deploy it in the **cell namespace** (`match-eu-1`), next to the engines, so a namespaced Role
  suffices and the existing `matching.io/role=gateway` NetworkPolicy selectors admit it; no
  cluster-wide watch or ClusterRole is needed.
- No PVC, no state desired: the watch cache is rebuilt on restart.
- Optionally render the same objects from `render.rs` so image/ingress settings come from the CRD;
  decision recorded in §8.

### 5.6 Stateless order system (phase 2 / F)

- Drop `--user-id-min/--user-id-max` from `order_system/src/main.rs` and the `DemoApp` range check;
  keep the fields in `OrderSystemShard` only as capacity/accounting metadata until they are retired.
- Render one Deployment/Service/Ingress for the order system (fan-out over the existing shards)
  instead of four; keep per-shard hosts as aliases during migration, then remove them.
- Introduce a `UserStateStore` abstraction in `order_system/src/state.rs` for per-user data;
  start with an in-memory implementation for tests and a sharded implementation for production.
  The demo `leverages` map moves behind this interface so an amend/cancel routed to any pod reads
  the same metadata.
- Idempotency: the engine's `client_order_id` and `client_request_id` dedupe `Place` and `Cancel`/`Amend`; the user-state store needs its own
  idempotency key for freeze/settlement side effects, with a saga/outbox to reconcile the two.
- Never put user account data in CRDs/etcd; back up and restore it with the database's tooling,
  separate from the CRD backup path.

## 6. Failure modes

| Failure | Behavior |
|---|---|
| Leader CR status stale | gateway re-discovers from `/metrics`; bounded retries |
| Active generation has no leader (election) | fail fast `503 no_leader`; never route to a random pod |
| Route CR watch down | keep last good table with a staleness warning; reject new symbols after TTL |
| User ID out of range | `400`, no fallback shard (phase 1 only; impossible in phase 2/F) |
| Sharded user-state DB unreachable (phase 2/F) | fail closed on freeze/authorization with `503`; never place an order with unverified user state |
| `Cancel`/`Amend` transport failure | `409 retry_required` with leader hint, no blind retry |
| Canary generation behind | keep canary off; alert via status, do not promote |
| Both generations active (bad CR) | validation rejects; gateway fails closed |

## 7. Test plan

- **Unit**: route-table build from CR fixtures; state-machine table (Shadow/Disabled/Cutover/
  Draining/Rollback); user-ID range selection including boundaries and out-of-range; canary hash
  determinism; allowlist denies admin paths; retry classification (Place vs Cancel/Amend vs reads).
- **Integration**: mock engine + mock order-system servers behind Axum `oneshot`; assert routing,
  leader refresh on `NotLeader`, no retry of Cancel/Amend, canary mirroring only reads, drain.
- **Render tests**: assert the `<shard>.<ingress>` host string (currently only the Ingress count
  is checked), plus the new gateway Ingress hosts.
- **Stateless order system (F)**: unit-test the `UserStateStore` sharding function (boundary user
  IDs, resharding), the saga/outbox idempotency, and that two different pods render the same user
  state; integration-test one Deployment with N replicas where each request lands on a random pod
  and succeeds for every user.
- **Live cutover**: on OrbStack, create a shadow generation (`eurusd-g1`-style), drive orders through
  the gateway host, exercise cutover → canary → draining → active and rollback, assert no duplicate
  orders and no lost writes; drive multiple user IDs across all four shards through the single
  orders host (phase 1), then repeat after collapsing to one stateless Deployment (phase 2).
- Acceptance harness: add a `gateway` phase that builds the route table from live CRs and checks the
  two hosts and the cutover sequence. Reuse the existing `migration` phase helpers
  `wait_route_cutover()`/`wait_route_rollback()` (`scripts/operator-acceptance.sh:1609–1647`) rather
  than duplicating the CR polling.

## 8. Open questions

- Watch CRDs directly (A) or poll the manager API (B) for the lab first? Recommendation: A, with
  the manager API as a fallback only if RBAC/watch becomes a lab blocker.
- Checked-in `deploy/gateway.yaml` vs renderer-generated objects? Recommendation: check in the
  manifest first, fold into `render.rs` only if the gateway needs CR-configurable hosts. The dead
  `CellSpec.gateway.url` could seed the public host, but it is a single URL with no per-host fields.
- Should the manager gain `GET /api/v1/gateway/routes` (a gateway-consumable projection of the route
  table with user-ID shard ranges) so clients can discover the single host and their shard?
  Recommendation: yes, but after the gateway itself — today `GET /api/v1/routes` and
  `GET /api/v1/order-systems/{name}` already expose the raw inputs.
- Where does `userId` come from for non-POST calls (header vs query vs JWT claim)? Recommendation:
  `X-User-Id` for the demo, JWT claim in production (security workstream).
- **Should F land before or after the gateway?** F removes half the work but adds a database tier.
  Recommendation: ship A phase 1 first (it is self-contained and fixes both halves cheaply), then
  introduce F when real user balances/freeze are required — the BFF is stateless enough today that
  the switch is cheap now but grows costlier as demo state accumulates.
- **Database/sharding choice for F**: client-side `hash(user_id) % N` vs a proxy (Vitess/Citus) vs
  a KV store with consistent hashing. Recommendation: start with a `UserStateStore` trait and an
  in-memory implementation; pick the concrete store with the P1 packaging/security workstream.
- Canary percentage source: route annotation vs a new field? Recommendation: an annotation on
  `SymbolRoute` (no CRD schema change, schema is preserve-unknown).
- Should the gateway expose Prometheus metrics now or after P1 observability? Recommendation: a
  minimal JSON counters endpoint for the lab, formal metrics with P1 observability.

## 9. Decision matrix — what to do now

The question is not "which design is ultimately best" but "which is stable and cheap to maintain
**today**", given that TLS/auth and packaging (P1 items 1–2) are not done and the only engine client
is the order system, which already has Raft-native failover. Scores are 1 (worst) to 5 (best);
weights reflect the "stable and easy to maintain right now" criterion.

| Criterion (weight) | A: CRD gateway | B: manager poll | C: ingress Lua/header | D: service mesh | F: stateless + sharded DB | G: stateless BFF + client discovery |
|---|---|---|---|---|---|---|
| Operational stability — fewest new failure domains (25%) | 3 | 2 | 2 | 3 | 2 | **5** |
| Maintenance surface — code + config (20%) | 3 | 4 | 2 | 1 | 2 | **5** |
| Immediate effort / time to value (15%) | 2 | 4 | 4 | 1 | 1 | **5** |
| Removes the user-ID routing requirement (15%) | 2 | 2 | 2 | 2 | **5** | **5** |
| Engine leader routing for the current client (10%) | **5** | **5** | 1 | 4 | 1 | 4 (client-side) |
| Full cutover / canary / drain semantics (10%) | **5** | **5** | 1 | 4 | 1 | 1 (deferred) |
| Doesn't create work blocked by P1 security/packaging (5%) | 3 | 3 | 2 | 2 | 2 | **5** |
| **Weighted total** | **3.10** | **3.35** | **2.10** | **2.30** | **2.10** | **4.50** |

**Recommendation for now: G.** It is the only option that improves stability instead of trading
one failure domain for another: there is no new binary to operate, no CRD watch cache, no database,
no header parsing, and no plaintext public gateway created before TLS/auth exists. It reuses the
Raft-native client failover that is already tested and live, and it deletes the artificial user
sharding that currently produces `409 user_shard` failures and manual shard URL selection.

Concretely, this means:
1. Remove `--user-id-min/--user-id-max` enforcement from `DemoApp`; keep the CRD fields as
   capacity metadata only.
2. Render one order-system Deployment/Service/Ingress instead of four; keep `<shard>.<ingress>`
   hosts as aliases during migration, then retire them.
3. Keep the single `orders.<ingress>` host (today a selector) as the one order-entry URL, backed by
   that Service — plain round robin, no user-ID routing.
4. Leave engine leader routing to `MatchEngineClient::with_members` (already deployed in the order
   system); do not add a public engine gateway yet.
5. Keep the demo `leverages` map at one replica until a `UserStateStore` exists, or move it into
   engine order metadata.

**When to revisit (triggers for A or F):**
- A second client that cannot use the Rust client appears (non-Rust/gateway consumer) → build A.
- Shadow/cutover/drain must be automated rather than driven by the manager/operator CRs → build A.
- TLS/auth and image packaging land and a public trading edge is actually required → build A behind
  that secured edge.
- Real per-user balances/freeze are required → add F's sharded `UserStateStore`; once user state
  exists, a single Deployment no longer implies "any pod can serve any user" without the DB.

This recommendation deliberately **defers** the full P1 TODO item rather than claiming to satisfy
it. A remains the design for the engine half when the trigger arrives; F is the target for user
state when real account data exists. The matrix is about sequencing, not about which design wins
eventually.

## 10. Decision log

- 2026-11-20 — design requested before implementation; alternatives A–E and recommendation
  recorded. No gateway code, manifests or CRD changes committed for this item.
- 2026-11-20 — added alternative F (stateless order system + user-ID-sharded database) and revised
  the recommendation: A for engine leader routing, F as the target for order entry with A's
  user-ID routing as the phase-1 interim. F removes the user-ID routing requirement instead of
  solving it at the edge, but introduces a database tier and a cross-system idempotency problem.
- 2026-11-20 — added alternative G and a weighted decision matrix (§9) for "stable and easy to
  maintain right now". **Decision: adopt G** — collapse the order-system shards into one stateless
  Deployment and keep Raft-native client-side leader discovery; defer the gateway until a second
  client, automated cutover, or a secured public edge requires it. This deliberately defers the
  full P1 TODO item; A remains the design for the engine half and F for real user state.
- 2026-11-20 — **implemented G**: removed `DemoApp::require_user` and the `--user-id-min/--user-id-max`
  CLI flags; the renderer now emits one Deployment/Service/PDB/HPA, a base-host Ingress
  (`orders.<base>`) and per-shard alias Ingresses all targeting that one Service; the operator
  reports a single observation; the manager API and GUI link to the base host; the static
  `orders-index` redirect Ingress in `deploy/web-ingress.yaml` was removed because it shadowed the
  rendered base host. Tests updated; acceptance harness now expects one Deployment/HPA and
  `shards + 1` Ingresses.
- 2026-11-20 — order-system objects now carry `app.kubernetes.io/managed-by` in **metadata only**
  (not `spec.selector`, which is immutable), so the operator's pruner can garbage-collect renamed
  workloads; this removed all four old per-shard Deployments/Services/PDBs/HPAs/Ingresses on the
  live roll.
- 2026-11-20 — live OrbStack validation of G: one `orders` Deployment on `arm64-v5` with no
  user-range args, one `orders` Service, base `orders.match-eu-1.example/demo/` plus four alias
  Ingresses; users 1, 300, 600, 900 and 9999 (outside every old range) all placed orders on one
  host with no `409 user_shard`; aliases still serve the WebUI; `operator-acceptance.sh render`
  passes.
