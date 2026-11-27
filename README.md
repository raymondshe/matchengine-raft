```
███╗   ███╗ █████╗ ████████╗ ██████╗██╗  ██╗███████╗███╗   ██╗ ██████╗ ██╗███╗   ██╗███████╗    ██████╗  █████╗ ███████╗████████╗
████╗ ████║██╔══██╗╚══██╔══╝██╔════╝██║  ██║██╔════╝████╗  ██║██╔════╝ ██║████╗  ██║██╔════╝    ██╔══██╗██╔══██╗██╔════╝╚══██╔══╝
██╔████╔██║███████║   ██║   ██║     ███████║█████╗  ██╔██╗ ██║██║  ███╗██║██╔██╗ ██║█████╗      ██████╔╝███████║█████╗     ██║
██║╚██╔╝██║██╔══██║   ██║   ██║     ██╔══██║██╔══╝  ██║╚██╗██║██║   ██║██║██║╚██╗██║██╔══╝      ██╔══██╗██╔══██║██╔══╝     ██║
██║ ╚═╝ ██║██║  ██║   ██║   ╚██████╗██║  ██║███████╗██║ ╚████║╚██████╔╝██║██║ ╚████║███████╗    ██║  ██║██║  ██║██║        ██║
╚═╝     ╚═╝╚═╝  ╚═╝   ╚═╝    ╚═════╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═══╝ ╚═════╝ ╚═╝╚═╝  ╚═══╝╚══════╝    ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝        ╚═╝
```

# ⚡ Matchengine Raft

<p align="center">
  <img src="./match_engine/MatchEngineRaft.png" alt="Matchengine Raft logo" width="380">
</p>

[![CI](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml/badge.svg)](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-%E2%89%A5%201.89-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](./LICENSE)
[![Snapshot v11](https://img.shields.io/badge/snapshot-v11-blue.svg)](./docs)
[![Market-data wire v7](https://img.shields.io/badge/marketdata%20wire-v7-blue.svg)](./docs/MARKET_DATA_PLAN.md)
[![WAL fsync: configurable](https://img.shields.io/badge/WAL%20fsync-configurable-blue.svg)](./docs/STORAGE.md)

**A complete exchange stack you can inspect, replay and operate — end to end.**

At its core is a Raft-replicated matching engine in Rust: deterministic order execution with
18-decimal precision, a full order-type lifecycle, replicated match events, and WAL durability you
tune to the deployment rather than inherit by assumption. Every state change is observable — replay
the log, dump a snapshot, reproduce a fill, or watch the book live in the operational console.

Around that core sits the rest of the exchange. A deterministic market-data path carries trades over
Aeron multicast or NATS into a Binance-style WebSocket service, backed by exact historical bars in
TDengine, per-symbol writer leases, and REST tail repair that closes real gaps instead of hiding
them. A stateless order-system BFF powers a React trading terminal while the book stays
market-data-fed and clients never poll the engine.

Operations are part of the design, not an afterthought. A CRD-driven Kubernetes control plane elects
an operator, bootstraps Raft, rolls pods one at a time with wire and snapshot compatibility gates,
verifies backups by restoring and hashing them, and exposes metrics, logs, alerts and dashboards.
The result is a system that runs in a single-node lab and scales into a sharded fleet — with no
black boxes, no hidden state and no guessing about what the engine did.

**On this page:** [why this stack](#-why-teams-pick-it) · [repository map](#-five-projects-one-stack) ·
[architecture](#-architecture) · [quick start](#-quick-start) · [capabilities](#-features) ·
[market data and bars](#-historical-bars-with-tdengine) · [terminal](#-trading-terminal) ·
[Kubernetes](#-kubernetes-control-plane) · [security](#-security-and-deployment-boundary) ·
[performance](#-performance) · [observability](#-observability-ui) · [docs](#-documentation-and-testing).

## ✨ Why teams pick it

| Layer | What you get | Dive in |
|---|---|---|
| ⚡ **Matching** | Deterministic price-time book, full order-type lifecycle, STP and exact 18-decimal arithmetic — `apply` never reads a clock, so replaying the log reproduces the book. | [Features](#-features) · [Matching](./docs/MATCHING.md) |
| 🛡️ **Durability and consensus** | OpenRaft 0.10 over gRPC with Pre-Vote and leadership transfer, a segmented checksummed WAL with selectable fsync, atomic snapshots and snapshot + WAL-tail recovery. | [Storage](./docs/STORAGE.md) · [API](./docs/API.md) |
| 📡 **Market data** | A bounded, leader-gated trade feed over Aeron or NATS, Binance-style WebSocket streams with conflation, and exact TDengine historical bars with Lease-guarded writers and REST gap repair. | [Historical bars](#-historical-bars-with-tdengine) · [Market-data system](./market_data_system/README.md) |
| 🧾 **Clients and APIs** | Leader-aware Rust client, `/events` cursors, batch writes, local-lease reads with `ReadIndex` fallback, and per-node write admission. | [API](./docs/API.md) · [Match results](./docs/MATCH_RESULTS.md) |
| 🖥️ **Trading surface** | A stateless order-system BFF and a pure React/TS terminal — the book is market-data-fed and the browser never calls engine `/write`. | [Terminal](#-trading-terminal) · [Order desk](./order_system/README.md) |
| ☸️ **Operations** | Embedded console, probes, metrics, replay tools, CRD-driven fleet control, one-pod-at-a-time rolls, verified backups and a ready observability stack. | [Kubernetes](#-kubernetes-control-plane) · [Observability](#-observability-ui) |

## 🧩 Five projects, one stack

| Project | What it is | Start here |
|---|---|---|
| 🦀 **match_engine** | Raft-replicated, deterministic matching engine: order book, WAL/snapshots, gRPC Raft, event log, market-data feed and operational console | [`match_engine/README.md`](./match_engine/README.md) |
| ☸️ **k8s_control_plane** | CRD-backed manager, Lease-elected operator, renderer and GUI for single-symbol engine fleets | [`k8s_control_plane/README.md`](./k8s_control_plane/README.md) |
| 📡 **market_data_system** | Standalone Aeron/NATS/JSONL consumer serving Binance-style WebSocket market data and TDengine-backed historical bars | [`market_data_system/README.md`](./market_data_system/README.md) |
| 🧾 **order_system** | Standalone order-entry WebUI and demo freeze/settlement estimates | [`order_system/README.md`](./order_system/README.md) |
| 🖥️ **trading_terminal** | Pure React/TS Binance-style spot terminal: book, tape, chart and order ticket behind nginx | [`trading_terminal/README.md`](./trading_terminal/README.md) |

```text
.
├── match_engine/          # engine crate, benches, examples, tests, fuzz, console assets and Dockerfiles
├── k8s_control_plane/     # manager API/GUI, operator and CRDs
├── market_data_system/    # WebSocket market-data service + historical bar writer/query
├── order_system/          # demo order desk and BFF
├── trading_terminal/      # React/TS terminal + nginx proxy (no Rust, own build)
├── docs/                  # cross-project design, API, runbook, diagrams and performance notes
├── scripts/               # cluster, lab and demo orchestration
└── .github/workflows/     # CI, fuzz and benchmark workflows
```

## 🏗️ Architecture

This project extends the
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) with
persistent storage, a gRPC transport and a live market-data path. The software architecture is
shown first, followed by the runtime architecture with the four-voter engine cluster, the
order-desk tier shared by user ID, and the market-data fan-out. For an interactive, source-linked
view of the engine internals, open the
[interactive architecture diagram](./docs/matchengine-raft-architecture.html).

<p align="center"><strong>Software architecture</strong></p>

<p align="center">
  <img src="./docs/matchengine-raft.png" alt="matchengine-raft software architecture" width="1000">
</p>

<p align="center"><strong>Runtime architecture</strong></p>

<p align="center">
  <img src="./docs/matchengine-raft-runtime.png" alt="Four-voter match engine runtime with order-desk and market-data tiers" width="1000">
</p>

<p align="center"><em>Four-voter engine cluster, order-desk tier shared by user ID, and the leader-gated Aeron feed fanned out to stateless market-data instances.</em></p>

Peer-to-peer Raft RPCs use gRPC on `port + 1000`. The public application and management API stays
on the `--http-addr` listener. Raft RPCs are not part of the HTTP JSON API.

**Runtime notes.** `RAFT_CORE_WORKERS` moves RaftCore and the state-machine worker onto a dedicated
runtime; leader-consistent reads default to `local_lease` with a `ReadIndex` fallback while `/read`
stays local; and market-data egress uses a bounded `try_send` off the consensus path, so a slow
subscriber never blocks `apply`. Details live in [Capabilities](#-features) and
[Storage](./docs/STORAGE.md).

## 🧪 Features

✅ Everything below is implemented in this repository and exercised by tests.

**🧩 Matching engine**
- **Price-time priority order book** with GTC, IOC, FOK, GTD, Day, market, market-to-limit, post-only, iceberg, stop-market, stop-limit, trailing-stop and pegged orders, plus self-trade prevention.
- **Amend and cancel** with deterministic priority rules: same-price reductions keep time priority; price or size increases re-queue under a new sequence.
- **Asset-aware fee deduction** with maker/taker rates, rebates and exact notional accounting; crypto/FX default to received-asset fees, ETF/equity always settle in quote.
- **Base or quote volume** with tick/lot enforcement, quote-limit normalization and quote-budget market sweeps.
- **Exact 18-decimal fixed point** from API input through matching, events, snapshots and the market-data feed — no `f64` arithmetic in the matching path.

**🛡️ Durability and recovery**
- **Raft-replicated state machine** with durable membership and application state; WAL fsync is configurable.
- **Segmented, checksummed WAL** with `RAFT_WAL_FSYNC_MS`: unset = no fsync (default; requires at least 4 voters), `0` = every append, `N > 0` = periodic every N ms; torn-tail recovery. `--max-wal-append-bytes` (default 8 MiB) splits one apply batch into byte-bounded writes under one disk lock and one fsync.
- **Prototype async WAL** (feature-gated, off by default): `append` defers fsync to a Tokio worker and fires `IOFlushed` only when durable; includes pending-entry reads and crash-recovery tests. Build with `--features async-wal`; run with `--async-wal` and `RAFT_WAL_FSYNC_MS=0`.
- **Four-voter no-fsync floor**: `/init` and `/change-membership` reject fewer than four voters, and writes return `503` until four voters exist, so a committed entry stays on at least three nodes.
- **Atomic snapshots** written by temp file → fsync → rename → directory fsync, with checksum validation before install. Cached snapshot bytes are shared as `Arc<[u8]>`, and a received snapshot streams into a validated staging file instead of an unbounded memory buffer.
- **Snapshot policy for real workloads**: every 100,000 applied entries or 30 minutes when newer entries exist, configurable with `--snapshot-interval-ms`.
- **Snapshot + WAL-tail recovery** that restores the latest snapshot, event window and idempotency map, then replays only newer log entries.
- **gRPC replication** with bidirectional `stream_append`, chunked snapshot transfer, Pre-Vote elections, leadership transfer and independent per-peer channels.

**🔌 APIs and data feeds**
- **Durable match-event feed** at `POST /events` with cursor pagination, long-poll (`wait_ms`), retention limits, `client_order_id` idempotency, gRPC `StreamEvents` streaming and a reference MQ bridge sidecar; replay-safe retries.
- **Deterministic market-data feed** with leader gating, shadow book, BBO, depth snapshots/deltas, lifecycle events, OHLC bars and a compact binary wire format with a reference decoder. The feature-gated **Aeron** egress publishes it on streams 2001–2007 with one exclusive publication per stream, an idle STATUS heartbeat and a stream-2010 snapshot-request path.
- **Standalone market-data system** at [`market_data_system/`](./market_data_system/): consumes the feed over Aeron or JSONL and serves Binance-style WebSocket streams. One instance serves several symbols from a shared hub; clients hold a per-connection subscription set, switch symbols with live `UNSUBSCRIBE`/`SUBSCRIBE`, and scale horizontally without adding engine work. Late joiners request an immediate snapshot from the engine; `/ready` also expires on a missing frame/heartbeat.
- **Flexible read consistency**: local `/read`, linearizable leader reads and follower reads. Leader-consistent paths use OpenRaft's caller-side lease cache (`local_lease`) by default, then the core lease path, then `ReadIndex` when the lease is stale; `--read-policy read_index` forces a fresh quorum round and `lease` keeps the core path. The four-node benchmark measured 11–36% higher read throughput and ~40% lower 64-reader p50/p99 than the core lease path. Lease reads require bounded clock drift (NTP/PTP); `/read-policy/status` reports hits and fallbacks.
- **Leader-aware Rust client** that tracks and forwards requests to the current leader, with configurable timeouts, transient retry/backoff and typed API/no-leader errors; retries are at-least-once, so use `client_order_id` for idempotent write retries.
- **Operational APIs** for initialization, membership changes, metrics, event status and market-data status.
- **Batch writes** at `POST /write_batch`: multiple requests in one Raft entry and one WAL append, with per-request sequences and continuous event ordinals.
- **P99-aware write admission**: each node caps concurrent `/write` and `/write_batch` requests at 1,024 by default, returns `429` on overflow, and exposes current/peak/admitted/rejected counters at `/admission/status`.

**📊 Operational visibility**
- **Embedded WebUI console** at `/console/`: cluster health, Raft leadership and replication, storage/event/feed state, a leader-only event viewer and diagnostics — no build step or external assets. The applied `BASE/QUOTE` trading symbol is the page hero.
- **Guarded Raft actions**: graceful leadership transfer over the gRPC `TransferLeader` RPC, a best-effort Pre-Vote election request, and snapshot triggering, with confirmation, action locking and audit logs.
- **Standalone demo order desk** at [`order_system/`](./order_system/): a separate process and crate that places real orders and shows the full book, symbol configuration, a cursor-following `/events` tape and labelled demo estimates of the order-system freeze plus maker/taker fee settlement, with a demo-only linear perpetual/leverage overlay; reuses the console theme.
- **Health and readiness probes**: `/health` reports the gRPC listener, Raft core and WAL/state-machine frontier; `/ready` additionally requires a known leader, and `/pretrade/status` reports symbol/provisioning readiness reasons, making them suitable for orchestrators and pre-trade gates.

**☸️ Kubernetes fleet control (production shape)**
- **One control plane for thousands of single-symbol engines** — each symbol keeps its own process and four-voter Raft group; a 2,000-symbol fleet is 8,000 engine processes and, at 20 processes per pod, 100 four-pod shards (400 pods), partitioned into cells. Packing cuts pods and Kubernetes objects, not the per-symbol Raft groups.
- **Zero-impact supervisor mode** — one supervisor container per pod reads `processes.json` and starts/stops individual children; adding, stopping or starting a symbol leaves every other engine's PID, listener and data directory untouched. The OrbStack validation kept all pod UIDs and restart counts unchanged while adding and cycling children.
- **Explicit containers blast-radius guard** — a non-empty containers shard returns `409 restart_required`; `allowRestart` opts into a one-voter-at-a-time pod roll, so a routine add cannot silently restart the fleet.
- **Complete lifecycle with storage decisions** — graceful shutdown/start reuses the same deployment, slot/ports and PVC and recovers WAL/snapshot without a new `/init`; an idle shard can be dropped with `retainStorage` keeping the PVCs for restore or deleting them irreversibly.
- **CRD-backed desired state** — eight `matching.io` CRDs (`TradingSymbol`, `SymbolShard`, `SymbolDeployment`, `SymbolRoute`, `Cell`, `MarketDataService`, `OrderSystem`, `TradingTerminal`) with status subresources; the manager writes through the Kubernetes API server, rebuilds its cache from informers, uses `resourceVersion` concurrency and rate-limits status writes. etcd persists the CRs; the manager never connects to etcd directly, and engine WAL/events never go there.
- **Lease-elected cell operator** — watches the CRDs, server-side-applies/prunes the StatefulSet/Services/PDB/NetworkPolicy/ConfigMap, labels the dedicated-shard leader for the leader Service, and writes shard/deployment/route/symbol status and conditions. Watches trigger immediate reconciliation; a Kubernetes Lease serializes replicas.
- **Desired-state manager, not a hand-run YAML pile** — catalog validation, lowest-free slot and `httpBasePort + slot` allocation, rendered StatefulSet/Services/PDB/NetworkPolicy/ConfigMap, reconcile bootstrap/upgrade plans, audit and an engine-console proxy across many shards and cells.
- **Production Kubernetes primitives** — four-voter shards, one PVC per pod, PDB, NetworkPolicy, headless gRPC services, health/readiness probes, non-root images and leader-aware routing metadata. The reference manager, supervisor and Lease-elected operator are implemented here; the operator also runs bootstrap/config/membership, quorum-safe containers add/start/stop rolls, PVC retention and cross-shard cutover/rollback.

See [the Kubernetes control-plane demo](./k8s_control_plane/README.md) for the runnable
implementation and [the design](./docs/KUBERNETES_CONTROL_PLANE.md) for the production operator
contract.

**✅ Built to be trusted**
- **Bounded public edge**: request-body, read-key, event-page, idempotency-key and snapshot-size limits, with malformed input rejected before Raft.
- **Explicit internal-service contract**: loopback or trusted private networks, gateway-owned TLS/authentication, plus startup warnings for non-loopback binds.
- **Fail-fast transport health**: if the gRPC listener dies, `run()` stops the node and returns an error instead of leaving a half-healthy HTTP API serving peers that cannot reach it.
- **Correctness evidence**: randomized differential tests against a reference model, explicit order-type conformance cases, deterministic replay checks and cluster recovery tests.
- **One symbol per process/cluster** for isolated resources and failure domains; scale horizontally by running one deployment per symbol.

## 🚀 Quick start

| Goal | Start here |
|---|---|
| Inspect a single engine and its HTTP API | [Run the engine](#run-the-engine) below |
| Exercise replication, membership and recovery | [Start a local cluster](#start-a-local-cluster) below |
| See order entry and market data together | [Run the demos](#run-the-demos) below |
| Operate a multi-symbol fleet | [Kubernetes control plane](#-kubernetes-control-plane) |
| Understand the design before running anything | [Architecture](#-architecture) and [Documentation](#-documentation-and-testing) |

### Run the engine

```shell
cd match_engine
cargo build --release
cargo run --release --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:21001
```

`protoc` is not required: the build script vendors it. Rust 1.89 or later is required (edition 2024).

### Start a local cluster

```shell
./scripts/test-cluster.sh
```

The script builds, starts a four-node cluster, initialises it, adds learners, changes membership
and reads the book back from every node. Use `--nodes N`, `--base-port`, `--keep`, `--no-fsync`,
`status` and `stop` to control it. See the [runbook](./docs/RUNBOOK.md) for manual startup,
membership changes, failure drills and recovery.

### Run the demos

```shell
./scripts/demo-pair.sh             # two engines + order desk
./scripts/market-data-system.sh    # two engines + market-data WebSocket service
```

## 🧾 Demo components

The standalone [order desk](./order_system/README.md) and
[market-data service](./market_data_system/README.md) have their own run instructions,
screenshots and design links. Start both with the scripts above.

## 🗄️ Historical bars with TDengine

`market_data_system --history` adds a durable bar tier inside the existing service — no new crate
and no new public edge. A single writer folds the deterministic trade tap into sparse traded `1s`
rows plus materialized `1m` and `5m` rollups and upserts them into TDengine as canonical 18-decimal
text; `3m`…`3d` are read-time `INTERVAL` rollups (`30m` prefers `5m` when present, with a `1m`
fallback). Reads cast DECIMAL columns to `VARCHAR` and
parse with a strict scale-18 parser, so no `f64` or 96-bit client decimal can round a value. The
writer row carries the engine `market_seq` and its own trade cursor, which lets a replacement
resume from the newest row without a hot standby; readers are stateless and query TDengine
directly. `GET /klines` is Binance-shaped and dense, and the same rows seed the WebSocket
subscribe replay.

P0 and P1 are implemented and validated in the OrbStack lab: schema, trade-derived writer,
`/klines` and WS replay, plus the rolling trade spool, revisions/gaps, per-symbol Lease failover,
REST-tail gap repair and native `/bars?since=<seq>`. The archive importer and reconciler, gap-gated
reads, time-zone anchors and the three-dnode production topology remain open.

[![Historical bars: TDengine as the system of record](./docs/historical-bars-tdengine.png)](./docs/historical-bars-tdengine.html)

[Open the interactive Archify diagram](./docs/historical-bars-tdengine.html) ·
[Full design and measurements](./docs/HISTORICAL_BARS.md) ·
[Service configuration](./market_data_system/README.md)

The HTML above is the exported Archify artifact (route/lens views and image/SVG/WebM export); the
PNG is a still capture of that export for inline viewing.

## 🖥️ Trading terminal

[`trading_terminal/`](./trading_terminal/) is a Binance-style spot trading UI and a **pure
frontend**: React + TypeScript with its own Vite build, no Rust dependency and no crate embedding.
nginx is deployment plumbing only; it serves the SPA and reverse-proxies `/md/*` to the global
market-data service and `/demo/api/*` to the order-system BFF on one origin, so the BFF's
same-origin guard passes without CORS changes. Order entry goes through the BFF with
`X-Demo-Action: submit`; the browser never calls engine `/write`.

The book ladder keeps the spread vertically centred with asks bottom-aligned above it and bids
top-aligned below it. The chart seeds up to 500 candles from the TDengine-backed `/md/klines`
endpoint on every symbol/interval change, then overlays live trade and `kline` frames without
reopening closed bars; the header reports the seeded count (`HISTORY 500` below).

![Trading terminal with TDengine-backed historical candles](./docs/trading-terminal-history.png)

The screenshot is the live OrbStack lab at <http://terminal.match-eu-1.example/> (seeded demo bars,
not exchange history). See [`trading_terminal/README.md`](./trading_terminal/README.md) and
[`k8s_control_plane/deploy/terminal.yaml`](./k8s_control_plane/deploy/terminal.yaml) for the
deployment.

## ☸️ Kubernetes control plane

The engine trades one symbol per process, so a multi-symbol deployment is a fleet of independent
Raft clusters. [`k8s_control_plane/`](./k8s_control_plane/) is the reference control plane: it
models four-pod `SymbolShard`s that pack several single-symbol engine processes per pod, validates
placement and ports, renders the StatefulSet/Services/PDB/NetworkPolicy set, and plans bootstrap,
membership, routing and same-version upgrades. The manager API/GUI owns the symbol catalog, RBAC,
approvals and the allowlisted engine-console proxy. It can reserve empty shards, place a new symbol
into a reserved or existing shard with the next free slot/port, and prepare a shadow migration plan
for an existing symbol. Supervisor-mode shards add a symbol by changing only the ConfigMap, so
existing engine processes keep running; containers-mode additions require an explicit restart
opt-in.

The same control plane supports graceful shutdown and start of one symbol, retaining its slot,
ports and PVC and recovering its WAL/snapshot without a new `/init`. An idle shard can be dropped
with an explicit choice to keep the PVCs for a later restore or delete them irreversibly. Desired
state can use the JSON demo store or the CRD backend, which writes the eight `matching.io` CRDs
through the Kubernetes API server (etcd underneath) and refreshes its cache from informers. A
Lease-elected operator watches the CRDs, applies/prunes the rendered workloads, labels dedicated
leader pods, writes shard/deployment/route status, bootstraps/configures each Raft group, rolls
containers one pod at a time behind a quorum gate, applies PVC retention windows and performs
cross-shard cutover/rollback with a distinct Raft ID range. The architecture scales to thousands
of single-symbol engines and their four-voter process instances across shards and cells.

[![Kubernetes control plane, engine shards, order systems, market data and historical bars](./docs/kubernetes-console-architecture.png)](./docs/kubernetes-console-architecture.html)

[Open the interactive diagram](./docs/kubernetes-console-architecture.html) ·
[Design and CRDs](./docs/KUBERNETES_CONTROL_PLANE.md) ·
[etcd storage](./docs/ETCD_STORAGE.md) ·
[Local test plan](./docs/KUBERNETES_LOCAL_TESTING.md) ·
[Reference demo](./k8s_control_plane/README.md)

![Kubernetes control plane manager GUI](./docs/kubernetes-control-plane.png)

The manager GUI's **matching engine** view: the live `fx-eu-01` shard hosts `eurusd` and
`gbpusd`, both Ready, with per-pod requests, placement and release validation, per-process console
links and observed leaders. The manager also has **market data** (terminal and fast WebSocket
tiers) and **order systems** (user-ID shards, Demo order desk links and an image picker) views.

## 🔒 Security and deployment boundary

The HTTP API and the gRPC Raft listener (HTTP port + 1000) are **plaintext and unauthenticated by
design**. This is an internal matching engine, not an internet-facing gateway: run it only on
loopback or a trusted private network, firewall both ports, and terminate TLS plus client
authentication at the gateway in front of it. TLS/mTLS and API authentication are intentionally
not implemented in this process, where they would add latency without protecting a public surface.

The node logs a warning when it is bound to a non-loopback address. Request handling is bounded at
the edge, before Raft proposes anything:

| Surface | Limit |
|---|---|
| Application/management JSON bodies | 1 MiB (`MAX_REQUEST_BODY_BYTES`) |
| `client_order_id` | 1–128 bytes, ASCII letters/digits plus `. _ - :`; empty rejected |
| `/read` key | 1–128 printable ASCII bytes |
| `/events` page size | 1–1000 events |
| `/events` long-poll wait | 0–1000 ms (`wait_ms`) |
| `/write_batch` | 1–128 non-nested requests, unique `client_order_id`s |
| gRPC Raft message | 64 MiB (`MAX_MESSAGE_SIZE`) |
| gRPC snapshot stream | 256 MiB total (`MAX_SNAPSHOT_BYTES`), checked before each chunk is staged |

Malformed JSON and invalid fields return `400`; oversized bodies return `413`. The replicated
`client_order_id` dedupe map retains at most 262,144 records, so the key bound also bounds its
memory.

## 📊 Performance

Headline numbers from the reference machine (Apple M4, Rust 1.93.1, release bench profile). Book-only
rows exclude HTTP, Raft and WAL; replicated rows use four in-process loopback voters with WAL fsync
disabled. Methodology, percentiles, durability modes and caveats are in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).

**Standout order-book gains (2026-11-26).** Indexed parked stops, GTD deadlines and pegged orders
removed three scans that previously ran on every request:

| Path | Before | After |
|---|---:|---:|
| Book-only realistic replay (`replay_mix`) | 616k ops/s | **3.09M ops/s** (~5x) |
| Peg discovery, 5k-order book, no pegs | ~19 µs/apply | **0.41 µs/apply** (~45x) |
| Peg discovery, 32 pegged orders | ~16–19 µs/apply | **2.3 µs/apply** |

Remaining open items are in the [order-book performance review](./docs/ORDER_BOOK_PERFORMANCE_REVIEW.md);
before/after evidence is in [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).

**Standout apply-path gains (2026-11-26).** Responses are now built only when a client is waiting,
and a batch publishes through the sink once instead of once per sub-request:

| Path | Before | After |
|---|---:|---:|
| `apply_path` rest / cancel / fill allocations per entry | 3.17 / 3.00 / 4.00 | **0.17 / 0.00 / 1.00** |
| Batch-rest allocations / requested bytes per entry | 1.19 / 1,875 B | **0.21 / 1,141 B** |
| Id-bearing Place allocations per entry | 3.32 | **2.32** |

Remaining open items are in the [apply-path performance review](./docs/APPLY_PATH_PERFORMANCE_REVIEW.md);
measurements are in [`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md).

| Workload | Result |
|---|---:|
| Order-book deep sweep (100 levels) | 11.66M fills/s |
| Order-book deep cancels | 19.63M cancels/s |
| BBO advance / BBO query | 20.37M advances/s / 434.5M queries/s |
| Realistic mixed replay (add/cancel/amend/stop/iceberg/taker) | 3.09M ops/s |
| Replicated writes, 512 in flight (pre-connected) | 114,484 writes/s, p99 5.76 ms |
| Replicated writes, 1,024 in flight (pre-connected) | 99,330 writes/s, p99 8.74 ms |
| Replicated sequential 128-order batches | 227,319 orders/s |
| Replicated 100 x 32 concurrent batches | 216,163 orders/s |
| Local-lease linearizable reads (1 / 64 readers) | 27,357 / 56,924 reads/s; p50 34.8 / 53.5 µs |
| Local-lease follower reads (1 / 64 readers) | 12,564 / 39,473 reads/s; p50 78.4 / 198.1 µs |
| Local-lease event reads (1 / 64 readers) | 8,224 / 45,243 reads/s; p50 118.8 µs / 1.34 ms |
| Market-data feed (null sink) | 495k fills/s, 0 drops at an 8,192-frame queue |
| WAL append / read, no fsync (storage-only) | 2.15M / 5.66M entries/s |

Book-only figures are best-case single-process order-book numbers, not end-to-end capacity. The four-node
rows use a 3-of-4 quorum on loopback with all nodes in one process; they do not model cross-host
network latency, independent disk contention or host failure. Pre-connected writes plateau at
512–1,024 in flight; the earlier connection-storm sweep included TCP setup and understated
capacity. Every-append durability is much lower on the single-node sequential path (262 writes/s)
but still reaches 180k orders/s for 100 x 32 batches on one node; see PERFORMANCE.md for that
trade-off.

**Recommended production profile.** For maximum throughput, leave `RAFT_WAL_FSYNC_MS` unset
(default; requires at least four voters), keep `RAFT_CORE_WORKERS` unset and the OpenRaft
`300/1/0` defaults, and use `/write_batch` with 32–128 orders. The server caps concurrent
`/write` and `/write_batch` requests at 1,024 per node by default; excess requests receive `429`
and should be retried with backoff. Lower `--max-inflight-writes` to `512` for a tighter tail at
roughly 1,024 concurrent clients, or set it to `0` to disable rejection; tune the value against
the deployment load. Keep the default local-lease reads only when NTP/PTP keeps clock drift bounded;
otherwise set `--read-policy read_index`.
If acknowledged writes must survive an unclean crash, set `RAFT_WAL_FSYNC_MS=0` and batch writes
to amortize the fsync. Do not use the 10 ms fsync mode, 1 ms batch linger, 1,024-entry
payload/purge settings, async WAL or thin LTO; none beat the defaults. See
[production configuration decisions](./docs/PERFORMANCE.md#production-configuration-decisions-2026-11-08).

## 📈 Observability UI

One Grafana dashboard covers Raft and market-data rates, scrape health, alerts and operator
reconcile/rollout counters. Installation, verification and the other views are in
[`docs/OBSERVABILITY.md`](./docs/OBSERVABILITY.md).

![Grafana lab observability dashboard](./docs/grafana-observability.png)

## 📚 Documentation and testing

- [Engine README](./match_engine/README.md) — build/run, engine configuration and internals.
- [API](./docs/API.md) — HTTP application/admin routes and the gRPC Raft service.
- [Storage](./docs/STORAGE.md) — WAL, snapshots, recovery and dump tools.
- [Matching](./docs/MATCHING.md) — order types and amendment rules.
- [Match-result delivery](./docs/MATCH_RESULTS.md) — events, cursors and idempotency.
- [Testing](./docs/TESTING.md) — cluster suite, replay matrix and failure tests.
- [Performance](./docs/PERFORMANCE.md) — measured baselines and methodology.
- [Market-data on Kubernetes](./docs/AERON_K8S_MESSAGE_BUS.md) — market-data message bus, multicast constraints and scale-out profiles.
- [Lab observability](./docs/OBSERVABILITY.md) — metrics-server for HPA CPU, Prometheus bridge, operator metrics, alerts, Grafana dashboard and Loki log shipping.
- [Trading terminal](./trading_terminal/README.md) — the React/TS client, deployment proxy and TDengine-seeded chart.
- [Historical bars](./docs/HISTORICAL_BARS.md) — TDengine schema, exact-decimal contract, HA contract and measurements; the [interactive Archify export](./docs/historical-bars-tdengine.html) is checked in beside its PNG preview.
- [Runbook](./docs/RUNBOOK.md) — cluster, upgrade, migration and recovery procedures.
- [Third-party investigations](./docs/THIRD_PARTY_INVESTIGATION.md) — external engine reviews.
- [TODO](./TODO.md) — current priorities and roadmap.

```shell
cargo test --manifest-path match_engine/Cargo.toml --locked
cargo test --manifest-path k8s_control_plane/Cargo.toml --locked
cargo test --manifest-path market_data_system/Cargo.toml --locked
cargo test --manifest-path order_system/Cargo.toml --locked
```

## 📬 Contact and credits

- Google Chat: [rshen.sy@gmail.com](https://chat.google.com/u/0/dm/rshen.sy@gmail.com)
- This project is a fork from
  [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the
  [OpenRaft](https://github.com/datafuselabs/openraft) project.
- Special thanks to the Databend community and Zhang Yanpo for their support.

## 📄 License

MIT — see [`LICENSE`](./LICENSE).

The project started as a fork of
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the
[OpenRaft](https://github.com/datafuselabs/openraft) project, which is dual-licensed under MIT or
Apache-2.0. The original copyright and license notices continue to apply to code derived from it;
this repository is distributed under the MIT license in `LICENSE`.
