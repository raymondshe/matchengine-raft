# Match Engine Raft

A practical implementation of a distributed matching engine built upon [OpenRaft](https://github.com/datafuselabs/openraft). Raft logs are persisted in a hand-written segmented write-ahead log and the state machine is made durable with atomic snapshots, so a node recovers all committed state after a restart.

> **Note**: For a more detailed guide in Chinese, please refer to [GUIDE_cn.md](./GUIDE_cn.md).

## Table of Contents

- [Features](#features)
- [Architecture Overview](#architecture-overview)
- [Getting Started](#getting-started)
- [Running the Cluster](#running-the-cluster)
- [Project Structure](#project-structure)
- [Storage Implementation](#storage-implementation)
- [Cluster Management](#cluster-management)
- [API Reference](#api-reference)
- [Testing](#testing)

## Features

- **Persistent Raft Storage**: A segmented, checksummed write-ahead log with one fsync per replicated batch, plus atomic file snapshots for the state machine
- **Crash Recovery**: A restarted node loads its snapshot and replays the retained log tail; recovery is covered by integration tests
- **gRPC Replication**: A [tonic](https://docs.rs/tonic) transport with bidirectional `stream_append`, chunked snapshot transfer and separate per-peer channels; HTTP/JSON remains selectable with `--transport http`
- **Matching Engine**: Includes a simple order book matching engine as an example application
- **HTTP Server**: Built with [Actix-web](https://docs.rs/actix-web) providing both internal Raft APIs and external application APIs
- **Read Consistency Options**: Local reads, linearizable reads through the leader, and linearizable follower reads
- **Smart Client**: A Rust client that automatically tracks and redirects requests to the current leader

## Architecture Overview

This project extends the [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) with persistent storage and a gRPC transport:

```
                    ┌─────────────────────────────────────────┐
                    │           Application Layer              │
                    │  (KV Store + Order Book Matching Engine)│
                    └─────────────────────┬───────────────────┘
                                          │
                    ┌─────────────────────▼───────────────────┐
                    │           OpenRaft Core                  │
                    │  (Consensus, Replication, Leadership)   │
                    └─────────────────────┬───────────────────┘
                                          │
              ┌───────────────────────────┼───────────────────────────┐
              │                           │                           │
    ┌─────────▼─────────┐     ┌─────────▼─────────┐     ┌─────────▼─────────┐
    │  Log + State      │     │   RaftNetwork     │     │   HTTP API        │
    │ (WAL + Snapshots) │     │  (gRPC / HTTP)    │     │  (Actix-web)      │
    └───────────────────┘     └───────────────────┘     └───────────────────┘
```

Peer-to-peer Raft RPCs use gRPC on `port + 1000` by default. The public API and the HTTP Raft
handlers stay on the `--http-addr` listener, so a node serves both transports regardless of which
one it dials.

### Key Components

1. **RaftStorage Implementation** ([`store/`](./src/store/))
   - Raft log entries in a segmented write-ahead log (`log/*.wal`)
   - Vote and purge marker in an atomically rewritten `log/state` file
   - State machine in memory, made durable by atomic `snapshot/current.snap` files

2. **Network Layer** ([`network/`](./src/network/))
   - gRPC Raft RPCs (`src/network/grpc/`) for replication, voting and snapshot streaming
   - Legacy HTTP/JSON Raft RPCs (`src/network/raft.rs`) selectable at runtime (`src/network/transport.rs`)
   - Separate replication, heartbeat and snapshot channels per peer
   - Management APIs for cluster administration

3. **Matching Engine** ([`matchengine/`](./src/matchengine/))
   - Order book with bids and asks
   - Example of a real-world state machine application

## Getting Started

### Prerequisites

- Rust 1.85 or later (the crate uses edition 2024)
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
./test-cluster.sh
```

This script builds, starts a 3-node cluster on `127.0.0.1:21001-21003` (gRPC Raft on
`22001-22003`), initializes it, adds learners, changes membership, and writes/reads data. It uses
only `curl` against the HTTP API, showing the request/response flow. It stores state in
`/tmp/matchengine-raft-test-cluster` and clears that directory on start, so reruns are
reproducible; set `RAFT_DATA_DIR` to keep state somewhere else.

### Using the Rust Tests

```shell
cargo test
```

The integration target runs the cluster scenario through `MatchEngineClient`, exercises the gRPC
snapshot path, checks the WAL and snapshot formats, and restarts nodes to prove recovery. Run it
over the legacy transport with:

```shell
MATCHENGINE_TEST_TRANSPORT=http cargo test --test cluster
```

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

Each node listens for gRPC Raft RPCs on `port + 1000` (`22001-22003` here). Use
`--transport http` to make a node dial peers over HTTP/JSON instead; the HTTP Raft endpoints are
always mounted, so mixed-transport clusters work during migration.

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

#### 5. Write and Read Data

Write data:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Set":{"key":"foo","value":"bar"}}' \
  http://127.0.0.1:21001/write
```

Read data from any node:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"foo"' \
  http://127.0.0.1:21002/read
```

You should be able to read the data from any node!

### Using the Helper Script

The project also includes `test.sh` for more convenient cluster management:

```shell
# Start a node
./test.sh start-node 1

# Initialize node 1 and add the other two as learners
./test.sh build-cluster 1

# Promote the learners to voters
./test.sh change-membership

# Check metrics
./test.sh metrics 1

# Place orders
./test.sh place-order 1

# Read the sequence counter
./test.sh get-seq 1

# Clean up (the data directory is renamed, not deleted)
./test.sh clean
```

`test.sh` keeps all state under `RAFT_DATA_DIR` (default `/tmp/matchengine-raft`), so
`place-order` results survive `start-node`/`kill` cycles.

## Project Structure

```
src/
├── bin/
│   ├── main.rs              # Server entry point (flags, env, storage config)
│   └── sled_test.rs         # Playground binary (gitignored local sled dir)
├── client.rs                # Smart client with leader forwarding
├── lib.rs                   # Node composition, servers, shutdown
├── app.rs                   # Application state
├── matchengine/
│   └── mod.rs               # Order book matching engine
├── match_result/
│   └── mod.rs               # MatchEvent ids + pluggable MatchResultSink (noop/memory/file)
├── network/
│   ├── api.rs               # Application HTTP endpoints
│   ├── management.rs        # Admin HTTP endpoints
│   ├── raft.rs              # Raft internal HTTP RPC endpoints
│   ├── raft_network_impl.rs # HTTP/JSON RaftNetworkV2 implementation
│   ├── transport.rs         # Runtime transport selection (gRPC or HTTP)
│   ├── grpc/
│   │   ├── client.rs        # gRPC RaftNetworkV2 (append, vote, snapshots)
│   │   ├── server.rs        # gRPC Raft service
│   │   └── mod.rs           # Protobuf types and message-size limits
│   └── mod.rs               # Transport enum, Raft port derivation
├── store/
│   ├── mod.rs               # Storage module
│   ├── log_store.rs         # Segmented WAL + vote/purge state
│   ├── state_machine.rs     # In-memory state machine + snapshots
│   ├── types.rs             # Request/response types
│   └── config.rs            # Storage layout / per-node directories
proto/
└── raft.proto               # gRPC Raft service definition
tests/
└── cluster/
    ├── main.rs              # Single integration test target
    ├── common.rs            # Unique temp data dirs
    ├── test_cluster.rs      # 3-node cluster, both transports
    ├── test_snapshot.rs     # Snapshot build/purge + learner install over gRPC
    ├── test_storage.rs      # WAL and snapshot formats
    ├── test_restart.rs      # Restart recovery: log replay and snapshot
    └── test_match_results.rs # Match events in the JSONL file sink
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

Vote and the last purged log id are safety-critical and rare, so they live in a small `log/state`
file that is rewritten atomically (temp file → fsync → rename → directory fsync). The committed
log id is appended to the WAL without its own fsync and becomes durable with the next append,
avoiding a second fsync per batch.

### Snapshots (`src/store/state_machine.rs`)

The state machine (KV map + order book + sequence counter) stays in memory so applying entries is
fast. Snapshots are written as `snapshot/current.snap` (magic + version + CRC32 + postcard metadata
+ postcard state) using the same atomic temp-file/rename protocol. `install_snapshot` validates the
incoming bytes before replacing the durable copy, so a bad snapshot cannot destroy a good one.

### Per-node data directory

Each node writes to `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`. Configure it with:

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

The equivalent environment variables are `RAFT_DATA_DIR` and `RAFT_INSTANCE_PREFIX`; the defaults
are `/tmp/matchengine-raft` and `matchengine`.

### Match-result processing

`apply` turns every `MatchResult` into a `MatchEvent` (`fill`, `rested` or `cancelled`) and hands it
to a pluggable `MatchResultSink` (`src/match_result/mod.rs`). The default sink drops the events, so
the engine behaves as before. To inspect them, enable the file sink:

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --match-event-log /tmp/node1-events.jsonl
```

Each line is one event, for example
`{"node_id":1,"event_id":{"log_index":42,"ordinal":0},"kind":"fill","order_sequence":7,"counterparty_sequence":3,"price":100.0,"volume":10.0}`.

The file sink is a debugging aid: it is node-local, is not replicated or fsynced, and replaying the
log after a restart writes the replayed events again. The durable clearing feed (a replicated event
log, `/events` cursor reads and idempotent submission) is designed in `docs/MATCH_RESULTS.md`.

### Snapshot configuration

`default_raft_config()` (`src/lib.rs`) sets only the heartbeat and election timings; snapshot
policy comes from openraft's defaults (`LogsSinceLast(5000)`, keeping 1000 covered logs). Tests
that need a snapshot quickly override it:

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // purge everything the snapshot covers
```

- A snapshot is built after the configured number of applied log entries (5000 by default).
- Logs covered by a snapshot plus `max_in_snapshot_log_to_keep` are purged. The snapshot test sets
  both values so a joining learner cannot catch up by log replication and must install a snapshot.

### Data recovery

On startup the node:
1. Recovers the WAL index, vote and purge marker (discarding a torn tail).
2. Loads `snapshot/current.snap` if it exists and rebuilds the order book and KV map.
3. Lets openraft replay the retained log tail on top of the restored state.

A single-node cluster therefore comes back with all committed KV writes, live orders and its
sequence counter, whether recovery is pure log replay or snapshot + tail. Both paths are covered by
`tests/cluster/test_restart.rs`.

## Cluster Management

Adding a node to a cluster involves 3 steps:

1. **Write the node info** through the Raft protocol to storage
2. **Add as Learner** to let it start receiving replication data from the leader
3. **Change membership** to promote the learner to a full voting member

> **Note**: Raft itself does not store node addresses. This implementation stores node information in the storage layer, and the network layer references the store to lookup target node addresses.

## API Reference

All endpoints speak JSON. Every handler returns a `Result` wrapper, so a successful call looks like
`{"Ok": ...}` and a forwarded or failed call like `{"Err": ...}`.

### Raft Internal APIs

Mounted on the public HTTP listener and used by the HTTP/JSON transport:

- `POST /append` - Append entries RPC
- `POST /vote` - Request vote RPC
- `POST /snapshot` - Install a full snapshot RPC

The gRPC transport exposes the same operations on `--http-addr` port + 1000
(`AppendEntries`, `Vote`, `InstallSnapshot`, `AppendStream`); see `proto/raft.proto`.

### Admin APIs

- `POST /init` - Initialize a cluster; body is `[]` for the receiving node alone, or
  `[[1, "127.0.0.1:21001"], ...]` for an explicit membership
- `POST /add-learner` - Add a learner node; body is `[id, "host:port"]`
- `POST /change-membership` - Change the voter set; body is `[1, 2, 3]`
- `GET /metrics` - Get Raft metrics
- `POST /get_linearizer` - Fetch a read linearizer from the leader (used by `/follower_read`)

### Application APIs

- `POST /write` - Submit a `Set`, `Place` or `Cancel` request through Raft
- `POST /read` - Read from the local state machine (may be stale)
- `POST /linearizable_read` - Linearizable read; the request must reach the leader
- `POST /follower_read` - Linearizable read served locally after fetching a linearizer from the leader

`/read`, `/linearizable_read` and `/follower_read` accept a JSON string key. Besides ordinary KV
keys, the state machine exposes:

- `orderbook_orders` - all live orders as a JSON array
- `orderbook_sequence` - the order sequence counter (`orderbook_sequance` is accepted as a legacy
  alias)

Example order payload accepted by `/write` (note the field name `sequence`):

```json
{"Place": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 1}}}
```

The matching engine overwrites `sequence` when it applies the order, so the value clients send is
ignored; `0` is the conventional placeholder. The field is still required by the deserializer.

## Testing

`cargo test` builds one integration target (`tests/cluster`) plus unit tests in the library.

| Test | What it proves |
|---|---|
| `tests/cluster/test_cluster.rs` | 3-node cluster: init, learner, membership, write/read, linearizable read, leader forwarding, membership shrink; runs over gRPC by default and HTTP with `MATCHENGINE_TEST_TRANSPORT=http` |
| `tests/cluster/test_snapshot.rs` | Snapshot creation, log purge, and a learner installing the purged state over the gRPC snapshot stream |
| `tests/cluster/test_storage.rs` | WAL framing, torn-tail truncation, segment rotation, truncate/purge across segments, vote/committed persistence, snapshot build/install/reopen, corrupt-file rejection |
| `tests/cluster/test_restart.rs` | Full node restart recovering KV, order book and sequence via WAL replay, and via snapshot + tail |
| `tests/cluster/test_match_results.rs` | A node started with `--match-event-log` records rest/fill/cancel events in log order while the `/write` response stays unchanged |

Every integration test uses a unique temporary data directory. The cluster test uses fixed ports
`21001-21003` (gRPC `22001-22003`), the snapshot test `21101-21102`, the restart test `21201-21202`
and the match-result test `21301`; stop leftover processes (`pkill -f matchengine-raft`) before
rerunning.

## Future Work

- [x] Optimize serialization — Raft RPCs use `postcard` inside protobuf envelopes (the public API stays JSON)
- [x] Improve network layer with gRPC — tonic transport with streaming appends and chunked snapshots
- [x] Add comprehensive testing — storage, snapshot, cluster and restart coverage
- [ ] Add the HTTP/JSON vs gRPC benchmark harness described in `docs/TRANSPORTS.md`
- [ ] Configure TLS/mTLS for the gRPC listener
- [x] Route match results through a pluggable `MatchResultSink` — `fill`/`rested`/`cancelled` events with `(log_index, ordinal)` ids, and a `--match-event-log` JSONL file sink for debugging
- [ ] Deliver match results durably to order/clearing systems — events in the `/write` response, a replicated event log with `/events` cursor reads, and idempotent submission; design in `docs/MATCH_RESULTS.md` (phases 1b–3)
- [ ] Implement more matching algorithms (market orders, integer tick prices)
- [ ] Enhance client library and add end-to-end latency benchmarks

## Credits

This project is a fork from [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the [OpenRaft](https://github.com/datafuselabs/openraft) project.

Special thanks to the Databend community and Zhang Yanpo for their support.

## License

Please refer to the original OpenRaft project for licensing information.
