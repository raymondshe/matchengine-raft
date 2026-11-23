# ⚡ match_engine

The Rust engine crate of the [Matchengine Raft](../README.md) project. It owns the replicated order
book, WAL/snapshots, gRPC Raft transport, event log and market-data feed. See the
[root README](../README.md) for the five-project overview, demos, Kubernetes control plane,
security boundary and benchmark headline table.

## 🚀 Build and run

Rust 1.89 or later is required (edition 2024). `protoc` is not required: the build script vendors it.

```shell
cargo build --release
cargo run --release --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:21001
```

The HTTP API listens on `--http-addr`; Raft peer RPCs use gRPC on `port + 1000`. There is no
`--transport` switch. The binary handles SIGTERM/SIGINT and drains through
`MatchEngineNode::run_until`, which matters in containers where it is PID 1.

A single node must be initialised with an empty JSON array:

```shell
curl -s -X POST http://127.0.0.1:21001/init -H 'content-type: application/json' -d '[]'
```

Start a local four-node cluster (and keep it for inspection) with:

```shell
../scripts/open-cluster.sh
../scripts/test-cluster.sh status
../scripts/test-cluster.sh stop
```

The runbook covers manual startup, membership changes, failure drills and recovery
([`docs/RUNBOOK.md`](../docs/RUNBOOK.md)). The OrbStack Linux lab is documented in
[`docs/ORBSTACK_LAB.md`](../docs/ORBSTACK_LAB.md).

## ⚙️ Engine configuration

| Setting | Purpose |
|---|---|
| `--id`, `--http-addr` | Raft node ID and HTTP listener; gRPC is HTTP port + 1000 |
| `--data-dir`, `--instance-prefix` | Durable root and node directory prefix (default `/tmp/matchengine-raft`, `matchengine`) |
| `--id-offset` | Adds to `--id` for both the Raft node ID and data-directory suffix; migration shadows use a non-overlapping range |
| `--read-policy local_lease\|lease\|read_index` | Leader-consistent read path; `local_lease` is the default, `read_index` forces a fresh quorum round |
| `--max-inflight-writes N` | Per-node `/write` and `/write_batch` admission cap (default 1024, `0` disables) |
| `--max-wal-append-bytes N` | Splits one apply batch into byte-bounded WAL writes (default 8 MiB, `0` disables) |
| `--snapshot-interval-ms N` | Snapshot timer; `0` disables only the timer, not the log-count policy |
| `--match-event-log FILE` | Node-local JSONL debugging sink, not replicated |
| `--market-data-log FILE` | Deterministic market-data JSONL sink |
| `--async-wal` | Prototype async WAL; requires `--features async-wal` and `RAFT_WAL_FSYNC_MS=0` |

Important environment variables:

- `RAFT_WAL_FSYNC_MS` — unset = no fsync (default; requires at least four voters), `0` = every
  append, `N > 0` = periodic every N ms.
- `RAFT_CORE_WORKERS` — positive value runs OpenRaft on a dedicated `raft-worker` runtime.
- `MATCHENGINE_READ_POLICY`, `MATCHENGINE_MAX_INFLIGHT_WRITES` — defaults for the flags above.
- `MATCHENGINE_ASYNC_WAL` — enables the prototype async WAL.

Cargo features: `async-wal`, `market-data-aeron` (Aeron egress; add `aeron-precompiled` on macOS
or Linux x86_64 to use the published binaries, or omit it for a native arm64 source build) and
`profiling` (OpenRaft runtime histograms).

## 🧱 Engine internals

1. **Raft storage** ([`src/store/`](./src/store/))
   - Segmented, checksummed WAL in `log/*.wal` with torn-tail recovery.
   - Vote and purge state in an atomically rewritten `log/state`.
   - In-memory state machine made durable by atomic `snapshot/current.snap` files.

2. **Network layer** ([`src/network/`](./src/network/))
   - gRPC Raft RPCs for append, vote, pre-vote, snapshot install and stream append.
   - Separate replication, heartbeat and snapshot channels per peer.
   - Application/management HTTP APIs, embedded console, health and readiness probes.

3. **Matching engine** ([`src/order_book/`](./src/order_book/))
   - Price-time priority book with GTC/IOC/FOK/GTD/Day, market, market-to-limit, post-only,
     iceberg, stop-market, stop-limit, trailing-stop and pegged orders, plus STP modes.
   - Deterministic `apply`; no wall clock in the matching path.
   - `OrderBook::execute_order` is the full lifecycle; `place_order` returns only fills.

4. **Market-data egress** ([`src/market_data/`](./src/market_data/))
   - `apply` hands match events and a depth journal to a bounded `try_send` tap.
   - A leader-gated tokio task keeps a shadow book, derives BBO/OHLC bars and encodes wire v7.
   - JSONL debugging sink and feature-gated Aeron publisher (streams 2001–2007).

## 🧪 Tests and documentation

```shell
cargo test --locked
cargo test --features async-wal --test cluster async_wal --locked
cargo test --features market-data-aeron,aeron-precompiled --lib market_data::aeron --locked
```

- [API reference](../docs/API.md)
- [Storage, WAL and recovery](../docs/STORAGE.md)
- [Matching and amendments](../docs/MATCHING.md)
- [Match-result delivery](../docs/MATCH_RESULTS.md)
- [Market-data plan](../docs/MARKET_DATA_PLAN.md)
- [Testing](../docs/TESTING.md)
- [Performance methodology and baselines](../docs/PERFORMANCE.md)
- [Project structure](../docs/PROJECT_STRUCTURE.md)
- [Roadmap](../TODO.md)

The engine crate is self-contained; the standalone demos and control plane live one level up and
have their own manifests and READMEs.

## 📄 License

MIT — see [`LICENSE`](./LICENSE). The project started as a fork of
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) in the
[OpenRaft](https://github.com/datafuselabs/openraft) project; the original notices continue to
apply to derived code.
