# Runbook

## Start a cluster

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

`cargo test` runs the cluster integration suite; see [Testing](./TESTING.md) for the test matrix.

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
[Per-node data directory](./STORAGE.md#per-node-data-directory).

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

## Membership and leadership

Adding a node to a cluster involves 3 steps:

1. **Write the node info** through the Raft protocol to storage
2. **Add as Learner** to let it start receiving replication data from the leader
3. **Change membership** to promote the learner to a full voting member

> **Note**: Raft itself does not store node addresses. This implementation stores node information in the storage layer, and the network layer references the store to lookup target node addresses.
