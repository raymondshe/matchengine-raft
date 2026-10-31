```
███╗   ███╗ █████╗ ████████╗ ██████╗██╗  ██╗███████╗███╗   ██╗ ██████╗ ██╗███╗   ██╗███████╗    ██████╗  █████╗ ███████╗████████╗
████╗ ████║██╔══██╗╚══██╔══╝██╔════╝██║  ██║██╔════╝████╗  ██║██╔════╝ ██║████╗  ██║██╔════╝    ██╔══██╗██╔══██╗██╔════╝╚══██╔══╝
██╔████╔██║███████║   ██║   ██║     ███████║█████╗  ██╔██╗ ██║██║  ███╗██║██╔██╗ ██║█████╗      ██████╔╝███████║█████╗     ██║
██║╚██╔╝██║██╔══██║   ██║   ██║     ██╔══██║██╔══╝  ██║╚██╗██║██║   ██║██║██║╚██╗██║██╔══╝      ██╔══██╗██╔══██║██╔══╝     ██║
██║ ╚═╝ ██║██║  ██║   ██║   ╚██████╗██║  ██║███████╗██║ ╚████║╚██████╔╝██║██║ ╚████║███████╗    ██║  ██║██║  ██║██║        ██║
╚═╝     ╚═╝╚═╝  ╚═╝   ╚═╝    ╚═════╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═══╝ ╚═════╝ ╚═╝╚═╝  ╚═══╝╚══════╝    ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝        ╚═╝
```

# Match Engine Raft

<p align="center">
  <img src="MatchEngineRaft.png" alt="Match Engine Raft logo" width="220">
</p>

[![CI](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml/badge.svg)](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-%E2%89%A5%201.89-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](./LICENSE)
[![Snapshot v9](https://img.shields.io/badge/snapshot-v9-blue.svg)](./docs)
[![Market-data wire v5](https://img.shields.io/badge/marketdata%20wire-v5-blue.svg)](./docs/MARKET_DATA_PLAN.md)

A practical implementation of a distributed matching engine built upon [OpenRaft](https://github.com/datafuselabs/openraft). Raft logs are persisted in a hand-written segmented write-ahead log and the state machine is made durable with atomic snapshots, so a node recovers all committed state after a restart.

| Area | Today |
|---|---|
| Execution | Deterministic price-time matching; `apply` reads no wall clock |
| Durability | Segmented WAL + atomic snapshots; snapshot every 100k entries or 30 minutes |
| Replication | Raft peer RPCs over gRPC |
| Symbol model | One symbol per process/cluster, configured and locked through Raft |
| Numeric model | Exact 18-decimal fixed point through matching, events, snapshots and feeds |
| Public edge | Bounded request/snapshot limits; loopback or private-network deployment |

## Table of Contents

- [Features](#features)
- [Architecture Overview](#architecture-overview)
- [Getting Started](#getting-started)
- [Running the Cluster](#running-the-cluster)
- [Security and Deployment Boundary](#security-and-deployment-boundary)
- [Project Structure](#project-structure)
- [Storage Implementation](#storage-implementation)
- [Cluster Management](#cluster-management)
- [API Reference](#api-reference)
- [Testing](#testing)
- [Performance and Current Limits](#performance-and-current-limits)
- [Contact](#contact)

## Features

Built for teams that need deterministic execution, durable recovery and a clean downstream feed — without treating the matching engine as a black box.

**Matching engine**
- **Price-time priority order book** with GTC, IOC, FOK, GTD, Day, market, post-only, iceberg, stop-market and stop-limit orders.
- **Amend and cancel** with deterministic priority rules: same-price reductions keep time priority; price or size increases re-queue under a new sequence.
- **Asset-aware fee deduction** with maker/taker rates, rebates and exact notional accounting; crypto/FX default to received-asset fees, ETF/equity always settle in quote.
- **Base or quote volume** with tick/lot enforcement, quote-limit normalization and quote-budget market sweeps.
- **Exact 18-decimal fixed point** from API input through matching, events, snapshots and the market-data feed — no `f64` arithmetic in the matching path.

**Durability and recovery**
- **Raft-replicated state machine** with durable membership, log and application state.
- **Segmented, checksummed WAL** with one fsync per replicated batch and torn-tail recovery.
- **Atomic snapshots** written by temp file → fsync → rename → directory fsync, with checksum validation before install.
- **Snapshot policy for real workloads**: every 100,000 applied entries or 30 minutes when newer entries exist, configurable with `--snapshot-interval-ms`.
- **Snapshot + WAL-tail recovery** that restores the latest snapshot, event window and idempotency map, then replays only newer log entries.
- **gRPC replication** with bidirectional `stream_append`, chunked snapshot transfer and independent per-peer channels.

**APIs and data feeds**
- **Durable match-event feed** at `POST /events` with cursor pagination, retention limits, `client_order_id` idempotency and replay-safe retries.
- **Deterministic market-data feed** with leader gating, shadow book, BBO, depth snapshots/deltas, lifecycle events, OHLC bars and a compact binary wire format.
- **Flexible read consistency**: local reads, linearizable leader reads and linearizable follower reads.
- **Leader-aware Rust client** that tracks and forwards requests to the current leader.
- **Operational APIs** for initialization, membership changes, metrics, event status and market-data status.
- **Batch writes** at `POST /write_batch`: multiple requests in one Raft entry and one WAL fsync, with per-request sequences and continuous event ordinals.

**Built to be trusted**
- **Bounded public edge**: request-body, read-key, event-page, idempotency-key and snapshot-size limits, with malformed input rejected before Raft.
- **Explicit internal-service contract**: loopback or trusted private networks, gateway-owned TLS/authentication, plus startup warnings for non-loopback binds.
- **Fail-fast transport health**: if the gRPC listener dies, `run()` stops the node and returns an error instead of leaving a half-healthy HTTP API serving peers that cannot reach it.
- **Correctness evidence**: randomized differential tests against a reference model, explicit order-type conformance cases, deterministic replay checks and cluster recovery tests.
- **One symbol per process/cluster** for isolated resources and failure domains; scale horizontally by running one deployment per symbol.

## Architecture Overview

This project extends the [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) with persistent storage and a gRPC transport:

```
                  ┌─────────────────────────────────────────────────────────┐
                  │                    Application Layer                    │
                  │              (Order Book Matching Engine)               │
                  │      apply ──► order book + replicated market_seq       │
                  │    MatchEvent ──► MatchResultSink (audit / clearing)    │
                  └─────────────────────┬───────────────────────────┬───────┘
                                        │                           │ MatchEvent + depth journal
                                        │                           │ try_send (never blocks)
                                        │                           ▼
                                        │             ┌──────────────────────────────────────┐
                                        │             │  Market-data task (leader-gated)   │
                                        │             │     shadow book · OHLC · wire v5     │
                                        │             │   deterministic market_seq/sub_seq   │
                                        │             └──────────────────────────────────────┘
                                        │                                 │ MarketDataSink
                                        │                                 ▼
                                        │             ┌──────────────────────────────────────┐
                                        │             │   JSONL now · Aeron 2001–2007 next   │
                                        │             │      TRADE BBO DEPTH LIFECYCLE       │
                                        │             │              STATUS BAR              │
                                        ▼             └──────────────────────────────────────┘
                  ┌─────────────────────────────────────────────────────────┐
                  │                      OpenRaft Core                      │
                  │          (Consensus, Replication, Leadership)           │
                  └────────────────────────────┬────────────────────────────┘
                                               │
                                               │
               ┌───────────────────────────────┴───────────────────────────────┐
               │                               │                               │
               ▼                               ▼                               ▼
    ┌─────────────────────┐         ┌─────────────────────┐         ┌─────────────────────┐
    │     Log + State     │         │     RaftNetwork     │         │      HTTP API       │
    │  (WAL + Snapshots)  │         │       (gRPC)        │         │     (Actix-web)     │
    └─────────────────────┘         └─────────────────────┘         └─────────────────────┘
```

Peer-to-peer Raft RPCs use gRPC on `port + 1000`. The public application and management API
stays on the `--http-addr` listener. Raft RPCs are not part of the HTTP JSON API.

Market-data egress is node-local and off the consensus path: the tap is a bounded `try_send`, so a
slow subscriber can never block `apply`. The JSONL sink is for debugging; the same wire-v5 messages
are what the Aeron adapter (streams 2001–2007) will publish next.

### Key Components

1. **RaftStorage Implementation** ([`store/`](./src/store/))
   - Raft log entries in a segmented write-ahead log (`log/*.wal`)
   - Vote and purge marker in an atomically rewritten `log/state` file
   - State machine in memory, made durable by atomic `snapshot/current.snap` files

2. **Network Layer** ([`network/`](./src/network/))
   - gRPC Raft RPCs (`src/network/grpc/`) for replication, voting and snapshot streaming
   - Separate replication, heartbeat and snapshot channels per peer
   - Management APIs for cluster administration

3. **Matching Engine** ([`order_book/`](./src/order_book/))
   - Price-time priority order book with bids and asks
   - Order types: GTC, IOC, FOK, GTD, Day, post-only, iceberg, stop-market and stop-limit
   - Deterministic expiry (`PurgeExpired` carries the timestamps through the log) and stop
     election on the replicated last trade price
   - The order book is the replicated state machine: deterministic `apply`, no clocks
   - `OrderBook::execute_order` is the full lifecycle; `place_order` is a fills-only wrapper

4. **Market-Data Egress** ([`market_data/`](./src/market_data/))
   - `apply` hands match events and a depth journal to a bounded tap (`try_send`; matching never waits)
   - A leader-gated tokio task keeps a shadow book, derives BBO/OHLC bars and encodes wire v5
   - Keys are `(symbol_id, stream_id, market_seq, sub_seq)`; one uncapped `TRADE` per taker
   - JSONL sink today; Aeron streams 2001–2007 are the next phase

## Getting Started

### Prerequisites

- Rust 1.89 or later (edition 2024; `actix-web` and `tonic` set the floor)
- Cargo

`protoc` is not required: the build script uses `protoc-bin-vendored` to compile
`proto/raft.proto`.

### Building

```shell
cargo build
```

> **Note**: Append `--release` for production builds, but this project is primarily intended as an example and not recommended for production use without further hardening.

## Running the Cluster

### Quick Start with Test Script

The easiest way to see the cluster in action is to run the provided test script:

```shell
./scripts/test-cluster.sh
```

This script builds, starts a 3-node cluster on `127.0.0.1:21001-21003` (gRPC Raft on
`22001-22003`), initializes it, adds learners, changes membership, and places an order and reads the
book back. It uses only `curl` against the HTTP API, showing the request/response flow. It stores
state in `/tmp/matchengine-raft-test-cluster` and clears that directory on start, so reruns are
reproducible; set `RAFT_DATA_DIR` to keep state somewhere else.

### Using the Rust Tests

```shell
cargo test
```

The integration target runs the cluster scenario through `MatchEngineClient`, exercises the gRPC
snapshot path, checks the WAL and snapshot formats, and restarts nodes to prove recovery.

See [Testing](#testing) for the individual test files.

### Manual Cluster Setup

#### 1. Start Nodes

Start the first node:

```shell
./target/debug/matchengine-raft --id 1 --http-addr 127.0.0.1:21001
```

Start additional nodes (in separate terminals):

```shell
./target/debug/matchengine-raft --id 2 --http-addr 127.0.0.1:21002
./target/debug/matchengine-raft --id 3 --http-addr 127.0.0.1:21003
```

Each node listens for gRPC Raft RPCs on `port + 1000` (`22001-22003` here). The HTTP listener is
reserved for the application and management API.

All state is written to `<data-dir>/<instance-prefix>-<id>/`; see
[Per-node data directory](#per-node-data-directory).

#### 2. Initialize the Cluster

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[]' \
  http://127.0.0.1:21001/init
```

`/init` takes a JSON array. An empty array (`[]`) initializes the receiving node as a single-node
cluster; a non-empty array of `[id, "host:port"]` pairs initializes a whole cluster at once.
A bare POST or `{}` is rejected by the deserializer. This initializes node 1 as the leader.

#### 3. Add Learners

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[2, "127.0.0.1:21002"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[3, "127.0.0.1:21003"]' \
  http://127.0.0.1:21001/add-learner
```

#### 4. Change Membership

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3]' \
  http://127.0.0.1:21001/change-membership
```

#### 5. Write and Read Orders

Place an order:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Place":{"order":{"live":"Limit","side":"Buy","price":4000.0,"volume":4.4,"id":1,"sequence":0}}}' \
  http://127.0.0.1:21001/write
```

Read the order book from any node:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"orderbook_orders"' \
  http://127.0.0.1:21002/read
```

You should be able to see the order from any node!

### Using the Helper Script

The project also includes `scripts/test.sh` for more convenient cluster management:

```shell
# Start a node
./scripts/test.sh start-node 1

# Initialize node 1 and add the other two as learners
./scripts/test.sh build-cluster 1

# Promote the learners to voters
./scripts/test.sh change-membership

# Check metrics
./scripts/test.sh metrics 1

# Place orders
./scripts/test.sh place-order 1

# Read the sequence counter
./scripts/test.sh get-seq 1

# Clean up (the data directory is renamed, not deleted)
./scripts/test.sh clean
```

`scripts/test.sh` keeps all state under `RAFT_DATA_DIR` (default `/tmp/matchengine-raft`), so
`place-order` results survive `start-node`/`kill` cycles.

## Security and Deployment Boundary

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
| `/write_batch` | 1–128 non-nested requests, unique `client_order_id`s |
| gRPC Raft message | 64 MiB (`MAX_MESSAGE_SIZE`) |
| gRPC snapshot stream | 256 MiB total (`MAX_SNAPSHOT_BYTES`), checked before each chunk is appended |

Malformed JSON and invalid fields return `400`; oversized bodies return `413`. The replicated
`client_order_id` dedupe map retains at most 262,144 records, so the key bound also bounds its
memory.

## Project Structure

```
src/
├── main.rs                  # Server entry point (flags, env, storage config)
├── client.rs                # Smart client with leader forwarding
├── lib.rs                   # Node composition, servers, shutdown
├── app.rs                   # Application state
├── order_book/
│   ├── mod.rs               # Order book matching engine
│   └── fixed.rs             # Exact fixed-point values and symbol metadata
├── market_data/
│   ├── mod.rs               # Tap, depth journal consumer, shadow book, deterministic publisher
│   └── wire.rs              # 64-byte binary header + message bodies (Aeron-ready)
├── match_result/
│   └── mod.rs               # MatchEvent ids, sinks, replicated event-log integration
├── network/
│   ├── api.rs               # Application HTTP endpoints
│   ├── management.rs        # Admin HTTP endpoints
│   ├── grpc/
│   │   ├── client.rs        # gRPC RaftNetworkV2 (append, vote, snapshots)
│   │   ├── server.rs        # gRPC Raft service
│   │   └── mod.rs           # Protobuf types and message-size limits
│   └── mod.rs               # Raft port derivation and transport limits
├── store/
│   ├── mod.rs               # Storage module
│   ├── log_store.rs         # Segmented WAL + vote/purge state
│   ├── state_machine.rs     # State machine, event log + snapshots
│   ├── types.rs             # Request/response types
│   └── config.rs            # Storage layout / per-node directories
proto/
└── raft.proto               # gRPC Raft service definition
examples/
├── sled.rs                  # Playground (local sled dir, not installed)
├── dump_wal.rs              # JSONL dump of WAL segments
└── dump_snapshot.rs         # JSONL dump of the current snapshot
tests/
└── cluster/
    ├── main.rs              # Single integration test target
    ├── common.rs            # Unique temp data dirs
    ├── test_cluster.rs      # 3-node gRPC cluster
    ├── test_snapshot.rs     # Snapshot build/purge, learner install + periodic timer
    ├── test_storage.rs      # WAL and snapshot formats
    ├── test_restart.rs      # Restart recovery: log replay, snapshot + WAL tail
    ├── test_match_results.rs # Match events in the JSONL file sink
    ├── test_market_data.rs    # Market-data feed: snapshot, trades, BBO, depth, bars
    ├── test_events.rs         # Event log: write events, cursor, retention, dedupe
    ├── test_membership.rs     # Leader failover + learner catch-up from a live log
    ├── test_late_join.rs      # Stale node rejoins and catches up from live members
    ├── test_exactness.rs      # 18-digit values through API, events, feed and snapshot restart
    ├── test_symbols.rs        # One-symbol config, precision rejection and symbol lock
    ├── test_fees.rs           # Maker/taker fees through API, events, feed and restart
    ├── test_fee_assets.rs     # QQQ/USD tick/lot, quote volume and quote fees
    ├── test_request_limits.rs # Hostile input and request-size limits
    └── test_batch_write.rs    # Batch writes in one Raft entry
benches/
├── matchengine.rs           # Order-book microbenchmarks
├── e2e.rs                   # Submit -> commit -> apply -> response
├── feed_load.rs             # Market-data feed load test
└── storage.rs               # WAL append/read/recovery
```

## Storage Implementation

### Write-ahead log (`src/store/log_store.rs`)

Raft log entries are appended to `log/NNNNNNNNNN.wal` segment files:

- Each record is framed as `[len: u32 LE][crc32: u32 LE][postcard payload]`. On open, recovery walks
  every segment and truncates a torn or corrupt tail instead of parsing it as a valid entry.
- `append()` writes the whole openraft batch with a single `write_all` followed by a single
  `sync_data`, so N entries replicated in one AppendEntries RPC cost one fsync.
- Reads use `pread` (`read_at`) against cached file handles, so replication can read while an
  append is in flight and no lock is held during disk IO.
- All blocking IO runs on `spawn_blocking`; the async runtime only encodes and decodes.
- Segments are 64 MiB by default and are deleted only when a purge fully covers them. The active
  segment is rotated before deletion, so a purge never leaves a hole.
- Each segment starts with an `MRFT` magic and a `u32` format version. `LogStore` implements
  `RaftLogStorage` (`get_log_state`, `read_vote`/`save_vote`, `read_committed`/`save_committed`,
  `append`, `truncate_after`, `purge`, `get_log_reader`).

Vote and the last purged log id are safety-critical and rare, so they live in a small `log/state`
file that is rewritten atomically (temp file → fsync → rename → directory fsync). The committed
log id is appended to the WAL without its own fsync and becomes durable with the next append,
avoiding a second fsync per batch.

### Snapshots (`src/store/state_machine.rs`)

The state machine (order book + sequence counter + retained match-event window + submission dedupe
map) stays in memory so applying entries is fast. Snapshot v9 is written as
`snapshot/current.snap` using the same atomic temp-file/rename protocol; `install_snapshot`
validates the incoming bytes before replacing the durable copy, so a bad snapshot cannot destroy a
good one.

The snapshot payload is postcard-encoded `StateMachineData`:

```rust
pub struct StateMachineData {
    pub orders: Vec<Order>,
    pub sequence: u64,
    pub stops: Vec<Order>,
    pub last_trade_price: Option<Fixed>,
    pub symbol_config: SymbolConfig, // the one symbol this process/cluster trades
    pub symbol_locked: bool,         // true after the first accepted order
    pub events: Vec<StoredMatchEvent>, // retained match events, oldest first
    pub events_evicted: u64,
    pub submissions: Vec<SubmissionRecord>, // client_order_id dedupe, oldest first
    pub submissions_evicted: u64,
}
```

`StoredMatchEvent` mirrors `MatchEvent` but always writes both optional fields; `MatchEvent` omits
absent options from JSON, which postcard cannot decode positionally. Event identity stays
`(log_index, ordinal)`, so no extra counter is stored. The file framing is:

```text
"MRFSNAP" | version u32 | crc32 u32 | postcard(SnapshotPayload)
```

### Per-node data directory

Each node writes to `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`. Configure it with:

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

The equivalent environment variables are `RAFT_DATA_DIR` and `RAFT_INSTANCE_PREFIX`; the defaults
are `/tmp/matchengine-raft` and `matchengine`.

### Order types

An order is one flat `Order` record; every optional type is a field on it. All new fields have serde
defaults, so payloads written before they existed still deserialize. The semantics follow
[OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs):

| Type | Field | Behaviour |
|---|---|---|
| GTC (default) | — | Rests until filled or cancelled. |
| IOC | `"tif": "IOC"` | Executes what crosses now; the remainder is cancelled and reported by a `cancelled` event. |
| FOK | `"tif": "FOK"` | A preflight sums the reachable visible + hidden depth at or inside the limit. Unless the whole order fits, it is refused (`rejected`, reason `fok_unfillable`) and the book is untouched. |
| GTD | `"tif": {"GTD": 1700000000000}` (Unix ms) | Rests until a mutating request's intake timestamp reaches the deadline (automatic purge) or an explicit `PurgeExpired` evicts it. |
| Day | `"tif": "DAY"` | Expires when `PurgeExpired` is called with `day_close_ms` and `now_ms >= close`; without a close it behaves like GTC. |
| Post-only | `"post_only": true` | Never takes liquidity: it rests in full, or is refused (`post_only_would_cross`) when it would trade. |
| Iceberg | `"display_volume": 100.0` | `volume` is the total; only one display tranche rests. The rest waits in `hidden_volume`, invisible to depth, and is refilled tranche by tranche as the visible part is consumed. |
| Stop-market | `"live": "Market", "stop_price": 95.0` | Parks off the book until a trade at or through the stop price (sell: `<=` stop, buy: `>=` stop), then sweeps every level. The child never rests: its remainder is cancelled. |
| Stop-limit | `"live": "Limit", "price": 96.0, "stop_price": 95.0` | Same election, child bounded by `price`; the remainder is cancelled. |

Market orders (`"live": "Market"`) ignore their own price, sweep the opposite side and cancel the
remainder; they never rest.

Expiry is deterministic: `apply` never reads a clock. Every admissible mutating request
(`Place`, `Cancel`, `Amend`) runs the automatic GTD purge at its intake timestamp before it is
applied, so a taker cannot trade against liquidity that is already due. `Day` still needs a market
close, so only the explicit request evicts it:

```json
{"PurgeExpired": {"now_ms": 1700000000000, "day_close_ms": null}}
```

`PurgeExpired` itself consumes a market sequence; its response is that sequence. Every evicted
order emits an `expired` event with its still-open volume.

A stop is only elected by a *new* trade: one the last trade price already crosses is refused
(`stop_would_trigger`) instead of turning into a market order on entry. One trade can cascade
through several stops; elected stops execute in admission order.

Unsupported combinations (a stop that is also post-only or an iceberg, a post-only market order, a
zero/negative size or price, a GTD deadline not after the order's own `timestamp`) are refused with
a `rejected` event carrying the reason.

### Order amendment

`Amend` changes the price and/or open volume of a resting limit order. The request identifies the
order exactly like `Cancel` — side, current price and sequence — and each new field is optional:

```json
{"Amend": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 7}, "price": 4002.0, "volume": 3.0}}
```

- A same-price reduction keeps the order's sequence, so it keeps time priority.
- A new price or a larger volume re-inserts the order with a new sequence (it loses priority); the
  `/write` response carries the sequence to use from then on.
- An amendment whose new price would cross the opposite side is refused (`amend_would_cross`): a
  resting book never crosses, so taking liquidity is still cancel + place.
- Icebergs, parked stops and market orders are not amendable (`unsupported_order`); cancel and
  re-place them instead.
- A successful amendment emits an `amended` event carrying both the previous and the new order
  sequence; the `/write` response is the amendment's `market_seq` (the new order sequence when the
  order was re-inserted). A refused amendment still consumes a sequence and publishes a `rejected`
  event with the reason.

### Match-result processing

`apply` turns every `MatchResult` into a `MatchEvent` (`fill`, `rested`, `cancelled`, `triggered`,
`expired`, `rejected` or `amended`). Events are derived from the applied log entry alone, so every
replica produces the same ids. Bounded batches are returned in the `/write` response and appended
to a replicated, snapshotted event log; the cursor API serves clearing and audit consumers:

```
POST /write { Place | Cancel | Amend | PurgeExpired }
        │
        ▼
  StateMachineStore::apply  (every replica)
        │
        ├──► order book + sequence + event window ──► snapshot   [replicated state]
        │
        ├──► MatchEvent batch ──► /write response                [capped at 512]
        │
        └──► POST /events + GET /events/status                   [cursor + retention]
                    │
                    ▼
          MatchResultSink                                     [node-local, best effort]
            ├── NoopSink      default: no local side effect
            └── FileLogSink   --match-event-log -> JSONL
```

`Place` accepts an optional `client_order_id`; retrying it returns the original sequence and
events instead of placing a second order. The event window is bounded by `--max-events` (env
`MAX_EVENTS`, default 262144); the submission map has a separate fixed 262144-record cap so retry
resolution never depends on a node-local flag. Both are included in snapshot v9. `GET /events/status` reports the
oldest/last retained id and eviction counters; consumers resume from `next_after` and dedupe on
`event_id`.

The file sink remains a debugging aid: node-local, not replicated or fsynced, and replayed entries
are written again after a restart. To inspect events locally:

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --match-event-log /tmp/node1-events.jsonl
```

Each line is one event, for example
`{"node_id":1,"event_id":{"log_index":42,"ordinal":0},"kind":"fill","order_sequence":7,"counterparty_sequence":3,"price":100.0,"volume":10.0}`.

The replicated event log is the durable clearing feed: it survives restarts and learner snapshots,
and retries are collapsed by `client_order_id`. See `docs/MATCH_RESULTS.md` for the consumer
contract.

Cursor example:

```json
POST /events
{"after": {"log_index": 100, "ordinal": 3}, "limit": 500}
```

```json
{"Ok": {
  "events": [ ... ],
  "next_after": {"log_index": 140, "ordinal": 1},
  "oldest_event_id": {"log_index": 1, "ordinal": 0},
  "last_event_id": {"log_index": 140, "ordinal": 1},
  "has_more": true
}}
```

Omitting `after` starts at the oldest retained event; `limit` must be between 1 and 1000. If your cursor is
older than `oldest_event_id`, the window has evicted events and clearing must resync out of band.
`GET /events/status` reports `retained_events`, `evictions_total`, `retention_limit`,
`submission_retention_limit` and `submission_evictions_total`.

### Market-data feed (opt-in)

`--market-data-log <file>` (env `MARKET_DATA_LOG`) turns on the deterministic market-data egress
designed in `docs/AERON_SOLUTION.md` Part II and implemented in `src/market_data/`:

- the matching path only appends the visible-depth changes it already computes (a transient
  journal) and copies the entry's `MatchEvent`s into a bounded queue — no book scans, encoding or
  IO on the matching path;
- a dedicated tokio task keeps a shadow book and publishes **deterministically sequenced** messages:
  all fills of one taker in one `TRADE` message (uncapped), BBO (`last`/`mid`/spread) per
  book-changing frame, depth snapshots and post-batch deltas, lifecycle events, OHLC bars (1 s and
  1 m by default) and a status message every 100th market sequence;
- every message carries `(symbol_id, stream_id, market_seq, sub_seq)`. `market_seq` is the replicated
  mutation sequence (the taker order sequence for `Place`; `Cancel`, `Amend` and `PurgeExpired`
  consume one), so the key is identical on leader and followers, across failover and restart.
  Downstream deduplicates on that tuple; `(log_index, ordinal)` remains the audit position;
- only the leader publishes by default (`--market-data-leader-only=false` feeds every replica);
  every leadership gain starts with a reset snapshot whose boundary is the next `market_seq`. A full
  queue drops frames and emits a `GAP`; `apply` is never blocked or slowed by a subscriber.

The shipped sink is JSONL for debugging. The same messages have a binary encoding in
`src/market_data/wire.rs` (64-byte header, version 5, scale-18 `i128` bodies), and `MarketDataSink`
is the seam the Aeron adapter will implement (next phase). Every message carries the configured
process `symbol_id`, so separate per-symbol deployments can share a bus. Tuning: `--market-data-depth`, `--market-data-status-every`,
`--market-data-bar-intervals-ms`, `--market-data-leader-only`.

`/read marketdata_status` returns the feed counters and the leader gate.

### Snapshot configuration

`default_raft_config()` (`src/lib.rs`) sets `snapshot_policy = LogsSinceLast(100_000)` and the
node also runs a 30-minute snapshot timer (`StorageConfig::snapshot_interval`, default
`DEFAULT_SNAPSHOT_INTERVAL`). The timer only requests a snapshot when new entries have been
applied since the last one, so an idle node does not rewrite the same file. `--snapshot-interval-ms`
(env `SNAPSHOT_INTERVAL_MS`) overrides the interval; `0` disables the timer while leaving the
100,000-entry policy active. Tests that need a snapshot quickly override the policy:

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // purge everything the snapshot covers
```

- A snapshot is built after the configured number of applied log entries (100,000 by default),
  or after 30 minutes when newer entries exist.
- Logs covered by a snapshot plus `max_in_snapshot_log_to_keep` are purged. The snapshot test sets
  both values so a joining learner cannot catch up by log replication and must install a snapshot.

### Data recovery

On startup the node:
1. Recovers the WAL index, vote and purge marker (discarding a torn tail).
2. Selects and loads `snapshot/current.snap` if it exists. The file is replaced atomically, so it
   is always the latest complete snapshot; a crash leaves only an ignored `.tmp` file.
3. Sets the state machine's `last_applied` from the snapshot metadata and lets openraft replay
   retained WAL entries whose log index is greater than the snapshot's last log id.

A single-node cluster therefore comes back with all live orders, the sequence counter, the retained
event window and the idempotency map, whether recovery is pure log replay or snapshot + newer WAL
tail. Both paths are covered by `tests/cluster/test_restart.rs`.

### Dumping durable state

The storage examples are read-only and validate the WAL/snapshot CRCs:

```shell
cargo run --example dump_wal -- <node-dir|log-dir|segment.wal>
cargo run --example dump_snapshot -- <node-dir|snapshot-dir|current.snap>
```

`dump_wal` prints one JSON object per WAL record (`entry` or `committed`) with file and offset;
`dump_snapshot` prints a metadata line, a state line, then one line per order, stop, event or
submission. Both write JSONL to stdout, so they compose with `jq` and other line-oriented tools.

## Cluster Management

Adding a node to a cluster involves 3 steps:

1. **Write the node info** through the Raft protocol to storage
2. **Add as Learner** to let it start receiving replication data from the leader
3. **Change membership** to promote the learner to a full voting member

> **Note**: Raft itself does not store node addresses. This implementation stores node information in the storage layer, and the network layer references the store to lookup target node addresses.

## API Reference

The HTTP application and management endpoints speak JSON. Every handler returns a `Result`
wrapper, so a successful call looks like `{"Ok": ...}` and a forwarded or failed call like
`{"Err": ...}`. Raft peer RPCs use gRPC, not the HTTP JSON API.

### Raft gRPC API

Raft peer-to-peer RPCs are served only by gRPC on `--http-addr` port + 1000. No Raft RPC is
mounted on the HTTP JSON listener:

- `AppendEntries` and `Vote` (unary)
- `InstallSnapshot` (client-streaming chunks)
- `AppendStream` (bidirectional pipelined appends)

See `proto/raft.proto`. Unary append/vote use `RPCOption::soft_ttl`, snapshots stream in 256 KiB
chunks, and both message directions are capped at 64 MiB. `Unavailable` maps to
`RPCError::Unreachable`, timeouts to `RPCError::Timeout`, and other statuses to
`RPCError::Network`.

### Admin APIs

- `POST /init` - Initialize a cluster; body is `[]` for the receiving node alone, or
  `[[1, "127.0.0.1:21001"], ...]` for an explicit membership
- `POST /add-learner` - Add a learner node; body is `[id, "host:port"]`
- `POST /change-membership` - Change the voter set; body is `[1, 2, 3]`
- `GET /metrics` - Get Raft metrics
- `POST /get_linearizer` - Fetch a read linearizer from the leader (used by `/follower_read`)

### Application APIs

- `POST /write` - Submit a `Place`, `Cancel`, `Amend`, `PurgeExpired` or `ConfigureSymbol` request
  through Raft. Order/mutation requests return the assigned `market_seq` (for a `Place`, also the
  order sequence) plus the entry's first events (capped at 512). `ConfigureSymbol` returns a status
  string and uses the `error` field on refusal; it does not consume a market sequence. `Place`
  accepts an optional `client_order_id` for idempotent retries. A `Batch { requests }` envelope is
  also accepted; `/write_batch` takes the same requests as a bare JSON array.
- `POST /write_batch` - Submit 1–128 non-nested requests in one Raft entry. The response contains a
  `batch` array with one response per request; event ordinals are continuous across the entry.
- `POST /read` - Read from the local state machine (may be stale)
- `POST /linearizable_read` - Linearizable read; the request must reach the leader
- `POST /follower_read` - Linearizable read served locally after fetching a linearizer from the leader
- `POST /events` - Read a page of the replicated match-event log (`after`, `limit`); linearizable and leader-only
- `GET /events/status` - Retained event window, eviction counters and retention limit

The Rust request/response types behind `/write` are:

```rust
pub enum MatchEngineRequest {
    Place { order: Order, client_order_id: Option<String> },
    Cancel { order: Order },
    Amend { order: Order, price: Option<Fixed>, volume: Option<Fixed> },
    PurgeExpired { now_ms: u64, day_close_ms: Option<u64> },
    ConfigureSymbol { config: SymbolConfig },
    Batch { requests: Vec<MatchEngineRequest> },
}

pub struct MatchEngineResponse {
    pub value: Option<String>,        // sequence string for compatibility
    pub sequence: Option<u64>,        // assigned market_seq
    pub events: Vec<MatchEvent>,      // first 512 events of the entry
    pub events_truncated: bool,       // true when more events exist
    pub error: Option<String>,        // non-fatal application error
    pub batch: Vec<MatchEngineResponse>, // per-request responses for /write_batch
}
```

Every order/mutation request consumes one `market_seq`; `Place` also uses it as the order
sequence, while `ConfigureSymbol` does not consume one. The response carries at most 512 events;
the full batch is available from `/events`. Fill events carry
`quote_volume`, `volume_unit`, `taker_fee`/`taker_fee_asset` and `maker_fee`/`maker_fee_asset`;
the same fields are on market-data `TRADE` and fill `LIFECYCLE` records (wire v5).
`client_order_id` is optional and only affects `Place`: a retry returns the original sequence and
events instead of placing a second order. It must be 1–128 printable ASCII bytes from
`[A-Za-z0-9._:-]`; an empty value is rejected, so omit the field to disable idempotency.

`/write_batch` executes sub-requests in order in one `apply` call, so later items see the effects of
earlier ones. Each item keeps its own sequence and events; all events share the entry's log index
with continuous ordinals. A rejected sub-request does not abort the rest of the batch. Empty
batches, nested batches, oversized batches and duplicate `client_order_id`s within one batch are
rejected before Raft.

`/read`, `/linearizable_read` and `/follower_read` accept a JSON string key naming a view of the
order book:

- `orderbook_orders` - all orders resting in the book as a JSON array
- `orderbook_stops` - the stop orders parked off the book, in admission order
- `orderbook_last_trade_price` - the last trade price (`null` before the first trade), which elects stops
- `orderbook_depth` - aggregated visible depth for the top 10 price levels per side
  (`orderbook_depth:N` selects another count, capped at 200)
- `orderbook_spread` - best ask minus best bid (`null` while either side is empty)
- `marketdata_status` - market-data feed counters and gate state (`{}` when the feed is off)
- `orderbook_sequence` - the order sequence counter (`orderbook_sequance` is accepted as a legacy
  alias)
- `symbol_config` - this process/cluster's replicated symbol and precision configuration
- `symbol_id` - the one symbol this process/cluster trades

Prices and quantities are stored internally as exact 18-scale fixed-point integers. JSON accepts
either decimal strings or legacy numbers; match events, `/read` and the market-data feed emit
decimal strings so 18-decimal values round-trip exactly. A process/cluster trades exactly one
symbol: `SymbolConfig` is replicated and snapshotted with base/quote `AssetId`/`AssetClass`
metadata, price tick, base lot and fee-asset policy. Prices/quantities with excess precision or
wrong increments are rejected, and orders routed to another `symbol_id` are refused. Scaling to
several symbols means running one process/cluster per symbol, so resources and failures stay
isolated. Values are normalized to the common 18-decimal scale.

Example order payload accepted by `/write` (note the field name `sequence`):

```json
{"Place": {"order": {"live": "Limit", "side": "Buy", "volume_unit": "base", "price": "4000.25", "volume": "4.4", "maker_fee_rate": "0.001", "taker_fee_rate": "0.002", "id": 1, "sequence": 1}, "client_order_id": "order-123"}}
```

`client_order_id` is optional; omit it for the old non-idempotent behaviour. `volume_unit` is
`base` by default; `quote` makes `volume` a gross quote amount (limit orders normalize at the limit
price, market orders use a quote budget/proceeds sweep).

Set this process's symbol, assets and accepted precision before trading:

```json
{"ConfigureSymbol": {"config": {
  "symbol_id": 0,
  "base_asset": {"code": "QQQ", "class": "etf", "decimals": 4},
  "quote_asset": {"code": "USD", "class": "fiat", "decimals": 2},
  "price_decimals": 2,
  "price_tick": "0.01",
  "base_lot": "0.0001",
  "fee_asset_policy": "quote"
}}}
```

The configuration is replicated and locked once the first order is accepted. Changing it while
orders are open, or after trading has started, is refused. A successful change on a publishing
node also queues a market-data reset snapshot for the new symbol. The response uses the optional
`error` field for this non-fatal case; the write itself is still committed.

Fee rates (`maker_fee_rate`, `taker_fee_rate`) are signed `Fixed` fractions supplied by the
gateway from the user level. `"0.001"` means 10 bps and negative values are rebates. They are
optional and default to zero. The engine computes each fill's notional from the maker price and
matched quantity, then charges the taker order's taker rate and the maker order's maker rate.
Fees are truncated toward zero at 18 places; `fee_overflow` is set when the notional or a fee
exceeds the supported fixed-point range. The engine records fees but does not move balances.

Base/quote volume semantics and asset-aware fee deduction for fiat, stablecoin, crypto, ETF and
equity pairs are implemented. ETF/equity bases always settle fees in the quote asset; crypto/spot
and fiat-to-fiat pairs default to the received asset, and crypto can explicitly select quote or
base. The full model and open extensions (sessions, corporate actions, fixed/regulatory fees, fee
tokens) are in [`docs/FEE_DEDUCTION.md`](./docs/FEE_DEDUCTION.md).

Every other order-type field (`tif`, `post_only`, `stop_price`, `display_volume`, `hidden_volume`,
`timestamp`) is optional and defaults to "plain GTC limit order":

```json
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": 4000.0, "volume": 100.0, "tif": "IOC", "display_volume": 10.0}}}
```

The matching engine overwrites `sequence` when it applies the order, and the receiving leader
stamps `timestamp` at intake, so both values clients send are ignored (`0` is the conventional
placeholder). `sequence` is still required by the deserializer.

## Testing

`cargo test` builds one integration target (`tests/cluster`) plus unit tests in the library.

| Test | What it proves |
|---|---|
| `src/order_book/property_tests.rs` | Randomized differential tests against a reference model plus mixed GTC/IOC/FOK/GTD/Day/market/post-only/iceberg/stop orders and positive, zero and rebate fee rates under all fee-asset policies: visible book, fills, priority, no crossed book, volume conservation, sequence/last-trade state and deterministic replay |
| `src/order_book/conformance_tests.rs` | Explicit market-FOK, no-liquidity market, hidden-iceberg cancel/expiry, parked-stop GTD/Day expiry and fee-reporting cases for market, IOC and stop-child fills |
| `tests/cluster/test_cluster.rs` | 3-node gRPC cluster: init, learner, membership, write/read, linearizable read, leader forwarding, membership shrink; owns and shuts down every node |
| `tests/cluster/test_snapshot.rs` | Snapshot creation, log purge, a learner installing the purged state over the gRPC snapshot stream, and the periodic snapshot timer with the log-count policy disabled |
| `tests/cluster/test_storage.rs` | WAL framing, torn-tail truncation, segment rotation, a 20k-record multi-segment reopen, truncate/purge across segments, vote/committed persistence, snapshot build/install/reopen, corrupt-file rejection |
| `tests/cluster/test_restart.rs` | Full node restart recovering the order book, sequence, event window and idempotency map via WAL replay and via snapshot + newer WAL tail; also 2k-order large-log replay and a multi-chunk (~120 KB) snapshot boot |
| `tests/cluster/test_match_results.rs` | A node started with `--match-event-log` records rest/fill/cancel events in log order while the `/write` response stays unchanged; a second node checks stop triggers, GTD expiry and the new `/read` keys |
| `tests/cluster/test_market_data.rs` | A node started with `--market-data-log` publishes a reset snapshot, one trade message per taker, BBO, post-entry depth totals and a closed OHLC bar; the assertions check `market_seq` anchors and `/read marketdata_status` counters |
| `tests/cluster/test_events.rs` | `/write` returns events, `/events` pages by cursor, `client_order_id` retries do not duplicate an order, and `--max-events` eviction is visible in `/events/status` |
| `tests/cluster/test_membership.rs` | Kill the elected leader: a surviving voter takes over with the committed book and accepts new writes. A learner added after the writes catches up through normal log replication, is promoted, and writes through its own node |
| `tests/cluster/test_late_join.rs` | A node with a stale snapshot/WAL is removed, misses 40 writes while the leader snapshots and purges, then rejoins as a learner and catches up from the live cluster |
| `tests/cluster/test_exactness.rs` | An 18-fractional-digit price and quantity round-trip through `/write`, `/read`, `/events`, the JSONL market-data feed, snapshot v9 and a restart without passing through `f64` |
| `tests/cluster/test_symbols.rs` | `ConfigureSymbol` sets the process symbol/precision, rejects over-precision and cross-symbol orders, locks the configuration once trading starts, and survives snapshot restart |
| `tests/cluster/test_fees.rs` | Maker and taker fee rates produce exact fill fees in `/write`, `/events`, the file sink and the market-data feed, and survive snapshot restart |
| `tests/cluster/test_fee_assets.rs` | QQQ/USD ETF/fiat metadata, tick/lot enforcement, quote-volume limit normalization and quote-asset fee deduction |
| `tests/cluster/test_request_limits.rs` | Oversized JSON bodies, malformed input, invalid/oversized `client_order_id`s, oversized read keys and out-of-range event pages are rejected with `400`/`413` before Raft changes the book |
| `tests/cluster/test_batch_write.rs` | Multiple requests are applied in one Raft entry with per-item sequences, continuous event ordinals, and edge validation for empty, nested, oversized and duplicate-id batches |

Every integration test uses a unique temporary data directory. The cluster test uses fixed ports
`21001-21003` (gRPC `22001-22003`), the snapshot tests `21101-21103`, the restart tests
`21201-21205`, the match-result tests `21301`/`21302`, the market-data test `21303`, the event-log test
`21304`, the failover test `21401-21403`, the learner catch-up test `21501-21502`, the late-join
test `21601-21603`, the exactness test `21701` (gRPC `22701`), the symbol-config test `21801` (gRPC `22801`), the fee test `21901` (gRPC `22901`), the
fee-asset test `21902` (gRPC `22902`), the request-limit test `21903` (gRPC `22903`) and the batch-write test `21904` (gRPC `22904`); stop
leftover processes
(`pkill -f matchengine-raft`) before rerunning.

## Performance and Current Limits

Measured baseline (Apple M4, Rust 1.93.1, release bench profile; full methodology in
[`docs/PERFORMANCE.md`](./docs/PERFORMANCE.md)):

| Workload | Result |
|---|---:|
| Order book: sweep 1k asks / 100 levels | 105.6 µs |
| End-to-end `/write` p50 / p99 | 3.99 ms / 5.17 ms |
| Concurrent `/write` throughput (1,000 in flight) | 18,350 writes/s |
| Sequential `/write_batch` (128 orders/batch) | 25,183 orders/s |
| Concurrent `/write_batch` (100 × 32 orders) | 157,035 orders/s |
| Market-data feed at 500k fills/s | 495k fills/s, 0 drops at 8,192-frame queue |
| WAL append / read (20k entries, 100-entry batches) | 26,049 / 2,386,089 entries/s |

Batch writes amortize one WAL append/fsync across up to 128 orders: sequential 128-order batches
reach 25,183 orders/s, and 100 concurrent 32-order batches reach 157,035 orders/s.

The feed also has a deliberate 2M fills/s overload case that drops frames and emits `GAP` rather
than blocking matching. Reproduce the baseline with `cargo bench --bench matchengine`,
`cargo bench --bench e2e`, `cargo bench --bench feed_load` and `cargo bench --bench storage`.

Implemented performance choices:

- One `write_all` + one `sync_data` per replicated append batch, so N entries in one AppendEntries
  RPC cost one fsync.
- `pread` reads against cached file handles, so replication can read while an append is in flight.
- The order book, event window and dedupe map stay in memory; `apply` does no disk IO.
- gRPC uses bidirectional `AppendStream` for pipelined replication and separate replication,
  heartbeat and snapshot connections per peer.
- Snapshots stream in 256 KiB chunks; `log/state` and `snapshot/current.snap` are replaced
  atomically.
- The event window is bounded by `--max-events` and paged through `/events`; `/read
  orderbook_orders` does not clone it.
- WAL appends avoid per-entry clones, recovery scans segments sequentially, contiguous log reads share one `pread`, snapshot encoding/writing runs outside the state-machine lock, and snapshot bodies stream to disk through an incremental CRC writer.
- A gRPC server failure is fatal to `run()`: HTTP is stopped, background tasks are joined and Raft is shut down before the error is returned. HTTP requests are logged once per request.

Current limits and known gaps:

- TLS/mTLS and API authentication are intentionally gateway responsibilities; this process is an internal service.
- Snapshot installation needs one contiguous buffer, bounded by 256 MiB per snapshot.
- One process/cluster trades one symbol; run one deployment per symbol to scale.

Data-format note: `Order` prices/quantities, `last_trade_price`, match events, depth/BBO/spread, the
JSONL feed and wire v5 use exact 18-scale fixed-point integers. Snapshot **v9** and wire **v5** are
breaking changes from earlier builds, so recreate data directories when upgrading. See
[`docs/DECIMAL.md`](./docs/DECIMAL.md).

Known gaps and the roadmap are tracked in [`TODO.md`](./TODO.md).

## Contact

- Google Chat: [rshen.sy@gmail.com](https://chat.google.com/u/0/dm/rshen.sy@gmail.com)

## Credits

This project is a fork from [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the [OpenRaft](https://github.com/datafuselabs/openraft) project.

Special thanks to the Databend community and Zhang Yanpo for their support.

## License

MIT — see [`LICENSE`](./LICENSE).

The project started as a fork of
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the
[OpenRaft](https://github.com/datafuselabs/openraft) project, which is dual-licensed under MIT or
Apache-2.0. The original copyright and license notices continue to apply to code derived from it;
this repository is distributed under the MIT license in `LICENSE`.
