# Leader-aware engine endpoints for clients (design)

Status: **decided and implemented** — C′ (Raft-native leader discovery in the reusable client) is
implemented; the optional F protocol follow-up remains open. The document keeps all alternatives
for reference.

Date: 2026-11-20.

## 1. Problem

During a rolling reboot of a match-engine shard, the order system can lose the ability to reach
the current Raft leader. The client keeps dialing a fixed pod address, and that pod may be down or
may no longer be the leader.

Evidence from the live `match-eu-1` lab and the code:

- The order-system `targets.json` is rendered with a **fixed pod-0 FQDN** for every symbol, e.g.
  `http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001` for `eurusd`
  (`k8s_control_plane/src/render.rs`, `engine = format!("http://{}-0.{}-headless...")`).
- A Kubernetes Service whose endpoint follows the leader exists **only for dedicated,
  single-process shards** (`{shard}-leader`, selector `matching.io/role=leader`). In the live
  cluster only `me-btcusdt-leader` exists; the packed shards `fx-eu-01`, `fx-eu-02` and
  `fx-sup-01` host two Raft groups per pod and have no leader Service.
- The pod-level label `matching.io/role=leader` cannot express which *process* on a packed pod is
  the leader (pod `fx-eu-01-0` can lead `eurusd` while following `gbpusd`).
- `MatchEngineClient::send_with_forwarding` caches the leader address from a `ForwardToLeader`
  response (`leader_node.addr`, a pod FQDN) and rewrites `self.leader`. Once cached, the client
  bypasses any stable Service DNS and re-pins a concrete pod, so the next rolling reboot of *that*
  pod can strand it again.
- The engine already forwards a follower `/write` to the leader, but a client whose only target is
  a dead pod never receives a `ForwardToLeader` response to act on.

## 2. What already works

- The engine client retries transient transport errors and forwards writes, batches, linearizable
  reads and events when it can talk to *any* live member (`match_engine/src/client.rs`).
- The operator already resolves a leader per process from engine observations and writes it to CR
  status (`SymbolDeployment`/shard `status.symbols[].leaderPod`, `SymbolRoute.status.leaderAddr`).
- The operator already labels leader pods for dedicated shards and maintains the `{shard}-leader`
  Service endpoint (`label_leaders`, `leader_service`).
- The operator already serializes rolling pod deletions and transfers leadership before replacing
  a pod (acceptance harness phases `roll` and `stopstart`).

## 3. Requirements and constraints

1. Order entry (and other clients) must reach the current leader through a **stable address**
   that does not change when leadership moves or a pod is replaced.
2. Must cover **packed shards** (multiple independent Raft groups per pod), not just the dedicated
   `me-btcusdt` shard.
3. Must tolerate the convergence window after an election: the Service endpoint may lag the new
   leader by one operator reconcile (~seconds), and the engine's forwarding must cover that gap.
4. Must not pin a concrete pod address in a way that survives a later reboot of that pod.
5. Must survive "no leader" (election in progress) with fail-fast `503`-style behavior, not a wrong
   write.
6. Keep changes compatible with the existing typed client (`MatchEngineClient`), the standalone
   order system, and the manager/operator test suites; no new CRDs unless clearly justified.
7. No blind retries of `Cancel`/`Amend`: `client_order_id` dedupes only `Place`
   (`TODO.md` P2 item 8 / delivery contract).

## 4. Alternatives

### A. Per-process leader Service + per-symbol pod labels (Kubernetes-native endpoint switch)

**How it works.** Render one ClusterIP Service per (shard, symbol) named
`<shard>-<symbol>-leader`, selecting `matching.io/cluster=<shard>` plus a new label
`matching.io/leader-<symbol>=true` on the pod that currently leads that Raft group. The operator
sets/removes those labels from `DeploymentObservation.leaderPod` on each reconcile; EndpointSlices
follow automatically. Order-system targets use the Service DNS. A client config flag stops the
engine client from caching the forwarded pod FQDN so it keeps using the Service DNS.

**Changes.** `render.rs` (new Service per deployment + target URLs), `operator.rs` (label
per-symbol on every pod, keep the dedicated pod-level label for compatibility), `model.rs`
(label-key helper), `match_engine/src/client.rs` (config `cache_forwarded_leader`, default true),
`order_system/src/state.rs` (use the flag), tests in all four, docs.

**Pros.** Literally meets "the Service endpoint switches to the leader"; stable DNS; no new CRDs;
uses the leader information the operator already computes; works for packed shards; failover is a
label patch rather than a client change.

**Cons / risks.** Label convergence lag after an election (mitigated by forwarding); correctness
depends on the operator writing labels for *every* symbol on *every* pod (a missed removal leaves a
stale endpoint); per-symbol labels multiply (small); a leader Service with zero endpoints during an
election makes clients fail fast — correct but needs the order system to wait for pretrade readiness
as it already does; requires the client change to avoid re-pinning a pod FQDN.

**Effort.** Medium (4 crates/files + tests + rollout).

### B. Stable per-shard Service selecting all voters + engine-side forwarding

**How it works.** Render one ClusterIP Service per shard selecting all engine pods of the cluster
(no leader label), and point order-system targets at it. Any live member accepts the request and
the engine forwards to the leader internally. Same client no-cache flag as A.

**Changes.** `render.rs` (one Service per shard + target URLs), client flag + order system flag.
No operator changes.

**Pros.** Smallest robust change; Service always has endpoints while any voter is up; simpler than
per-symbol labels; leader changes need no Kubernetes object update at all.

**Cons / risks.** Does not literally "switch the endpoint to the leader" — every request may hop
through a follower, adding one HTTP round trip and dependence on forwarding behavior; a follower
that is not yet caught up can return stale read results for non-linearizable reads; the Service
DNS hides which pod served the request, which complicates debugging; still needs the client
no-cache flag.

**Effort.** Small.

### C. Client-side multi-target retry

**How it works.** Render a target list containing every pod FQDN of the shard (or resolve the
headless Service), and have `MatchEngineClient` try the next address on transport failure before
giving up. Combined with the engine's forwarding, any reachable pod suffices.

**Changes.** `match_engine/src/client.rs` (target list, retry rotation, optional re-resolution),
`order_system/src/state.rs` and the rendered targets. No operator/renderer Service changes.

**Pros.** No Kubernetes endpoint machinery; explicit control over retry/backoff; works even if
label updates are broken; can keep leader affinity/caching after a successful leader is found.

**Cons / risks.** Pushes topology into every client; DNS/pod list must be kept fresh as pods are
replaced; retry-on-transport-error can duplicate non-idempotent requests (must respect the
`client_order_id` contract); more client code and more client tests; does not satisfy the "K8S
endpoint" requirement.

**Effort.** Medium.

### D. Resolve the leader from CR status / manager API

**How it works.** Clients (or a small sidecar) read `SymbolRoute.status.leaderAddr` /
`SymbolDeployment.status.leaderPod` via the Kubernetes API or the manager's cluster API and dial the
current leader pod directly. A sidecar could maintain a local `/etc/hosts` or a loopback listener.

**Changes.** New resolver component or client logic; RBAC/API access for clients; refresh logic.

**Pros.** Uses the authoritative leader record that already exists; no label churn; can expose
leader identity to operators/logging.

**Cons / risks.** Adds a dependency on the API server/manager in the data path (or a sidecar per
pod); stale status during elections; more moving parts; client-side resolution logic duplicated in
every language; not a Kubernetes Service endpoint.

**Effort.** Medium–large.

### E. Service mesh / proxy with leader-aware routing

**How it works.** Deploy Envoy/Linkerd (or a small custom proxy) in front of the engine pods; the
control plane programs the leader endpoint from CR status or engine metrics; clients dial one proxy
address.

**Changes.** New infrastructure (sidecars or gateway), control-plane integration, mTLS/observability
configuration.

**Pros.** Centralized routing, retries, circuit breaking and metrics; no client code changes needed
for routing.

**Cons / risks.** Large operational footprint for a small lab; new failure domain; needs careful
handling of Raft stream/long-poll traffic; overkill until the gateway work (TODO P1 item 4) lands.

**Effort.** Large.

### F. Engine returns a typed leader hint; client follows it

**How it works.** Change the follower `/write` response from `500` to `503` with a stable leader
endpoint (service DNS rather than pod FQDN) and have clients refresh their target from that hint.
This is the "typed leader redirect" already noted in `docs/KUBERNETES_CONTROL_PLANE.md` and
`TODO.md` P2 item 8.

**Changes.** Engine HTTP layer and error type; client target-refresh logic; order-system updating.

**Pros.** Protocol-level solution independent of Kubernetes; also fixes typed errors for gateways;
can carry leader id/addr for observability.

**Cons / risks.** Does not help when the only target pod is dead (no response to read the hint
from) — must be combined with B, C or A; wire/API change affecting all clients; needs careful
backward compatibility.

**Effort.** Medium.

### G. Accept the current design and make failures explicit

**How it works.** Keep pod-0 targets; make the order system surface a clear "engine leader
unavailable" state and require operators to wait for the roll to finish. Document that order entry
is briefly unavailable during rolling reboots.

**Pros.** Zero code changes.

**Cons / risks.** Does not meet the stated requirement; the order desk silently cannot trade
during a roll; poor production experience.

**Effort.** None.

## 4a. Raft-native leader discovery already exists (preferred building block)

The engine exposes leader/membership state on its HTTP port, and `MatchEngineClient::metrics()`
already calls it:

- `GET /metrics` — the flattened `RaftMetrics` payload. Live example from `fx-eu-01-0`:
  `current_leader: 1` and
  `membership_config.membership.nodes["1"].addr = "fx-eu-01-1...svc.cluster.local:21001"`.
  That is exactly a Raft-derived leader id plus the HTTP address to dial.
- `GET /console/api/cluster` — a purpose-built, versioned (`schema_version: 1`) snapshot with
  `leader_id: 1` and `nodes: [{ node_id: 1, addr: "...:21001", reachable: true, ... }]`, i.e. the
  same information in a smaller payload. Documented as "Local node plus peer snapshots from the
  current membership".
- `GET /health` and `GET /console/api/node` — `leadership.leader_id`, `state`, `is_leader`, `term`;
  small but lack the address mapping, so `/metrics` or `/console/api/cluster` is needed to resolve
  it.

This means the client can do **one read-only discovery call** to any reachable member and dial the
leader directly — no blind write retries against members one by one. Those endpoints are current
Raft state, not cached deployment configuration, so they are the authoritative source for the
leader. Read-only discovery is safe to retry on another bootstrap address if the first member is
down; the existing `ForwardToLeader` path remains the fallback for the election/lag window.

Caveats: `/metrics` is a heavier payload (called rarely, on leader change/failure); `current_leader`
can be `None` or briefly stale during an election, so the client must re-query and keep the
forwarding fallback; membership addresses are the HTTP addresses recorded at `/init` (stable pod
FQDNs in Kubernetes).

## 5. Comparison

| Alternative | Stable leader DNS | Packed shards | Survives dead target pod | New CRD | Effort |
|---|---|---|---|---|---|
| A — per-process leader Service | yes | yes | yes | no | medium |
| B — all-voter Service + forwarding | no (any live voter) | yes | yes | no | small |
| C — client multi-target retry | no | yes | yes | no | medium |
| D — resolve from CR status/API | no | yes | yes | no | medium–large |
| E — service mesh / proxy | yes | yes | yes | no | large |
| F — typed 503 leader hint | only with B/C/A | yes | only with B/C/A | no | medium |
| G — explicit failure | no | no | no | no | none |

## 6. Recommendation

**C′ — Raft-native leader discovery (a refinement of C), with F as an optional protocol follow-up.**

Rationale: the engine already publishes the current leader and the membership address map through
`/metrics` (and `/console/api/cluster`), and `MatchEngineClient::metrics()` already fetches it. Use
that as the primary routing mechanism and keep Kubernetes out of the leader-switch path entirely.

Flow:

1. Bootstrap with any reachable member address (the shard's stable pod FQDNs; a small
   `targets.json` change — **no new Service, no label, no EndpointSlice, no operator change**).
2. Call `GET /metrics` (or `/console/api/cluster`) on that member; read `current_leader` and resolve
   `membership_config.membership.nodes[leader_id].addr` to the leader's HTTP URL.
3. Dial the leader directly for writes. `ForwardToLeader` responses still update the cached leader
   as today; the forwarding path covers the brief election/lag window.
4. On a transport failure, **retry the read-only discovery call** on the next bootstrap address,
   then re-dial the newly reported leader. Never retry a write blindly across members —
   discovery is idempotent, writes are not necessarily.
5. Treat the cached leader as a hint, not a pin; re-discover after a reboot of that pod.

This is strictly better than plain C: normally one read-only discovery call plus a direct write, no
write hop through a follower, and no per-member write retries. It keeps existing delivery semantics
(at-least-once; `Place` safe with `client_order_id`; `Cancel`/`Amend` no-blind-retry, TODO P2 item 8),
works identically for packed and dedicated shards, and introduces no new Kubernetes objects.

`/console/api/cluster` is the nicer payload (`leader_id` + `nodes[].addr` + reachability,
`schema_version: 1`), but `/metrics` is the canonical documented API and already has a typed client
method — use it in the client, keep the console API for debugging/gateways.

**Why not the others:**

- **A** moves leader tracking into Kubernetes labels/EndpointSlices — more failure modes and
  reconcile lag for something Raft already knows. Keep it only if a future gateway needs a Service
  DNS for the leader.
- **B** needs a new Service per shard; unnecessary because the existing headless Service already
  provides stable pod DNS and the client can hold the member list directly.
- **D** adds an API-server/manager dependency to the data path and stale-status handling.
- **E** is disproportionate operational cost until the gateway work lands.
- **G** does not solve the problem.

**Follow-up (F):** return a typed `503` + leader hint so non-Rust clients/gateways can refresh
deterministically. Not required for the order system, which already parses `ForwardToLeader`; it
also serves TODO P2 item 8.

**Required config change:** render a member-address list in `targets.json` (the shard's engine pod
FQDNs, derived from the existing deployment/slot layout). No new Kubernetes objects.

## 7. Test plan

- Unit tests: leader address is resolved from a mocked `/metrics` (or `/console/api/cluster`)
  payload; a null/stale `current_leader` triggers re-discovery; discovery retries on another
  bootstrap address and stops when the list is exhausted; a cached leader is a hint, not a pin;
  non-idempotent operations are never blind-retried.
- Integration tests: four-voter cluster — kill the leader, kill the configured bootstrap pod,
  restart a formerly pinned pod, and verify writes keep succeeding after re-discovery; packed shard
  where pod 0 leads one symbol and follows another; election in progress (`current_leader` null)
  returns a clean `no_leader`/`503`; rolling reboot of every ordinal in turn.
- Live lab: `fx-eu-01`, `fx-eu-02` and `fx-sup-01` (packed) plus `me-btcusdt` (dedicated); replay a
  fixture batch through `scripts/replay-order-system.py` during a pod roll and assert no duplicate
  or rejected orders.

## 8. Open questions

- Use `/metrics` (canonical, typed client method already exists) or `/console/api/cluster`
  (smaller purpose-built payload, versioned)? Recommendation: `/metrics` for the client, console API
  for debugging/gateways.
- Should the bootstrap member list be rendered into `targets.json`, or derived at runtime from the
  existing headless Service DNS? Rendering is deterministic; runtime DNS adapts to pod replacement
  without a config roll.
- Discovery retry budget: bounded (a few read-only calls) so a dead cluster fails fast rather than
  blocking the order desk.
- Should the cached leader address be persisted across process restarts? Recommended no —
  re-discover on startup.
- Keep the existing dedicated `{shard}-leader` Service as-is for other consumers? Recommended yes;
  it is independent of this solution and already works.

## 9. Decision log

- 2026-11-20 — alternatives documented; initial per-process Service patch reverted.
- 2026-11-20 — selected **C′ (Raft-native leader discovery via `/metrics`)**, optional **F (typed
  leader hint)** as a follow-up. Rationale: Raft already owns leader identity and publishes it on
  the engine HTTP API; Kubernetes leader labels/Services add complexity and reconcile lag without
  adding resilience.
- 2026-11-20 — **Implemented C′** in the reusable client and wired the order system to it:
  - `match_engine/src/client.rs`: `MatchEngineClient::with_members(Vec<(NodeId, addr)>, config)`
    stores the full Raft member set; the cached leader is a hint, not a pin. On a transport error
    (`Timeout`/`Unreachable`/`Network`) the client re-discovers the leader with a read-only
    `GET /metrics` on the members (skipping the failed address), resolves
    `membership_config...get_node(current_leader).addr`, and retries at the new leader. Edge
    `429`/`5xx` responses do not trigger re-discovery. `new`/`with_config` remain single-target and
    backward compatible.
  - `order_system/src/state.rs`: optional `engineMembers` in `TargetSpec` (`nodeId` + `addr`,
    camelCase); when present the backend uses `with_members`, otherwise it keeps the single-target
    client.
  - `k8s_control_plane/src/render.rs`: renders `engineMembers` for every order-system target,
    enumerating `0..shard.spec.replicas` as `raft_id_offset + ordinal` and the stable per-ordinal
    headless-Service FQDNs. The existing `engine` (pod-0) field is unchanged.
  - Tests: `client_discovers_new_leader_after_target_dies` (engine integration),
    `target_engine_members_enable_raft_failover_client` (order-system unit), and updated
    `order_system_renders_hpa_and_rolling_shards` (control-plane render). Full `cargo test`,
    `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check` pass in all three
    crates.

### Live OrbStack validation (2026-11-20)

Deployed `matchengine-k8s-control-plane:operator-v89` (renderer) and
`matchengine-order-system:arm64-v4` (client) on the OrbStack cluster. The operator re-rendered
`orders-config` with four `engineMembers` per target (verified for all 7 targets).

- **Hard leader kill**: hard-deleted `fx-eu-01-1` (leader for both `eurusd` and `gbpusd`) while
  driving orders through all four order-system shards. 28/28 writes succeeded before the kill;
  the 4 in-flight writes at the kill returned `502/transport` (stale `current_leader` in the
  followers' `/metrics` during the election), then **100/100 writes succeeded with no client
  restart**. All four order-system pods logged `re-discovered Raft leader` with the new pod's
  address.
- **Operator rolling upgrade**: rolled `fx-eu-01` one pod at a time to a digest-pinned image
  while driving ~10 writes/s through all four shards. **1,480/1,480 writes succeeded** across
  the whole roll, including while pods were terminating and being replaced. The roll completed
  4/4 updated, 4/4 Ready; the shard image was restored to the original tag afterwards.
- Observed side issue, fixed separately the same day: engine pods under sustained order load
  could linger in `Terminating` past the 60 s grace period until force-deleted, because tonic's
  graceful gRPC shutdown waited for long-lived peer `AppendStream` connections to close. The
  node now caps that drain at 5 s and aborts the server task on expiry; a second live roll with
  the fixed image terminated each pod in ~6 s and completed in ~1 minute, with orders continuing
  to succeed throughout.

Still outstanding: the optional F typed `503` + leader hint for non-Rust clients/gateways.
