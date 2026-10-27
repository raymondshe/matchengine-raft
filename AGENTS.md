# AGENTS.md

Single-crate Rust project (edition 2024): a Raft-replicated KV store + order-book matching engine built on OpenRaft.

## Commands

- Build and run one node: `cargo run --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:21001 --transport grpc`
  - `--transport` is `grpc` (default) or `http`; it selects **outbound** Raft RPC only. The node always serves both the HTTP API on `--http-addr` and gRPC on the same host at `port + 1000`.
  - `--data-dir` (env `RAFT_DATA_DIR`, default `/tmp/matchengine-raft`) and `--instance-prefix` (env `RAFT_INSTANCE_PREFIX`, default `matchengine`) place durable state in `<data-dir>/<prefix>-<id>/{log,snapshot}`.
- `cargo test` runs everything (a single integration target, `cluster`, ~10s once built). Focus it with `cargo test --test cluster`.
  - `MATCHENGINE_TEST_TRANSPORT=http cargo test --test cluster` runs the cluster suite over the legacy HTTP/JSON transport.
  - `tests/cluster/test_snapshot.rs` forces a snapshot, purges the log, and checks that a joining learner installs the snapshot over gRPC.
  - `tests/cluster/test_storage.rs` covers the WAL and snapshot formats directly; `tests/cluster/test_restart.rs` boots a node, stops it, and proves recovery via log replay and via snapshot.
  - Every integration test allocates a unique temp data dir (`tests/cluster/common.rs`); never point tests at the default `/tmp/matchengine-raft` or a second run will inherit the first run's membership.
- First build is a normal crates.io download + compile; `openraft` 0.10.0-alpha.37 is a regular version dependency (it used to be a git pin, which cost ~7 min of checkout/build). The build script vendors `protoc` via `protoc-bin-vendored`, so no system protobuf compiler is needed.
- Cluster walkthrough over HTTP: `./test-cluster.sh` (builds, starts 3 nodes, initialises with `-d '[]'`, writes/reads). It uses `/tmp/matchengine-raft-test-cluster` and clears it on start unless `RAFT_DATA_DIR` is set. `./open-cluster.sh` starts the same cluster in `/tmp/matchengine-raft-open-cluster` and leaves it running.
- `./test.sh <cmd> [node]` is the manual harness: `start-node`, `build-cluster`, `change-membership`, `metrics`, `place-order`, `get-seq`, `kill-node`, `kill`, `clean`. Node arguments default to 1 where it makes sense.
- No CI, no rustfmt/clippy config, no Makefile — nothing gates commits. `cargo fmt`/`cargo clippy` are optional.

## Where things are wired

- `build_match_engine_node()` in `src/lib.rs` is the only composition point: it opens `LogStore`/`StateMachineStore` from a `StorageConfig`, creates the Raft config, `TransportNetworkFactory`, binds the gRPC listener, starts the tonic server and registers every actix route. It returns a `MatchEngineNode` whose `run()` waits for shutdown and whose `shutdown()` stops HTTP, gRPC and Raft cleanly (needed before reopening the same storage).
- `start_match_engine_node_with_transport()`, `start_match_engine_node_with_config()` and `start_match_engine_node_with_storage()` are thin build+run wrappers for the binary, tests and benchmarks.
- Actual HTTP routes: `/init`, `/add-learner`, `/change-membership`, `GET /metrics`, `/get_linearizer`, `/write`, `/read`, `/linearizable_read`, `/follower_read`, and Raft RPC `/vote`, `/append`, `/snapshot`. There is no `/raft-append`, `/raft-vote`, `/raft-snapshot` or `/consistent_read`.
- gRPC Raft service (`proto/raft.proto`, `src/network/grpc/`): `AppendEntries`, `Vote`, `InstallSnapshot` (client-streaming chunks), `AppendStream` (bidirectional, used by `stream_append`). Envelopes are protobuf; payloads are `postcard`-encoded openraft types.
- Raft port convention: `BasicNode.addr` stays the HTTP API address, and the Raft transport lives at `raft_addr_from_http(addr)` = same host, port + `RAFT_PORT_OFFSET` (1000). `src/network/mod.rs` owns the derivation and the `Transport` enum.
- `/init` takes a JSON array, so a single-node cluster must be initialised with `-d '[]'`. A bare POST or `-d '{}'` fails deserialization ("invalid type: map, expected a sequence"); all scripts now send `'[]'`.
- `POST /snapshot` is the HTTP transport's v2 full-snapshot RPC (one request carrying the whole snapshot). The gRPC transport streams the same snapshot as 256 KiB chunks.
- `src/bin/sled_test.rs` is an auto-discovered second binary (playground; creates a local `my_db` sled dir, gitignored).

## Branch state — read before changing storage or network code

- Upgraded to `openraft` 0.10.0-alpha.37 from crates.io (git pin and the `openraft-legacy` dependency are gone). Installing it is a normal `cargo fetch`; no git checkout needed.
- The node now runs **gRPC (tonic) by default** for peer-to-peer Raft RPCs; the HTTP/JSON implementation is retained and selectable with `--transport http`.
  - `src/network/grpc/client.rs`: `RaftNetworkV2` implementation with unary append/vote, a custom `stream_append` over `AppendStream`, and chunked `full_snapshot` that honours the `cancel` future.
  - `src/network/grpc/server.rs`: tonic service calling the same `Raft` APIs as the HTTP handlers. Its `AppendStream` handler feeds `Raft::stream_append`, so the follower pipelines into RaftCore.
  - `src/network/transport.rs`: `TransportNetworkFactory` / `TransportNetwork` enum; runtime transport selection without changing `Raft`'s type.
  - `src/network/raft_network_impl.rs`: legacy HTTP/JSON `RaftNetworkV2` implementation; `stream_append` stays sequential there.
- Transport failure mapping: tonic `Unavailable` -> `RPCError::Unreachable` (backoff), `DeadlineExceeded` / elapsed `soft_ttl` -> `RPCError::Timeout`, other statuses -> `RPCError::Network`. Raft-level rejections travel as encoded `RaftError` payloads, matching the HTTP path.
- Message limits: both directions of the gRPC service are capped at 64 MiB (`MAX_MESSAGE_SIZE`); snapshot chunks are 256 KiB.
- Deadlines: unary vote/append and each `stream_append` request in flight get `RPCOption::soft_ttl()`. `stream_append` only times a wait while a request is in flight (the heartbeat worker polls for a response before the feeder necessarily sends), and `full_snapshot` has no per-RPC deadline — it is bounded by the `cancel` future and HTTP/2 keep-alive, because a legitimate snapshot can outlive one heartbeat interval.
- Each peer gets separate gRPC connections for replication, heartbeat and snapshot (openraft's `new_heartbeat_client` / `new_snapshot_client` default to `new_client`, and each `GrpcNetwork` owns its own lazy channel), so a snapshot cannot head-of-line-block a vote.
- Storage is **durable**. `src/store/log_store.rs` is a hand-written segmented WAL: `log/NNNNNNNNNN.wal` with an 8-byte `MRFT`+version header, `[len u32 LE][crc32 u32 LE][postcard payload]` records, one `write_all` + one `sync_data` per openraft append batch, `pread` reads, and blocking IO on `spawn_blocking`. Recovery walks segments and truncates a torn tail. Vote + `last_purged` live in an atomically rewritten `log/state`; `committed` is a WAL record that becomes durable with the next append (no second fsync).
- `src/store/state_machine.rs` keeps KV/order book/sequence in memory and persists `snapshot/current.snap` (magic `MRFSNAP` + version + postcard meta + postcard content) via temp file → fsync → rename → dir fsync. `install_snapshot` validates before replacing the durable copy. Restart = open snapshot, then openraft replays the retained log tail.
- `src/store/config.rs` owns `StorageConfig` (`data_dir`, `instance_prefix`, `node_dir`/`log_dir`/`snapshot_dir`). The old sled-backed `src/store/store.rs` and the `walkdir` dependency are gone; `sled` remains only for the `sled_test.rs` playground binary.

## Test gotchas

- `tests/cluster/test_cluster.rs` boots 3 nodes in threads on fixed ports 127.0.0.1:21001-21003 (gRPC 22001-22003) and never shuts them down. `test_snapshot.rs` uses 21101-21102 (gRPC 22101-22102); `test_restart.rs` uses 21201 and 21202 (gRPC 22201-22202) and does shut its nodes down. Kill leftovers (`./test.sh kill`, i.e. `pkill -f matchengine-raft`) before rerunning or the bind fails.
- `test_snapshot.rs` polls `metrics().current_leader` after `init`; without that wait the first write can race the election and fail with `NotLeader`.
- The cluster test asserts an exact `LinearizableReadError` message string naming node 1; changing error formatting breaks it.
- The snapshot test depends on `snapshot_policy = LogsSinceLast(10)` and `max_in_snapshot_log_to_keep = 0`: the learner must join after the covered log has been purged, otherwise it catches up by log replication and the test proves nothing.

## Matching-engine quirks

- `OrderBook::place_order` stops matching as soon as the taker's remaining volume reaches zero, and
  a partially filled maker is updated in place so its `id`, `side`, `live` and `sequence` survive.
  `Order::new` still hardcodes `id: 0`, so do not use it to rebuild a resting order. Regression
  tests: `exact_fill_*`, `partial_fill_preserves_maker_identity` in `src/matchengine/mod.rs`.
- `Order` deserializes `sequence`; the scripts now send `sequence` too. `Order::sequence` is still
  required by the deserializer even though `place_order()` overwrites it.
- `/read` special keys: `orderbook_orders`, and `orderbook_sequence` (misspelled variant `orderbook_sequance` also accepted).

## Docs vs code

- Trust `src/` over prose. `README.md`, `README_CN.md`, `GUIDE_cn.md` and `TODO.md` now match the
  WAL + snapshot + gRPC design; README requires Rust ≥ 1.85 (edition 2024).
- `docs/AERON_SOLUTION.md` and `docs/TRANSPORTS.md` are investigation documents: gRPC via `tonic`
  is implemented (`src/network/grpc/`) and is the default, HTTP remains selectable, and no transport
  benchmark exists. Read `docs/TRANSPORTS.md`'s status banner before treating its numbers as results.
