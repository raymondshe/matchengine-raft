# Operational console (WebUI) design

Status: **implemented** — the embedded `/console/` UI, node/cluster APIs, guarded
transfer/election/snapshot/purge/runtime-config actions and the gRPC `TransferLeader` RPC are in
place. Streaming updates, sparklines and guarded membership/bootstrap actions remain future work.

Last updated: 2026-11-10.

Related documents: [`AGENTS.md`](../AGENTS.md), [`docs/PERFORMANCE.md`](./PERFORMANCE.md),
[`docs/MATCH_RESULTS.md`](./MATCH_RESULTS.md), [`docs/MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md).

## 1. Summary

Add a small operational console served by the existing Actix Web HTTP server on every
matching-engine node. The console is an internal SRE/operator tool, not a public trading UI. It
shows node health, Raft leadership and replication, durable storage state, event retention and
market-data status, and offers guarded Raft actions such as leadership transfer and a best-effort
election request.

The console is a static, dependency-free HTML/CSS/JavaScript application embedded in the node
binary. It talks only to the node that served it; a server-side cluster endpoint aggregates peer
status so the browser does not need CORS, peer credentials or arbitrary URL access.

The existing HTTP listener remains plaintext and unauthenticated by design. TLS, client
authentication and any operator authorization belong to the gateway in front of the node. The
console inherits that contract and must not be exposed directly to an untrusted network.

## 2. Goals and non-goals

### Goals

- Give an on-call operator a single page to answer:
  - Is this node alive and ready?
  - Who is the Raft leader, and is the cluster stable?
  - Are followers/learners keeping up?
  - Is the WAL, state machine and snapshot frontier healthy?
  - Is the event window retaining enough history?
  - Is the market-data feed enabled and publishing?
- Show cluster-wide status from one node, including unreachable peers, without requiring a
  separate monitoring stack.
- Support graceful leadership transfer between voters, with confirmation, target validation and a
  clear pending/failed state.
- Provide a best-effort election trigger when graceful transfer is unavailable.
- Keep the console self-contained: no Node.js build step, no CDN, no external fonts or charts.
- Keep the backend read-mostly and reuse existing health/metrics/read/event endpoints where
  possible.

### Non-goals

- Public internet exposure. TLS and authentication stay at the gateway.
- Order entry, cancel/amend, or a trading terminal. The existing `/write` API remains the trading
  edge; the console does not become a second order-entry path.
- Replacing Prometheus/Grafana or long-term metrics storage. The console is an instantaneous
  operational view; it does not retain history.
- Multi-cluster management. One process/cluster trades one symbol; the console manages the cluster
  that served it. A gateway can link to several consoles.
- Membership changes, node shutdown or arbitrary destructive storage actions in the first
  version. `purge-log` is the one local log-deletion action and is guarded by a snapshot boundary.
- A JavaScript framework or a frontend build pipeline in the first version.

## 3. Constraints and current state

- The node already runs Actix Web on `--http-addr` and serves `/health`, `/ready`, `/metrics`,
  `/read`, `/events`, `/events/status` and the admin routes. The console shares that listener and
  port.
- The HTTP and gRPC listeners are plaintext and unauthenticated. The supported deployment is
  loopback or a trusted private network behind a gateway.
- One process/cluster trades exactly one symbol. The console therefore has no symbol/cluster
  selector in the first version.
- `GET /health` already reports the gRPC listener flag, Raft role/leader, WAL frontier and
  state-machine applied state.
- `GET /metrics` returns the raw OpenRaft `RaftMetrics` JSON, wrapped in a `Result`. The console
  should not couple its UI to that unstable internal shape; it should expose a stable,
  console-specific schema and derive it from `RaftMetrics`.
- OpenRaft exposes `raft.trigger().transfer_leader(to)`, `raft.trigger().elect(pre_vote)` and
  `raft.trigger().snapshot()`. The current gRPC transport does **not** implement the
  `TransferLeader` RPC, so `transfer_leader()` cannot complete a graceful transfer yet. The design
  includes adding that RPC as a backend prerequisite.
- The existing admin endpoints (`/init`, `/add-learner`, `/change-membership`) already have the
  same trust level as the console actions. The gateway is responsible for protecting the whole
  HTTP listener.

## 4. Users and workflows

| User | Workflow |
|---|---|
| On-call SRE | Open the console, confirm the leader, spot a lagging or unreachable peer, check WAL/snapshot health, inspect event retention and the market-data gate. |
| Operator doing maintenance | Select a caught-up voter, transfer leadership away from the node being drained, watch the term/leader change, then proceed with maintenance. |
| Incident responder | Check whether the node is ready, whether it knows a leader, how far applied/committed/snapshot indices diverge, and whether the event window is near eviction. |
| Developer | Inspect membership, replication indices, heartbeat ages and the latest applied log id without attaching a debugger. |

Primary flow:

1. Open `http://<node-http-addr>/console/`.
2. The overview loads the local node immediately, then the cluster aggregate.
3. The operator sees the leader, all voters/learners, per-peer lag and any unreachable peers.
4. For a leadership change, the operator opens **Actions**, picks a voter, confirms the target and
   current term, and watches the cluster view converge on the new leader.

## 5. Information architecture

### 5.1 Overview

- Cluster summary cards: current leader, term, number of reachable nodes, maximum replication lag,
  number of nodes not ready.
- Node cards, one per member:
  - node id, HTTP/gRPC addresses, voter/learner role
  - healthy/ready/grpc-listening chips
  - Raft state (`Leader`, `Follower`, `Candidate`, `Learner`, `Shutdown`)
  - last log / applied / committed / snapshot / purged indices
  - lag versus the leader's last log (leader view)
  - WAL frontier and state-machine applied index
  - warning badges for unreachable, not ready, lagging, or storage error
- A banner for stale data, partial peer failures, or a cluster without a leader.

### 5.2 Raft

- Role, term, vote and current leader.
- Membership table: voters, learners, node addresses.
- Leader-only replication table: per-node matched index, lag, last heartbeat age.
- Log frontier: last log, local committed, cluster committed, last applied, snapshot, purged.
- Process-local transport counters: heartbeat/replication `AppendStream` streams and requests,
  snapshot send/receive count with average and max transfer time.
- Recent leadership/election state transitions if the UI keeps a short client-side history
  (optional; not persisted).

### 5.3 Storage and node

- WAL `last_log_id` and `last_purged_log_id`.
- State-machine `last_applied`.
- Snapshot index and purged index.
- Event window: retained count, oldest/latest event ids, eviction total, retention limit.
- Market-data status: enabled/gate state, counters and last sequence, from
  `/read marketdata_status`.
- Order-book scalar views: sequence, spread and last trade price from `/read`; avoid fetching the
  full order list in the console.
- Node identity, version, uptime, HTTP/gRPC addresses and data-directory paths (diagnostics only).

### 5.4 Events

- Read-only view of the replicated match-event log.
- Leader-only, because `POST /events` is leader-gated.
- Cursor pagination using the existing `(log_index, ordinal)` event id and `next_after`.
- Pause/follow control, page size 1–1000, client-side filter by event kind/side.
- If the local node is not the leader, show a banner with the leader's console link instead of
  silently returning an error.

### 5.5 Actions

First version:

- **Transfer leadership** to a selected voter. Preferred graceful operation.
- **Request election on this node** using OpenRaft Pre-Vote. Best-effort fallback; useful when
  graceful transfer is unavailable or the operator explicitly wants a specific node to campaign.
- **Trigger snapshot** on this node. Low-risk diagnostic/maintenance action; still confirmed.
- **Purge this node's log**. Requires an existing snapshot and an index at or below the snapshot
  boundary (blank defaults to the snapshot index). The action is local to the serving node;
  OpenRaft clamps to the snapshot and may delay the purge while a replication task is reading the
  entries. It never purges another node's log.
- **Runtime switches**. Node-local OpenRaft `tick`, `heartbeat`, `elect` and `pre_vote` toggles.
  Refused on the leader so a maintenance toggle cannot stop leases/heartbeats for the cluster;
  transfer leadership first. Every field is optional and at least one is required.
- Action history (last N actions) with timestamp, action, target, result, detail and error. This
  starts as an in-memory ring buffer and is not durable.

Later, with a separate design:

- Add learner, change membership, initialize cluster, download diagnostics bundles.

### 5.6 Diagnostics

- Copy/download the current cluster snapshot as JSON.
- Show the exact health/metrics payloads for a node, with a "copy JSON" button.
- No arbitrary shell access, no file browser and no log tail in the first version.

## 6. Backend design

### 6.1 Route map

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/console` | Redirect to `/console/` |
| `GET` | `/console/` | Embedded HTML shell |
| `GET` | `/console/app.js` | Embedded JavaScript |
| `GET` | `/console/app.css` | Embedded CSS |
| `GET` | `/console/api/node` | Stable local node snapshot |
| `GET` | `/console/api/cluster` | Local + peer snapshots |
| `POST` | `/console/api/actions/transfer-leader` | Graceful leadership transfer |
| `POST` | `/console/api/actions/elect` | Request a local election with Pre-Vote |
| `POST` | `/console/api/actions/snapshot` | Trigger a local snapshot |
| `POST` | `/console/api/actions/purge-log` | Purge local log up to a snapshot-covered index |
| `POST` | `/console/api/actions/runtime-config` | Node-local OpenRaft runtime switches |
| `GET` | `/console/api/actions` | Recent in-memory action records |

The console routes live under `/console` so a gateway can expose or block the whole surface with
one rule. Existing `/health`, `/metrics`, `/read`, `/events` and `/events/status` remain unchanged
and can still be used by external monitoring.

Module layout:

- `src/network/console.rs` — routes, cluster aggregation, action validation and embedded assets.
- `web/console/index.html`, `web/console/app.js`, `web/console/app.css` — static UI.
- `tests/cluster/test_console.rs` — aggregate, action and transfer integration tests.

### 6.2 Node snapshot

`GET /console/api/node` returns a stable schema, not raw OpenRaft internals. Proposed shape:

```json
{
  "schema_version": 1,
  "node_id": 1,
  "version": "0.0.6",
  "started_at_ms": 1762000000000,
  "http_addr": "127.0.0.1:21001",
  "grpc_addr": "127.0.0.1:22001",
  "healthy": true,
  "ready": true,
  "grpc_listening": true,
  "leadership": {
    "state": "Leader",
    "is_leader": true,
    "leader_id": 1,
    "term": 7,
    "vote": "..."
  },
  "raft": {
    "last_log_index": 42,
    "local_committed_index": 42,
    "cluster_committed_index": 42,
    "last_applied_index": 42,
    "snapshot_index": 20,
    "purged_index": 10,
    "voters": [1, 2, 3],
    "learners": [4],
    "replication": [
      { "node_id": 2, "matched_index": 42, "lag": 0, "last_ack_ms_ago": 18 }
    ]
  },
  "transport": {
    "heartbeat_streams": 2,
    "heartbeat_requests": 8,
    "replication_streams": 6,
    "replication_requests": 120,
    "snapshot_sent": 1,
    "snapshot_sent_bytes": 4096,
    "snapshot_sent_avg_micros": 34418,
    "snapshot_sent_max_micros": 39963,
    "snapshot_received": 0,
    "snapshot_received_avg_micros": 0
  },
  "storage": {
    "wal_last_index": 42,
    "wal_last_purged_index": 10,
    "state_machine_applied_index": 42,
    "snapshot_index": 20
  },
  "events": {
    "retained_events": 1234,
    "oldest_event_id": { "log_index": 30, "ordinal": 0 },
    "last_event_id": { "log_index": 42, "ordinal": 5 },
    "evictions_total": 0,
    "retention_limit": 262144
  },
  "market_data": {
    "enabled": true,
    "leader_gate_open": true,
    "last_market_seq": 42
  },
  "capabilities": ["console_v1", "transfer_leader", "purge_log", "runtime_config"],
  "errors": []
}
```

Notes:

- Indices are `Option<u64>` in Rust and `null` in JSON when absent.
- `vote` is a display string derived from `RaftMetrics::vote`; the UI does not parse it.
- `replication` is present only on the leader; a follower reports an empty array and a note.
- `heartbeat` ages are derived from `RaftMetrics::heartbeat`; absent entries become `null`.
- `capabilities` lets the cluster view disable leader transfer until every voter supports the new
  RPC, which matters during a rolling upgrade.
- The console must tolerate unknown fields and additive changes (`schema_version` remains `1` for
  additive changes).

Implementation note: extract the health collection into a reusable `NodeSnapshot` collector so
`/health`, `/console/api/node` and the cluster aggregate share the same storage/metrics reads.

### 6.3 Cluster aggregate

`GET /console/api/cluster` returns the local node plus every member from the local membership
configuration.

```json
{
  "schema_version": 1,
  "generated_at_ms": 1762000000123,
  "local_node_id": 1,
  "leader_id": 1,
  "term": 7,
  "nodes": [
    {
      "node_id": 1,
      "addr": "127.0.0.1:21001",
      "role": "voter",
      "local": true,
      "reachable": true,
      "latency_ms": 0,
      "snapshot": { "...": "NodeSnapshot" },
      "error": null
    },
    {
      "node_id": 2,
      "addr": "127.0.0.1:21002",
      "role": "voter",
      "local": false,
      "reachable": true,
      "latency_ms": 3,
      "snapshot": { "...": "NodeSnapshot" },
      "error": null
    }
  ],
  "warnings": []
}
```

Aggregation rules:

- Before initialization, membership is empty. The cluster view shows the local node with an
  "uninitialized" banner and bootstrap instructions; it does not guess peers.
- Addresses come only from the local membership configuration. The endpoint never accepts a
  user-supplied URL, so it cannot be used as an SSRF proxy.
- Fetch peer `GET /console/api/node` with a shared `reqwest::Client`, a 500–750 ms timeout and at
  most 8 concurrent requests. A three-node cluster should complete well under 100 ms on a LAN.
- A peer failure is a partial result: `reachable: false`, `error` populated, and a warning in the
  response. It must not fail the whole console.
- The first implementation reuses one shared `reqwest::Client` but does not cache the aggregate;
  add a short cache/single-flight if console polling load becomes significant.
- For rolling upgrades, a peer that lacks `/console/api/node` can be shown from `/health` plus
  `/metrics`, or marked "console API unsupported"; the UI must not crash.
- If this node is a follower, its `replication` table is empty. The UI uses the leader's node
  snapshot when available for lag; otherwise it shows "leader view unavailable".

### 6.4 Leader switching

There are two operations, with different guarantees.

#### Graceful transfer (preferred)

Add a `TransferLeader` RPC to the gRPC transport:

```proto
rpc TransferLeader(TransferLeaderCall) returns (TransferLeaderResponse);

message TransferLeaderCall { bytes payload = 1; }
message TransferLeaderResponse { bytes payload = 1; }
```

- The client implements `RaftNetworkV2::transfer_leader` with the same postcard envelope and
  status mapping as `append_entries`/`vote`.
- The server decodes `TransferLeaderRequest<MatchEngineTypeConfig>`, calls
  `raft.handle_transfer_leader(req).await`, and encodes
  `Result<TransferLeaderResponse<MatchEngineTypeConfig>, RaftError<...>>`.
- The console action endpoint runs only on the current leader:
  1. Read `RaftMetrics`.
  2. Reject if `current_leader != Some(self.id)` with `409 Conflict` and the current leader id.
  3. Reject self-target, unknown targets, and learners with `400`/`409`.
  4. Reject a target whose leader-known matched index is behind the last log with `409 Conflict`;
     OpenRaft would otherwise reject it with `LogNotFlushed`.
  5. Call `raft.trigger().transfer_leader(target).await`.
  6. Return `202 Accepted` immediately. The transfer is asynchronous.
- The UI polls the cluster snapshot for up to ~15 seconds and reports:
  - `pending`: still the same leader/term
  - `changed`: a new leader is visible
  - `failed/timeout`: current leader/term is unchanged; the operator can inspect logs and retry
- Do not retry automatically. A transfer can be rejected because the target's log is not flushed
  or its vote changed; retrying in a loop adds noise and can disrupt the cluster.

#### Best-effort election request (fallback)

- `POST /console/api/actions/elect` runs on the node that should campaign.
- Backend validates that the node is a voter (not a learner), then calls
  `raft.trigger().elect(true)` (Pre-Vote enabled).
- Pre-Vote avoids incrementing the term when the target cannot win or a healthy leader still has a
  quorum. This makes the operation safe to offer as a fallback.
- A forced election (`pre_vote = false`) is not exposed in the first version; it can step down a
  healthy leader and should require a separate, explicit design.
- The cluster view may offer "request election on target" by server-side POSTing to the target's
  `/console/api/actions/elect` endpoint, but only if the target advertises the capability. The
  target must be reachable; otherwise the action fails with `502/504`.

### 6.5 Action safety

- All actions require `Content-Type: application/json` and a custom header such as
  `X-Console-Action: 1`. The custom header forces a CORS preflight for cross-origin requests; the
  console does not enable CORS, so a malicious web page cannot silently POST to a loopback node.
- Reject an `Origin` header that is present and does not match the request `Host` (or a configured
  allowed origin for a reverse proxy).
- Serialize actions with a process-wide async mutex / `AtomicBool`: one Raft action at a time,
  returning `429 Too Many Requests` for a concurrent action.
- Log every action with `tracing::info!`: action, target, current leader, term, remote address,
  result and error. Do not claim an authenticated operator identity; the gateway owns that.
- The console is not a new trust boundary. Existing `/init` and `/change-membership` are equally
  powerful, so the gateway must already protect the whole HTTP listener.

#### Purge-log and runtime-config constraints

- `purge-log` is local maintenance, not a cluster mutation: it calls
  `raft.trigger().purge_log(upto)` on the serving node only. It requires a completed snapshot and
  rejects `upto` above `RaftMetrics::snapshot.index` with `400`; a missing snapshot is `409`.
  Blank `upto` uses the snapshot index. OpenRaft clamps the request and may delay the purge while
  a replication task still reads the entries, so the action only schedules the purge.
- `runtime-config` toggles OpenRaft's `tick`, `heartbeat`, `elect` and `pre_vote` switches on the
  serving node. At least one field is required (`400` otherwise). It is refused with `409` when
  this node is the leader: disabling ticks or heartbeats on the leader would stop leases and
  replication for the whole cluster. Transfer leadership first, then tune the follower. These
  switches are in-memory and reset on restart.
- Both actions use the same `X-Console-Action`/origin check, process-wide action lock and action
  history as transfer/election/snapshot; the history records the purge index or the exact runtime
  changes in `detail`.

### 6.6 Static assets

- Store assets in `web/console/index.html`, `web/console/app.js`, `web/console/app.css` and embed
  them with `include_str!` in `src/network/console.rs`. This keeps the binary self-contained and
  avoids runtime path/package issues.
- No external CDN, fonts, images or analytics. The console must work on an isolated network.
- Headers:
  - `Content-Type` with UTF-8 for HTML/JS/CSS.
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self';
    connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self';
    frame-ancestors 'none'`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `Cache-Control: no-store` for API responses; `no-cache` for assets during the first version.
- Serve `/console` as a redirect to `/console/` so relative asset paths are stable.

## 7. Frontend design

### 7.1 Technology

- Vanilla HTML, CSS and ES modules. No framework, bundler, npm or CDN.
- One page with client-side tabs; the browser keeps a small amount of state.
- Use `fetch` with `AbortController` timeouts and a shared polling loop.
- Render text with `textContent`/DOM APIs, not `innerHTML` from server data, to avoid XSS.
- Keep the JS small enough to audit; target < 30 KiB minified-equivalent.

### 7.2 Layout

A static HTML layout preview is at
[`OPERATIONAL_CONSOLE_LAYOUT.html`](./OPERATIONAL_CONSOLE_LAYOUT.html). It uses illustrative data
and no JavaScript; it is a visual reference for the implementation, not production code.

- Top bar: product/version, node id, cluster leader, health/ready chips, auto-refresh toggle,
  "last updated" age.
- Left navigation: Overview, Raft, Storage, Events, Actions, Diagnostics.
- Main panel: cards and tables; no chart library in the first version.
- Responsive down to a laptop screen; no mobile-first requirement for an internal console.
- Color is never the only signal: pair red/yellow/green with text labels and icons.

### 7.3 Polling and freshness

- Poll `/console/api/cluster` every 2 seconds while the tab is visible.
- Pause polling when `document.hidden`; refresh immediately on `visibilitychange`.
- On network failure, back off 2s → 4s → 8s, keep the last successful data visible and show a
  stale banner with the failure reason.
- Show the server's `generated_at_ms` and the browser's fetch time; do not pretend data is live
  when a peer is unreachable.
- Actions trigger an immediate refresh, then continue the normal poll until the leader/term
  settles or the action timeout expires.

### 7.4 Action UX

- Transfer:
  - Target dropdown lists voters only, with current role and lag.
  - Targets that are unreachable or do not report zero replication lag are disabled by default;
    the operator can refresh and retry after the target catches up.
  - Confirmation dialog shows current leader, target, term and target lag.
  - Require an explicit checkbox ("I understand this changes the Raft leader").
  - After submission, show a pending timeline and the live leader/term; never hide the old leader
    until a new one is observed.
- Election request:
  - Label it "Request election on this node (Pre-Vote, best effort)".
  - Explain that a healthy leader may reject the request.
- Snapshot:
  - Show the current snapshot/applied indices before and after.
- Purge:
  - Show the current snapshot and purged indices, and default the input to the snapshot index.
  - Require an explicit checkbox; explain that only local log entries covered by the snapshot are
    deleted and that OpenRaft may delay the purge while replication is in flight.
- Runtime config:
  - Disable submission while this node is the leader and explain that leadership must move first.
  - Each switch is "unchanged/enable/disable"; require a checkbox and show the exact changes in
    the result line.
- Disable actions when the node is unhealthy, not a voter, not the leader (for transfer), or the
  required capability is missing.

## 8. Data sources

| UI element | Source |
|---|---|
| Node health, ready, gRPC flag | `GET /health` (local), `GET /console/api/node` (peers) |
| Raft role, leader, term, vote | `RaftMetrics` via `NodeSnapshot` |
| Log/applied/committed/snapshot/purged | `RaftMetrics` + `LogStore`/state-machine state |
| Membership voters/learners | `RaftMetrics::membership_config` |
| Replication lag/heartbeat | leader `RaftMetrics::replication` / `heartbeat` |
| Transport streams, requests, snapshot timing | `GET /metrics.transport_metrics` (also `NodeSnapshot.transport`) |
| Event retention | `GET /events/status` |
| Market-data gate/counters | `POST /read` with `"marketdata_status"` |
| Order-book sequence/spread/last trade | `POST /read` with scalar keys |
| Event pages | `POST /events` (leader only) |
| Actions | new `/console/api/actions/*` endpoints |

The console must not call `/read orderbook_orders` on a large book; it is a full snapshot and can
produce a very large response. Scalar keys only in the first version.

## 9. Security

- **Trust boundary:** the node HTTP listener remains plaintext and unauthenticated. The console is
  for loopback or a trusted private network behind a gateway. The gateway must terminate TLS,
  authenticate operators and restrict `/console` and `/console/api/actions/*`.
- **CSRF:** JSON content type plus a custom action header, no CORS, and an Origin/Host check.
  No cookies are used, so there is no ambient credential to steal. A per-process console token can
  be added later if the gateway cannot enforce method-level protection.
- **XSS:** CSP with no inline scripts; DOM text rendering; no third-party assets.
- **SSRF:** cluster aggregation only fetches addresses from the local membership configuration.
  Action endpoints never accept a URL.
- **Data exposure:** node addresses, data-directory paths and log indices are visible. Keep full
  paths in the diagnostics tab rather than the default overview.
- **Rate limiting:** one action at a time; aggregate caching and a peer-fetch concurrency cap.
- **Audit:** structured action logs; optional in-memory action history for the UI. Durable audit
  belongs to the gateway/log pipeline, not this process.

## 10. Failure handling

| Failure | Behavior |
|---|---|
| A peer is unreachable | Node card shows `unreachable` with the error; other nodes still render; aggregate returns partial data. |
| No leader | Overview shows "no leader"; `/ready` semantics already return 503; actions are disabled. |
| Local storage read fails | `/console/api/node` reports `healthy: false` and an error; UI shows a red banner. |
| gRPC listener stops | Health reports `grpc_listening: false`; the node fail-fast path stops HTTP/Raft shortly after. |
| Transfer target is a learner/unknown/self | Action rejected with 400/409 before touching Raft. |
| Transfer target is lagging | UI warns; OpenRaft may reject with `LogNotFlushed`; UI reports timeout/failed and keeps the current leader. |
| Action endpoint called on a follower | 409 with the current leader id; UI offers the leader's console link. |
| Browser loses connectivity | Keep last data, show stale banner, exponential backoff. |
| Console API is older/newer during rolling upgrade | `capabilities` and `schema_version` gate unsupported views/actions; unknown fields are ignored. |

## 11. Testing

### Unit tests

- `NodeSnapshot` conversion from `HealthResponse`/`RaftMetrics` with absent indices.
- Action validation: self target, unknown target, learner target, not leader, action in progress.
- Capability gating for `transfer_leader`.
- CSP/security headers and asset content types.
- Origin/Host and custom-header rejection.

### Integration tests

- `tests/cluster/test_console.rs`, ports 21906-21908 (gRPC 22906-22908):
  - `GET /console/` returns HTML with the expected CSP and no external asset URLs.
  - `/console/api/node` reports the local node and storage frontier before/after a write.
  - `/console/api/cluster` reports all three nodes, leader and followers; stop one node and assert
    a partial response with `reachable: false`.
  - `/console/api/actions/transfer-leader` rejects non-leader, learner, self and unknown targets.
  - On a healthy three-node cluster, transfer leadership from node 1 to node 2, poll until
    `current_leader == 2`, assert node 1 becomes a follower and a write still succeeds.
  - `POST /console/api/actions/elect` with Pre-Vote does not change the term when a healthy leader
    still has quorum.
  - Action endpoints reject cross-origin-style requests without the custom header.
- Existing health/readiness tests remain the source of truth for `/health` and `/ready`.

### Manual checks

- Chrome/Safari/Firefox on the internal network; no external network access.
- Tab hidden/resume, peer down, leader election, action confirmation and timeout.
- Keyboard navigation and screen-reader labels for the action dialogs.

## 12. Performance budgets

- Assets: HTML + CSS + JS < 100 KiB uncompressed total; no external requests.
- Cluster aggregate: p95 < 100 ms for a healthy three-node LAN cluster; per-peer timeout
  500–750 ms; total request bounded to ~2 s.
- Polling: 2 s interval; one in-flight request per tab; pause when hidden; the aggregate reuses a
  shared peer HTTP client and does not cache yet.
- Action submission: < 50 ms to return `202`; completion is observed by polling, not held open.
- Browser memory: < 50 MiB for the console tab in normal use.

## 13. Implementation plan

### M0 — Backend foundation (done)

1. Add the `TransferLeader` RPC to `proto/raft.proto`, the gRPC client/server and the
   `RaftNetworkV2` implementation.
2. Extract a reusable node-snapshot collector from the health code; add
   `GET /console/api/node` and `GET /console/api/cluster`.
3. Add action endpoints with validation, action serialization, audit logging and capability
   reporting.
4. Add embedded assets and the `/console` routes with security headers.
5. Add integration tests for the aggregate and transfer path.

Acceptance: a three-node cluster can be inspected and leadership can be transferred from the
console API without changing the existing `/write`, `/read` or Raft wire formats.

### M1 — Read-only console (done)

1. Overview, Raft, Storage, Events and Diagnostics tabs.
2. Polling, stale/error banners, leader/follower views.
3. Event pagination and market-data/order-book scalar status.
4. No actions enabled yet.

Acceptance: an operator can diagnose a lagging peer, storage frontier, event retention and feed
state from one page.

### M2 — Guarded actions (done)

1. Transfer leadership, election request and snapshot action.
2. Confirmation dialogs, action lock, timeout/pending states, action history.
3. Security tests for custom header/Origin and target validation.

Acceptance: transfer succeeds on a healthy cluster; invalid/unsafe requests are rejected without
changing Raft state.

### M3 — Later

- SSE streaming instead of polling.
- Lightweight sparklines for term/lag/event retention.
- Guarded membership/bootstrap actions after a separate design.
- Optional `--console-read-only` / `--console=off` configuration if the gateway cannot enforce
  method-level restrictions.
- Chinese localization if operators need it.

## 14. Alternatives considered

| Alternative | Why not chosen for the first version |
|---|---|
| React/Vue/Svelte SPA with a build step | Adds Node/npm and a build artifact pipeline for a small internal tool. Vanilla JS is enough for tables, cards and dialogs. |
| Server-rendered templates only | Harder to do live polling and action dialogs without a full page reload; a tiny JS layer is simpler. |
| Browser fetches peers directly | Requires CORS, exposes peer addresses to the browser and complicates mixed-version handling. Server-side fan-out is safer. |
| WebSocket/SSE in MVP | Polling every 2 s is sufficient for a 3–5 node cluster and much simpler to reason about. SSE is a later optimization. |
| Raw `/metrics` JSON in the UI | Couples the frontend to OpenRaft internals and the `Result` wrapper. A stable `NodeSnapshot` schema is easier to evolve. |
| Only election trigger, no transfer RPC | Election is best-effort and can disrupt a healthy leader; graceful transfer is the correct maintenance operation. Implement both, prefer transfer. |
| Expose membership changes immediately | High-risk quorum changes need a separate confirmation/diff design. Show membership read-only first. |

## 15. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Operator transfers leadership to a lagging/unreachable voter | Target validation, lag warning, confirmation, OpenRaft's `LogNotFlushed` rejection, no automatic retries, poll for actual outcome. |
| Console becomes a second unauthenticated admin surface | Gateway restriction, action header/Origin checks, action lock, audit logs; no new trust boundary beyond existing admin routes. |
| Aggregate fan-out adds load during an incident | Shared peer HTTP client, 500–750 ms peer timeout, max 8 concurrent fetches, partial results; a short cache/single-flight is a follow-up. |
| UI shows stale data during a partition | Show `generated_at_ms`, per-peer fetch time, stale banner and unreachable badges. |
| Mixed-version cluster lacks `TransferLeader` | `capabilities` gates the action; fallback election request; document rolling-upgrade behavior. |
| OpenRaft metrics shape changes | Stable `NodeSnapshot` schema; UI ignores unknown fields; `schema_version` for breaking changes. |
| Large `/read orderbook_orders` response | Console uses scalar `/read` keys only; full book download is not part of the UI. |
| XSS from event/order fields | CSP, DOM text rendering, no `innerHTML` from server data. |

## 16. Decisions and open questions

Resolved in the first implementation:

1. The console is enabled by default at `/console`, behind the same gateway trust boundary as the
   existing admin API. A CLI/env opt-out is still desirable for hardened deployments.
2. The gRPC `TransferLeader` RPC is implemented in M0 and is the preferred leadership-switch path.
3. Action audit uses structured logs plus a bounded in-memory history; durable audit belongs to the
   gateway/log pipeline.

Still open:

4. Should the cluster aggregate warn when the local membership log is behind the leader's? It can
   only use local membership today.
5. Do we need a per-process console token in addition to the custom header? Recommendation: only
   if the gateway cannot enforce method-level restrictions.

## 17. Acceptance criteria

- `GET /console/` serves a self-contained HTML/CSS/JS console from the node with no external
  requests.
- The overview shows local health, leader/term, all members, per-peer reachability and lag.
- The Raft view shows membership, log/apply/commit/snapshot/purge indices and leader replication
  data.
- The storage view shows WAL/state-machine frontiers, event retention and market-data status.
- The event view pages the existing event log and clearly indicates leader-only availability.
- Leadership transfer from a healthy three-node cluster succeeds; the old leader steps down and
  writes continue.
- Invalid actions (non-leader, learner, self, unknown target, missing action header) are rejected
  without changing Raft state.
- Purge-log rejects a missing snapshot or an index above the snapshot and accepts the snapshot
  index; runtime-config is refused on the leader and accepted on a follower.
- Existing `/health`, `/ready`, `/metrics`, `/write`, `/read` and `/events` behavior is unchanged.
- Tests cover action validation, transfer success and security headers.
