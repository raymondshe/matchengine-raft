# Open Code Review — whole codebase (outstanding findings)

- **Date:** 2026-11-25
- **Mode:** delegation (host agent's model; OCR CLI v1.14.3)
- **Scope:** range `41da9624ab9e7a941dc310a65a9c6a2ab70a627b` (root commit) → `HEAD`
- **Rules:** default only; no custom rule set configured
- **Method:** `ocr delegate preview` / `ocr delegate rule`, diffs read with plain
  `git --no-pager diff --no-ext-diff --no-textconv --no-color`; no `ocr review` or
  `--analysis` was used. Findings were spot-checked against the current source.
- **Status:** findings that have been fixed have been removed from this report. The remaining
  items are either deferred design-level hardening (tracked as P1 in `TODO.md`) or
  documentation-only follow-ups.

## Coverage (original review)

| Metric | Value |
|---|---:|
| Reviewable files | 205 |
| Reviewed | 191 |
| Skipped (with reason) | 14 |
| Files accounted for | 205 / 205 (100%) |
| Original findings | 43 |
| Outstanding findings | 2 |

Skipped files and reasons:

- Generated artifacts with no reviewable logic: `docs/historical-bars-tdengine.html`,
  `docs/kubernetes-console-architecture.html`, `docs/matchengine-raft-architecture.html`,
  `docs/matchengine-raft-thread-model.html`, `k8s_control_plane/static/index.html`,
  `LICENSE`, `.dockerignore`, `k8s_control_plane/.dockerignore`, `match_engine/.dockerignore`,
  `match_engine/LICENSE`, `trading_terminal/.dockerignore`.
- Vendored/static assets: `market_data_system/web/app.css`,
  `market_data_system/web/lightweight-charts.js` (TradingView Lightweight Charts v5.3.0,
  Apache-2.0), `order_system/web/app.css`.

Every reviewed path is an addition in this range (the range is the project's initial
implementation), so there is no per-file before/after workflow change to report.

## Remaining findings

| Path | Issue | Category |
|---|---|---|
| `k8s_control_plane/deploy/prometheus/grafana.yaml` | Hard-coded `admin`/`admin` credentials on an Ingress-exposed Grafana with no TLS/auth. The Prometheus lifecycle endpoint was removed in the fix pass; this item still needs Secret-backed credentials and gateway auth/TLS (lab-scoped, but the production overlay includes the stack). | security |
| `k8s_control_plane/deploy/tdengine-ingress.yaml` | Plaintext REST with default `root:taosdata` and no NetworkPolicy of its own. Lab-only by documentation; the production overlay must not reuse it. | security |

## Verification notes

Spot-checked directly against source before finalizing: the backup uninstall patch indices,
fuzz snapshot version, symbol validators, duplicate-name inserts, console event-button wiring,
terminal leverage handling, and TDengine HA `TAOS_FQDN`. Suspected issues that were checked and
ruled out include the market-data driver flags, replay response text,
`check-30m-rollup.py`'s Decimal comparison, `orbstack-cluster.sh`'s dual-fsync check,
`RaftThread::spawn`'s `started_rx.await`, event long-poll wakeups, WAL recovery framing and
gRPC stream/snapshot lifecycle. The `order_system` depth finding was also ruled out: the engine
stores the visible tranche in `Order.volume` and the iceberg reserve in `hidden_volume`, and
`OrderBook::depth()` sums `Order.volume`, so subtracting the reserve would double-subtract.

---

# Part II — Performance and security review (outstanding findings)

- **Date:** 2026-11-25
- **Method:** targeted source/manifest audit on top of Part I. A delegated security pass
  covered `match_engine`, `market_data_system`, `order_system`, `k8s_control_plane`,
  `trading_terminal`, `deploy/`, scripts and CI; its findings were re-checked against source.
  Performance findings were verified directly against source and existing benchmark evidence.
- **Dependency check:** `npm audit --package-lock-only --omit=dev` in `trading_terminal/`
  reports **0 vulnerabilities**. `cargo audit` is not installed in this environment, so no
  Rust advisory scan was run (lockfile versions were inspected for obviously stale pins).
- **Status:** fixed findings have been removed; only open items remain.

## Summary

| Dimension | Original findings | Outstanding | Fixed/removed |
|---|---:|---:|---:|
| Performance | 10 | 4 | 6 |
| Security | 23 | 19 | 4 |

"Critical/High" security findings assume the manifests are deployed **as shipped**, with
`ingress-nginx` publishing the `*.match-eu-1.example` hosts and no authenticating gateway.
`TODO.md` P1 #1 explicitly records TLS/auth as outstanding production work; these are known,
unmitigated gaps, not new bypasses. Findings marked *lab-only* are not in the production
overlay unless noted otherwise.

## Remaining performance findings

### Low

- **`market_data_system/src/session.rs:474-498`** — combined-mode framing re-copies each
  payload per subscriber (`wrap` allocates a `Vec` for every forward task). The stream wrapper
  is identical for all subscribers of a key, so it can be precomputed once; existing
  `ws_fanout` numbers (110–186k frames/s) show this is an optimization, not a blocker.
- **`market_data_system/src/server.rs:406-495`** — a `/klines` request issues up to three
  sequential TDengine queries (`has_bars`, `last_ts`, `previous_close`, then `bars`/`rollup`).
  Cache `last_ts`/`has_bars` briefly or pipeline to cut tail latency. (The unbounded time-range
  issue was fixed by clamping the requested span.)
- **`k8s_control_plane/src/api.rs:55-125`** — `store.snapshot()` clones the catalog per request
  and `render_*` rebuilds full manifest lists for detail views; fine at current scale, but not
  O(1) in CR count.
- **`market_data_system/src/feed.rs:763-767`** — `bar_quote.retain` scans the accumulator on
  every fill batch; bounded to two windows per interval, so this is minor.

### Checked and sound (performance)

WAL batching and the default no-fsync production profile, snapshot encode outside the
state-machine lock, inline `SmallVec` apply buffers, bounded event window/market-data queues,
bar history at 1,000 entries and terminal trades at 60, depth capped at 200 levels, TDengine
writes batched 200 rows/insert with time-bounded `SELECT` and `CAST(... AS VARCHAR)` reads,
`limit` capped at 1,000 for `/klines`, bounded WebSocket queues with slow-consumer close, and
the `ws_fanout`/`read_e2e`/`apply_path` baselines in `docs/PERFORMANCE.md`. No benchmark was
run as part of this pass; numbers quoted are from the existing documentation.

## Remaining security findings

### Critical as shipped — known P1 gap

**S1 · `k8s_control_plane/src/api.rs:55-125,314-397,1370-1387,1555-1590`;
`provision.rs:279-321`; `validate.rs:791-798`; `deploy/web-ingress.yaml:1-30` —
unauthenticated manager API.**
No auth/authz/CSRF/rate-limit layer exists on any route, including `create_shard`,
`upgrade_shard`, `create_order_system`, `create_market_data_service` and `upsert_bundle`.
`upgrade_shard` accepts any non-empty image and only *warns* when it is not digest-pinned; the
operator server-side-applies that image. The manager is published in plaintext on
`console.match-eu-1.example` and its ServiceAccount can write every `matching.io` CR and delete
PVCs. Anyone who can reach that host can change desired state, deploy an arbitrary image, drop
shards and read audit/manifest data — a cluster-compromise path. The repo documents this as
demo-only (`web-ingress.yaml` header, `TODO.md` P1 #1), so this blocks production rather than
being an unknown bypass. Fix: OIDC/mTLS + authorization and CSRF tokens, an image allow-list
with digest/signature enforcement, per-action approval for destructive operations, and a
narrower ServiceAccount.

### High

**S2 · `match_engine/src/network/console.rs:509-886,1013-1026`;
`k8s_control_plane/src/api.rs:744-772`; `deploy/web-ingress.yaml:20-31` — engine console
actions are unauthenticated and re-exported publicly.** The five actions require only the
presence of `X-Console-Action` and an `Origin` match *if* `Origin` is present; `curl` supplies
both. The manager proxy explicitly allow-lists those action paths, so a remote caller can
force elections, snapshots, log purges and disable `tick`/`heartbeat` on a follower — an
availability/control-integrity issue. Purge is capped at the snapshot index, so this is not
direct log loss. Fix: authenticate the console, remove the action proxy from the public host,
and verify a nonce/token value rather than header presence.

**S3 · `match_engine/src/network/management.rs:76-150`; `grpc/server.rs:195-345`;
`src/lib.rs:764-780` — Raft/admin endpoints have no authentication.** `/init`, `/add-learner`
and `/change-membership` accept arbitrary voter addresses from any pod admitted by the
rendered NetworkPolicy; gRPC has no TLS/interceptor. This is internal-by-design, but the
rendered policy trusts every pod in the namespace and the `match-engine` pods of the whole
cell. Fix: mTLS/shared secret on both listeners, bootstrap-only membership endpoints, and
per-peer limits. (The unlimited snapshot-stream staging issue was fixed with a concurrency cap.)

**S4 · `k8s_control_plane/src/render.rs:1521-1531`; `market_data_system/src/main.rs:142-145`;
`deploy/tdengine-ingress.yaml` — historical store uses default `root`/`taosdata` over plaintext
HTTP.** `MarketDataHistory` has only `url`, `database` and `seal_lag_ms`; the renderer emits no
user/password or `secretKeyRef`, so the service falls back to the defaults and sends HTTP
Basic to `taosAdapter`. The lab REST Ingress publishes that API explicitly with default
credentials, and the production overlay reuses the same renderer. Fix: Secret-backed DSN
credentials, TLS to `taosAdapter`, remove the REST Ingress outside the lab, and a TDengine
NetworkPolicy. (`docs/HISTORICAL_BARS.md` already lists Secret-backed DSN as P1.)

**S5 · `order_system/src/api.rs:168-256`; `render.rs:2416-2449` — unauthenticated order
entry and symbol configuration.** `/demo/api/orders`, `/orders/cancel`, `/orders/amend` and
`/symbol` rely on `X-Demo-Action` + optional Origin only; `user_id` is caller-supplied, so any
caller can trade as any user and, before the first accepted order locks it, set symbol
precision/fees/STP. Demo by design, but the production overlay uses the same renderer and the
terminal proxies `/demo/api/*` same-origin. Fix: authenticate the desk, bind `user_id` to the
session, require TLS, and gate `ConfigureSymbol` behind an admin credential.

### Medium

| # | Issue | Evidence / fix |
|---|---|---|
| S7 | Lab observability Grafana default credentials | `prometheus/grafana.yaml:38-41` sets `GF_SECURITY_ADMIN_PASSWORD=admin` on an unauthenticated plaintext Ingress. The `--web.enable-lifecycle` flag in `prometheus.yaml` was removed in the fix pass. Generate a Secret and add auth/TLS or exclude Grafana from the production overlay. |
| S9 | Supply-chain pinning | Actions are tag-pinned (`actions/checkout@v5`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`) and Dockerfiles use mutable base tags. Pin to SHAs/digests and add `permissions: contents: read` (only `fuzz.yml` sets it today). No `pull_request_target`, no untrusted event interpolation, no CI secrets — good. |
| S10 | Namespace Pod Security / default-deny | Only `match-control` has PSA labels (`manager.yaml:15-18`); `match-eu-1` does not. Rendered per-shard policies exist, but lab TDengine has no NetworkPolicy and `loki.yaml:237-241` uses a lab `hostPath`. Production overlay enforces `restricted` only in `match-control`. Label workload namespaces restricted and add default-deny + a TDengine policy. |
| S12 | WebSocket amplification | Defaults are 256 connections × 1,024 streams (~262k receivers) with a global connection cap, so one client can deny service to others. Add per-IP quotas and a lower terminal `max_streams`. |
| S13 | Unauthenticated read surfaces | `GET /api/v1/{audit,catalog,backup-status,images}` and `/shards/{name}/manifests` expose topology, images and operator actions; part of S1's exposure. Fix with the same auth layer. |
| S14 | History DSN credentials in CRD | `MarketDataHistory.url` is a plain string rendered into `--tdengine-url`; if it embeds `user:password`, the secret is readable by every CR reader and lands in bundled catalogs/backups. Known P1 ("Secret-backed DSN"). Move credentials to a Secret ref. |
| S15 | Overbroad RBAC | `prometheus/bridge.yaml:19-25` uses a ClusterRole for services/endpoints/pods though it scrapes one namespace; `rbac.yaml` lets `match-backup` create pods cluster-wide (needed for checksum jobs but over-scoped). Prefer namespace Roles and restrict pod creation to the target namespace. |

### Low

- **CSRF guards are presence checks, not tokens** (`console.rs:1013-1026`, `order_system/src/api.rs:257-273`); browser CSRF is blocked today only because the non-simple header triggers preflight and no CORS is configured. Use nonce values plus real auth.
- **`client_order_id` dedupe** is a fixed 262,144-entry map; an unauthenticated writer can evict legitimate retry keys, defeating exactly-once retries.
- **Internal error strings** are returned on 500 from `/write`, `/init`, `/add-learner` and `/change-membership`; use correlation IDs.
- **`match_engine/docker/linux.Dockerfile:36`** runs as root and installs iptables (lab partition tool only); keep it out of production images.
- **ingress-nginx ClusterRole** can read Secrets cluster-wide (standard controller RBAC; no TLS Secrets today).
- **`/events` long-poll** has no admission gate; many concurrent polls consume tasks/memory.
- **Manager GUI** emits no CSP/security headers, though it consistently HTML-escapes catalog values (`static/index.html:692`); add the same headers used by the engine/desk consoles.

### Checked and sound (security)

TDengine SQL values/identifiers are validated and typed (no request string interpolation),
`valid_identifier` rejects `/`/`..`/quotes; backup bundle paths reject traversal and drills use
HMAC-SHA256 with a minimum key length; archive/replay paths use the validated symbol charset;
the console proxy allow-lists paths and rejects dot segments; body caps (engine 1 MiB, BFF 1 MiB,
axum default 2 MiB plus the new explicit console-proxy cap), `client_order_id`/read-key
validation, event page and batch caps, write admission, gRPC 64 MiB / snapshot 256 MiB with
CRC+postcard validation and a four-stream staging cap, and WS stream/connection caps are all
present. Engine, market-data, order, manager and operator images run non-root with dropped
capabilities, read-only root filesystems and seccomp RuntimeDefault; no `privileged`,
`hostNetwork` or `cluster-admin` in `deploy/`; production images are digest-pinned. Real secrets
and generated patches are gitignored, examples are placeholders, and no GitHub tokens/private
keys were found. React and the static consoles use escaping/CSP, and there is no CORS layer.
`npm audit` is clean; no Rust advisory scan was available.

---

Fixed findings have been removed from this report. The source fixes are applied in the working
tree and were verified with all crate tests, Clippy/fmt, the terminal build and a Kustomize
build; no commit was made.
