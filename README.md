# Match Engine Raft

A practical implementation of a distributed key-value store and matching engine built upon [OpenRaft](https://github.com/datafuselabs/openraft). This project demonstrates how to implement persistent storage for Raft logs and snapshots on disk using [Sled](https://github.com/spacejam/sled) as the underlying storage engine.

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

## Features

- **Persistent Raft Storage**: Raft logs and state are persisted using Sled, a high-performance embedded key-value store
- **Snapshot Management**: State machine snapshots are stored as files on disk for efficient recovery
- **Matching Engine**: Includes a simple order book matching engine as an example application
- **HTTP Server**: Built with [Actix-web](https://docs.rs/actix-web) providing both internal Raft APIs and external application APIs
- **Smart Client**: A Rust client that automatically tracks and redirects requests to the current leader

## Architecture Overview

This project extends the [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) with persistent storage capabilities:

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
    │   RaftStorage     │     │   RaftNetwork     │     │   HTTP Server      │
    │  (Sled + Files)   │     │  (Reqwest Client) │     │  (Actix-web)       │
    └───────────────────┘     └───────────────────┘     └───────────────────┘
```

### Key Components

1. **RaftStorage Implementation** ([`store/`](./src/store/))
   - Raft logs and votes stored in Sled
   - State machine in memory with file-based snapshots
   - Automatic snapshot creation every 500 log entries

2. **Network Layer** ([`network/`](./src/network/))
   - Internal Raft RPC for replication and voting
   - Connection pooling for efficient node-to-node communication
   - Management APIs for cluster administration

3. **Matching Engine** ([`matchengine/`](./src/matchengine/))
   - Order book with bids and asks
   - Example of a real-world state machine application

## Getting Started

### Prerequisites

- Rust 1.60 or later
- Cargo

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

This script demonstrates a 3-node cluster using only `curl` commands, showing the HTTP communication between client and cluster.

### Using the Rust Test

```shell
cargo test
```

This runs the same scenario as `test-cluster.sh` but using the Rust `ExampleClient`.

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

#### 2. Initialize the Cluster

```shell
curl -X POST http://127.0.0.1:21001/init
```

This initializes node 1 as the leader.

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

# Build the cluster
./test.sh build-cluster

# Check metrics
./test.sh metrics 1

# Clean up
./test.sh clean
```

## Project Structure

```
src/
├── bin/
│   └── main.rs              # Server entry point
├── client.rs                # Example Raft client
├── lib.rs                   # Library with server setup
├── app.rs                   # Application state
├── matchengine/
│   └── mod.rs               # Order book matching engine
├── network/
│   ├── api.rs               # Application HTTP endpoints
│   ├── management.rs        # Admin HTTP endpoints
│   ├── raft.rs              # Raft internal RPC endpoints
│   ├── raft_network_impl.rs # RaftNetwork implementation
│   └── mod.rs
└── store/
    ├── mod.rs               # Storage module
    ├── log_store.rs         # Raft log storage implementation
    ├── state_machine.rs     # State machine implementation
    ├── store.rs             # Snapshot file I/O operations
    ├── types.rs             # Storage types
    └── config.rs            # Storage configuration
```

## Storage Implementation

### RaftStorage Trait

The `RaftStorage` trait implementation is the core of this project. It handles:

1. **Raft State**: Stored in Sled using dedicated keys
2. **Logs**: Stored in Sled with log index as key
3. **State Machine**: In-memory with file-based snapshots
4. **Votes**: Persisted in Sled to ensure election safety

### Snapshot Configuration

```rust
let mut config = Config::default().validate().unwrap();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(500);
config.max_applied_log_to_keep = 20000;
config.install_snapshot_timeout = 400;
```

- A snapshot is created every 500 log entries
- Up to 20,000 log entries are kept before purging
- Snapshot files are stored on disk with metadata in the filename

### Data Recovery

On startup, the store:
1. Loads the latest snapshot from disk
2. Replays any remaining logs from Sled
3. Restores the state machine to the latest state

## Cluster Management

Adding a node to a cluster involves 3 steps:

1. **Write the node info** through the Raft protocol to storage
2. **Add as Learner** to let it start receiving replication data from the leader
3. **Change membership** to promote the learner to a full voting member

> **Note**: Raft itself does not store node addresses. This implementation stores node information in the storage layer, and the network layer references the store to lookup target node addresses.

## API Reference

### Raft Internal APIs

- `POST /raft-append` - Append entries RPC
- `POST /raft-snapshot` - Install snapshot RPC
- `POST /raft-vote` - Request vote RPC

### Admin APIs

- `POST /init` - Initialize a single-node cluster
- `POST /add-learner` - Add a learner node
- `POST /change-membership` - Change cluster membership
- `GET /metrics` - Get Raft metrics

### Application APIs

- `POST /write` - Write data to the state machine
- `POST /read` - Read data from the state machine (local read)
- `POST /consistent_read` - Consistent read (goes through leader)

## Future Work

- [ ] Optimize serialization (replace JSON with Protobuf/Avro)
- [ ] Improve network layer with gRPC
- [ ] Add matching result distribution via message queues
- [ ] Implement more matching algorithms
- [ ] Add comprehensive testing and benchmarking
- [ ] Enhance client library

## Credits

This project is a fork from [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the [OpenRaft](https://github.com/datafuselabs/openraft) project.

Special thanks to the Databend community and Zhang Yanpo for their support.

## License

Please refer to the original OpenRaft project for licensing information.
