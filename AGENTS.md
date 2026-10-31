# AGENTS.md

Single-crate Rust project (edition 2024): a Raft-replicated order-book matching engine built on OpenRaft.

## Commands

- Build and run one node: `cargo run --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:21001`
  - The HTTP API stays on `--http-addr`; Raft peer RPCs are gRPC on the same host at `port + 1000`. There is no `--transport` switch.
  - `--data-dir` (env `RAFT_DATA_DIR`, default `/tmp/matchengine-raft`) and `--instance-prefix` (env `RAFT_INSTANCE_PREFIX`, default `matchengine`) place durable state in `<data-dir>/<prefix>-<id>/{log,snapshot}`.
  - `--match-event-log <file>` (env `MATCH_EVENT_LOG`) appends every applied fill/rest/cancel to a JSONL file. Debug aid only: node-local, not replicated, duplicates lines when the log is replayed after a restart. The durable feed is the replicated event log served by `/events`; `--max-events` (env `MAX_EVENTS`, default 262144) bounds it.
  - `--market-data-log <file>` (env `MARKET_DATA_LOG`) enables the deterministic market-data feed (`src/market_data/`) and appends its messages as JSONL. Leader-only by default; `--market-data-depth`, `--market-data-status-every`, `--market-data-bar-intervals-ms` and `--market-data-leader-only` tune it.
CI (`.github/workflows/ci.yml`) runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` on push/PR. `Cargo.lock` is tracked (do not add it back to `.gitignore`).
- `cargo test` runs everything (a single integration target, `cluster`, ~10s once built). Focus it with `cargo test --test cluster`.
  - `cargo bench --bench matchengine` runs the order-book micro-benchmarks (no framework, prints best-of-5 per-round times).
  - `cargo bench --bench e2e` starts a single node (127.0.0.1:24001 on the default multi-thread runtime) and measures HTTP submit → Raft commit → apply → response, including a `/write_batch` sweep at 1/8/32/64/128 orders per entry plus concurrent batches; pass `-- current` or `-- both` to compare the old current-thread runtime. `cargo bench --bench feed_load` drives the market-data queue at 50k/500k fills/s and an overload case. Results live in `docs/PERFORMANCE.md`.
  - `cargo bench --bench storage` measures WAL append/read/reopen with real order payloads; results live in `docs/PERFORMANCE.md`.
  - `cargo bench --bench p0_bench -- duplicate|read|snapshot` measures the P0 state-machine optimizations: duplicate retry scan, large reads under write load, and snapshot under write load.
  - `tests/cluster/test_snapshot.rs` forces a snapshot, purges the log, and checks that a joining learner installs the snapshot over gRPC; a second case disables the log-count policy and proves the 30-minute timer path writes `snapshot/current.snap`.
  - `tests/cluster/test_storage.rs` covers the WAL and snapshot formats directly, including a 20k-record multi-segment reopen; `tests/cluster/test_restart.rs` boots a node, stops it, and proves recovery via log replay and via snapshot + newer WAL tail (including events and dedupe), plus 2k-order large-log replay and a multi-chunk (~120 KB) snapshot boot. `tests/cluster/test_membership.rs` kills the elected leader and proves a remaining voter keeps the committed book and accepts writes, and adds a learner after writes to prove it catches up from the live log before promotion. `tests/cluster/test_late_join.rs` gives a node a stale snapshot/WAL, removes it, advances the cluster while the leader purges the log, then re-adds it as a learner and verifies it catches up from live members.
  - `tests/cluster/test_match_results.rs` boots a node with the file sink and checks rest/fill/cancel events in log order (port 21301, gRPC 22301); a second test on 21302 exercises a stop trigger, GTD expiry and the new `/read` keys.
  - `tests/cluster/test_exactness.rs` proves 18-fractional-digit prices/quantities survive `/write`, `/read`, `/events`, the JSONL market-data feed, snapshot v9 and a restart (port 21701, gRPC 22701).
  - `tests/cluster/test_symbols.rs` configures the process symbol/precision, rejects over-precision and cross-symbol orders, locks the configuration once trading starts, and proves it survives snapshot restart (port 21801, gRPC 22801).
  - `tests/cluster/test_fees.rs` proves maker/taker fee rates produce exact fill fees in `/write`, `/events`, the file sink and the market-data feed, and survive snapshot restart (port 21901, gRPC 22901).
  - `tests/cluster/test_fee_assets.rs` configures an ETF/fiat QQQ/USD symbol, enforces tick/lot increments, quotes a limit order in quote volume and proves quote-asset fee deduction in `/write` and `/events` (port 21902, gRPC 22902).
  - `tests/cluster/test_batch_write.rs` applies two requests in one Raft entry and checks per-item sequences, continuous event ordinals and empty/nested/oversized/duplicate-id rejection (port 21904, gRPC 22904).
  - `tests/cluster/test_request_limits.rs` sends oversized bodies, malformed JSON, invalid/oversized `client_order_id`s, oversized `/read` keys and out-of-range `/events` pages and proves they fail with `400`/`413` before Raft changes the book (port 21903, gRPC 22903).
  - Every integration test allocates a unique temp data dir (`tests/cluster/common.rs`); never point tests at the default `/tmp/matchengine-raft` or a second run will inherit the first run's membership.
- First build is a normal crates.io download + compile; `openraft` 0.10.0-alpha.37 is a regular version dependency (it used to be a git pin, which cost ~7 min of checkout/build). The build script vendors `protoc` via `protoc-bin-vendored`, so no system protobuf compiler is needed.
- Cluster walkthrough over HTTP: `./scripts/test-cluster.sh` (builds, starts 3 nodes, initialises with `-d '[]'`, writes/reads). It uses `/tmp/matchengine-raft-test-cluster` and clears it on start unless `RAFT_DATA_DIR` is set. `./scripts/open-cluster.sh` starts the same cluster in `/tmp/matchengine-raft-open-cluster` and leaves it running.
- `./scripts/test.sh <cmd> [node]` is the manual harness: `start-node`, `build-cluster`, `change-membership`, `metrics`, `place-order`, `get-seq`, `kill-node`, `kill`, `clean`. Node arguments default to 1 where it makes sense.
- No rustfmt/clippy config and no Makefile; CI gates push/PR with `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.

## Where things are wired

- `build_match_engine_node()` in `src/lib.rs` is the only composition point: it opens `LogStore`/`StateMachineStore` from a `StorageConfig`, creates the Raft config, `GrpcNetworkFactory`, binds the gRPC listener, starts the tonic server and registers every actix route. It returns a `MatchEngineNode` whose `run()` waits for shutdown and whose `shutdown()` stops HTTP, gRPC and Raft cleanly (needed before reopening the same storage).
- The binary runs on a multi-threaded Tokio runtime (`#[tokio::main(flavor = "multi_thread")]` in `src/main.rs`); Actix HTTP workers are separate threads. `spawn_blocking` is reserved for WAL/snapshot file I/O and other blocking calls. See the thread/task audit in `docs/PERFORMANCE.md`.
- Match-result processing is pluggable: `src/match_result/mod.rs` defines `MatchEvent`/`EventId` and the `MatchResultSink` trait (`NoopSink`, `MemorySink`, `FileLogSink`). `build_match_engine_node` installs `NoopSink`, or a `FileLogSink` when `StorageConfig::match_event_log` is set, via `StateMachineStore::open_with_sinks_and_retention`. `apply` derives events from the entry alone, appends them to the replicated event window, returns the first 512 in `/write`, and collects sink/tap work in `PendingPublish`; the state-machine lock is acquired per entry and the sink/tap publishing runs after it is released. Sink errors are logged and never fail the apply. `POST /events` serves the window with an `(log_index, ordinal)` cursor; `GET /events/status` reports retention. `Place { client_order_id }` dedupes retries through a fixed 262144-record map (`MAX_SUBMISSIONS`), kept independent of `--max-events` so retry resolution cannot diverge across nodes.
- `MatchEngineRequest::Batch { requests }` applies 1–128 non-nested requests in one entry (one WAL append/fsync): sub-requests run in order, each keeps its own sequence/response under `MatchEngineResponse.batch`, and event ordinals continue across the whole entry. `/write_batch` takes a bare JSON array; empty, nested, oversized and duplicate-`client_order_id` batches are rejected at the edge.
- `start_match_engine_node_with_config()` and `start_match_engine_node_with_storage()` are thin build+run wrappers for the binary, tests and benchmarks.
- Actual HTTP routes: `/init`, `/add-learner`, `/change-membership`, `GET /metrics`, `/get_linearizer`, `/write`, `/write_batch`, `/read`, `/linearizable_read`, `/follower_read`, `POST /events`, `GET /events/status`. Raft RPCs are gRPC-only; there are no HTTP `/append`, `/vote` or `/snapshot` routes.
- gRPC Raft service (`proto/raft.proto`, `src/network/grpc/`): `AppendEntries`, `Vote`, `InstallSnapshot` (client-streaming chunks), `AppendStream` (bidirectional, used by `stream_append`). Envelopes are protobuf; payloads are `postcard`-encoded openraft types.
- Raft port convention: `BasicNode.addr` stays the HTTP API address, and the gRPC Raft transport lives at `raft_addr_from_http(addr)` = same host, port + `RAFT_PORT_OFFSET` (1000). `src/network/mod.rs` owns the derivation.
- `/init` takes a JSON array, so a single-node cluster must be initialised with `-d '[]'`. A bare POST or `-d '{}'` fails deserialization ("invalid type: map, expected a sequence"); all scripts now send `'[]'`.
- `examples/sled.rs` is a playground example (creates a local `my_db` sled dir, gitignored); `sled` is a dev-dependency, so it is not linked into the node binary.
- `cargo run --example dump_wal -- <node-dir|log-dir|segment.wal>` and `cargo run --example dump_snapshot -- <node-dir|snapshot-dir|current.snap>` print read-only, CRC-validated JSONL dumps of durable state. `read_wal_file` and `read_snapshot_file` are the public helpers behind them.
- `cargo run --release --example replay_orders -- --orders orders.jsonl [--expected expected.jsonl]` starts an in-process node, replays orders in `/write_batch` batches, and compares the generated match-event JSONL (ignoring `node_id`/`log_index` unless `--strict-log-index`).

## Branch state — read before changing storage or network code

- **Breaking wire/disk change (2026-10-29):** `Order.price`/`volume`, stop/iceberg fields,
  `OrderBook.last_trade_price`, `MatchEvent.price/volume`, depth/BBO records and the market-data
  wire format now use 18-scale `Fixed` integers. Orders carry `volume_unit`, `SymbolConfig` carries
  base/quote `AssetId`/`AssetClass`, tick/lot and `FeeAssetPolicy`, and fills carry
  `taker_fee`/`maker_fee` plus the fee asset from the order-level user tier. ETF/equity bases
  force `FeeAssetPolicy::Quote`; crypto/spot and fiat-to-fiat pairs default to `Received`.
  Snapshot **v9** and
  wire **v5** are incompatible with older builds; v9 rejects v8. Wipe the node's `--data-dir`
  when moving an existing example deployment across this change; there is no log-entry migration.
- Upgraded to `openraft` 0.10.0-alpha.37 from crates.io (git pin and the `openraft-legacy` dependency are gone). Installing it is a normal `cargo fetch`; no git checkout needed.
- The node runs **gRPC (tonic) only** for peer-to-peer Raft RPCs; the HTTP/JSON transport was removed.
  - `src/network/grpc/client.rs`: `RaftNetworkV2` implementation with unary append/vote, a custom `stream_append` over `AppendStream`, and chunked `full_snapshot` that honours the `cancel` future.
  - `src/network/grpc/server.rs`: tonic service calling the `Raft` APIs. Its `AppendStream` handler feeds `Raft::stream_append`, so the follower pipelines into RaftCore.
  - `src/network/grpc/client.rs`: also owns `GrpcNetworkFactory`, the only `RaftNetworkFactory` used by `Raft::new`.
- Transport failure mapping: tonic `Unavailable` -> `RPCError::Unreachable` (backoff), `DeadlineExceeded` / elapsed `soft_ttl` -> `RPCError::Timeout`, other statuses -> `RPCError::Network`. Raft-level rejections travel as encoded `RaftError` payloads.
- Message limits: both directions of the gRPC service are capped at 64 MiB (`MAX_MESSAGE_SIZE`); snapshot chunks are 256 KiB.
- Public edge: the HTTP API and gRPC listener are plaintext and unauthenticated **by design**; this is an internal matching engine behind a gateway that owns TLS and client authentication. The supported deployment contract is loopback or a trusted private network. Application JSON bodies are capped at 1 MiB, `client_order_id` at 128 printable bytes, `/read` keys at 128 bytes, `/events` pages at 1–1000, gRPC messages at 64 MiB, and snapshot streams at 256 MiB (`src/network/api.rs`, `src/network/mod.rs`, `src/network/grpc/server.rs`).
- Deadlines: unary vote/append and each `stream_append` request in flight get `RPCOption::soft_ttl()`. `stream_append` only times a wait while a request is in flight (the heartbeat worker polls for a response before the feeder necessarily sends), and `full_snapshot` has no per-RPC deadline — it is bounded by the `cancel` future and HTTP/2 keep-alive, because a legitimate snapshot can outlive one heartbeat interval.
- Each peer gets separate gRPC connections for replication, heartbeat and snapshot (openraft's `new_heartbeat_client` / `new_snapshot_client` default to `new_client`, and each `GrpcNetwork` owns its own lazy channel), so a snapshot cannot head-of-line-block a vote.
- Storage is **durable**. `src/store/log_store.rs` is a hand-written segmented WAL: `log/NNNNNNNNNN.wal` with an 8-byte `MRFT`+version header, `[len u32 LE][crc32 u32 LE][postcard payload]` records, one `write_all` + one `sync_data` per openraft append batch, `pread` reads, and blocking IO on `spawn_blocking`. Recovery walks segments and truncates a torn tail. Vote + `last_purged` live in an atomically rewritten `log/state`; `committed` is a WAL record that becomes durable with the next append (no second fsync).
- Default snapshots are `LogsSinceLast(100_000)` plus a 30-minute timer; `StorageConfig::snapshot_interval` / `--snapshot-interval-ms` tunes it and `0` disables only the timer.
- Storage hot paths: WAL append uses a borrowed encoder, recovery scans segments sequentially, contiguous reads share one `pread`, snapshot encode/write happens outside the state-machine lock, and snapshot bodies stream through an incremental CRC writer. `docs/PERFORMANCE.md` records the baseline.
- A failed or dropped gRPC server task is fail-fast: `MatchEngineNode::run()` stops HTTP and background tasks, shuts Raft down, and returns the error. HTTP requests use one `Logger` middleware.
- `src/store/state_machine.rs` keeps the order book + sequence + event window in memory and persists `snapshot/current.snap` (magic `MRFSNAP` + version + postcard meta + postcard content) via temp file → fsync → rename → dir fsync. Snapshot version is 9 (v3 added the extended `Order`, parked stops and last trade price; v4 added the retained `MatchEvent` window and client-order-id dedupe map; v5 stored prices/quantities as 18-scale `Fixed` integers; v6 stored the last trade price and match events as `Fixed`; v7 added the replicated single-symbol precision configuration; v8 added maker/taker fee rates to orders and fee fields to match events; v9 added base/quote asset metadata, tick/lot rules, volume units and fee assets). Older files are rejected on open. `install_snapshot` validates before replacing the durable copy. Restart = open snapshot, then openraft replays the retained log tail.
- `src/store/config.rs` owns `StorageConfig` (`data_dir`, `instance_prefix`, `node_dir`/`log_dir`/`snapshot_dir`). The old sled-backed `src/store/store.rs` and the `walkdir` dependency are gone; `sled` remains only for the `examples/sled.rs` playground.

## Test gotchas

- `src/order_book/property_tests.rs` runs randomized place/cancel/amend traces against a reference model, plus mixed GTC/IOC/FOK/GTD/Day/market/post-only/iceberg/stop orders with positive, zero and rebate fee rates under every fee-asset policy. Failures include the seed and step; reproduce one seed by narrowing the loop while debugging.
- `src/order_book/conformance_tests.rs` holds the explicit edge cases the randomized suite should not have to rediscover: market FOK, empty-book market, hidden iceberg cancel/expiry, parked-stop GTD/Day expiry and fee fields for market, IOC and stop-child fills.
- `tests/cluster/test_cluster.rs` boots 3 nodes in the test runtime on fixed ports 127.0.0.1:21001-21003 (gRPC 22001-22003), owns them, and shuts them down on success or panic. `test_snapshot.rs` uses 21101-21103 (gRPC 22101-22103); `test_restart.rs` uses 21201-21205 (gRPC 22201-22205); `test_match_results.rs` uses 21301 and 21302 (gRPC 22301/22302), `test_market_data.rs` uses 21303 (gRPC 22303), `test_events.rs` uses 21304 (gRPC 22304), `test_membership.rs` uses 21401-21403 (gRPC 22401-22403) for leader failover and 21501-21502 (gRPC 22501-22502) for learner catch-up, `test_late_join.rs` uses 21601-21603 (gRPC 22601-22603) for a stale node rejoining, `test_exactness.rs` uses 21701 (gRPC 22701), `test_symbols.rs` uses 21801 (gRPC 22801), `test_fees.rs` uses 21901 (gRPC 22901), `test_fee_assets.rs` uses 21902 (gRPC 22902), `test_request_limits.rs` uses 21903 (gRPC 22903) and `test_batch_write.rs` uses 21904 (gRPC 22904); these tests shut their nodes down. Kill leftovers (`./scripts/test.sh kill`, i.e. `pkill -f matchengine-raft`) before rerunning or the bind fails.
- `test_snapshot.rs` polls `metrics().current_leader` after `init`; without that wait the first write can race the election and fail with `NotLeader`.
- The cluster test asserts an exact `LinearizableReadError` message string naming node 1; changing error formatting breaks it.
- The snapshot test depends on `snapshot_policy = LogsSinceLast(10)` and `max_in_snapshot_log_to_keep = 0`: the learner must join after the covered log has been purged, otherwise it catches up by log replication and the test proves nothing.

## Matching-engine quirks

- `OrderBook::execute_order` is the full lifecycle `apply` runs (validate, park a stop, sweep, rest
  or cancel the remainder, run the stop cascade) and returns an `Outcome`; `place_order` is the
  compatibility wrapper that returns only the fills. `OrderBook::place_order` stops matching as soon
  as the taker's remaining volume reaches zero, and a partially filled maker is updated in place so
  its `id`, `side`, `live` and `sequence` survive. `Order::new` still hardcodes `id: 0`, so do not
  use it to rebuild a resting order. Regression tests: `exact_fill_*`,
  `partial_fill_preserves_maker_identity` in `src/order_book/mod.rs`.
- Order types live as flat fields on `Order` (`tif`, `post_only`, `stop_price`, `display_volume`,
  `hidden_volume`, `timestamp`), all with serde defaults. `stop_price`/`display_volume` use `Fixed::ZERO`
  (JSON `0`) as "none" so the record stays cheap to copy in the matching loop. `apply` never reads a clock:
  every admissible mutating request purges due GTD orders at its intake timestamp; `Day` still needs
  the explicit `PurgeExpired { now_ms, day_close_ms }`. Stops are elected by
  `OrderBook::last_trade_price`, which is replicated state. Parked stops live in
  `orderbook.stops` (admission order, off the book, invisible to depth) and are cancelled by side +
  sequence. A stop child never rests: whatever it cannot execute is cancelled. A stop the last
  trade already crosses is refused (`stop_would_trigger`) rather than firing on entry.
- Rejections (FOK unfillable, post-only crossing, stop already crossed, invalid shape) do not enter
  the book; `apply` publishes a `rejected` event with a `reason` and the write still returns the
  assigned sequence. Event kinds are `fill`, `rested`, `cancelled`, `triggered`, `expired`,
  `rejected`, `amended`; a stop cascade keeps increasing ordinals inside the same log entry.
- The market-data tap does not run on the matching path: book mutations append `DepthChange`
  entries to a transient `OrderBook::depth_journal` (`enable_depth_journal`, `#[serde(skip)]`) and
  `apply` hands the journal plus the entry's `MatchEvent`s to a bounded tokio `mpsc` sender
  (`try_send`). A separate tokio task owns the shadow book, OHLC bars, encoding and the sink. A full
  queue drops frames and emits `GAP`; matching is never blocked. The feed is leader-gated by
  default through a `raft.metrics()` watcher, which pushes a reset snapshot on every leadership
  gain (`--market-data-leader-only=false` publishes from every replica).
- The receiving HTTP node stamps `Order.timestamp` at intake (overwriting client values) before
  proposing; it travels in the log and labels events and market data. `MatchEvent.side` is the side
  of the order the event is about.
- Market-data keys are `(symbol_id, stream_id, market_seq, sub_seq)`. `market_seq` is the replicated
  mutation sequence: `Place` uses the taker order sequence, `Cancel`/`Amend`/`PurgeExpired` each
  consume one, and stop cascades anchor to the electing taker. Boundaries are entry-driven (one
  TRADE per taker with every fill, one depth/BBO per changed entry, status every `status_every`
  sequences), so the key is identical across replicas, failover and restart. `GAP` is diagnostic.
  A takeover snapshot's boundary is the **next** sequence.
- `OrderBook::amend` changes a plain resting limit order keyed like `cancel` (side + current price +
  sequence): a same-price volume reduction keeps the sequence (time priority) and updates in place;
  a new price or a larger volume re-inserts under a NEW sequence, which the `/write` response
  returns. Crossing amendments are refused (`amend_would_cross`); icebergs, parked stops and market
  orders are cancel + re-place. `OrderBook::spread`/`mid_price`/`depth` are read-only views (depth
  aggregates visible volume only, hidden iceberg reserve excluded).
- `Order` deserializes `sequence`; the scripts now send `sequence` too. `Order::sequence` is still
  required by the deserializer even though `place_order()` overwrites it.
- Fills compute maker/taker fees from the two orders' user-level `maker_fee_rate`/`taker_fee_rate` (signed fractions of notional). `MatchResult` carries `notional`, both fees and `fee_overflow`; `MatchEvent` carries the fees and flag, and market-data `TRADE`/fill `LIFECYCLE` mirror them. The engine records fees but does not maintain balances.
- Match results leave `apply` as `MatchEvent`s through a `MatchResultSink`. `EventId` is
  `(log_index, ordinal)`, so no event counter is stored in snapshots; `OrderBook::resting_volume`
  is the authoritative source for a `Rested` event's volume (do not derive it by subtraction).
- A process/cluster trades exactly one symbol. Replicated `StateMachineData.symbol_config` + `symbol_locked` (snapshot v9) hold its precision; `MatchEngineRequest::ConfigureSymbol` updates it through Raft before trading, locks it after the first accepted order, and queues a market-data reset snapshot when the node is publishing. `apply` rejects `Place`/`Amend` values with more decimals than configured and rejects orders with a different `symbol_id`. `/read symbol_config` and `/read symbol_id` expose it. Multi-symbol means one process/cluster per symbol, not multiple books in one process.
- `/read` keys are resolved by `read_state` in `src/network/api.rs`: `orderbook_orders`,
  `orderbook_stops`, `orderbook_last_trade_price`, `orderbook_depth` (`orderbook_depth:N` for N
  levels, capped at 200), `orderbook_spread`, `marketdata_status`, and `orderbook_sequence`
  (misspelled variant `orderbook_sequance` also accepted). Any other key returns an empty string;
  there is no KV store.

## Docs vs code

- Trust `src/` over prose. `README.md`, `README_CN.md` and `TODO.md` now match the
  WAL + snapshot + gRPC design; README requires Rust ≥ 1.89 (edition 2024).
- `docs/PERFORMANCE.md` records the measured order-book, end-to-end, feed-load and storage baselines and how to reproduce them.
- `docs/AERON_SOLUTION.md` is an investigation document. `docs/TRANSPORTS.md` is historical: the
  gRPC/tonic implementation (`src/network/grpc/`) is now the only Raft transport; the HTTP/JSON
  fallback and its comparison were removed on 2026-10-30.
- `docs/MARKET_DATA_PLAN.md` tracks the market-data egress implementation (tap, shadow book,
  deterministic `market_seq`/`sub_seq` sequencing, binary wire format v5 (scale-18 `i128`
  prices/quantities/fees plus fee assets), JSONL sink, leader gating); the Aeron adapter is still
  the next phase.
- `docs/FEE_DEDUCTION.md` documents the implemented base/quote volume semantics,
  fiat/stablecoin/equity/ETF asset metadata, tick/lot rules and fee-asset policies. Remaining
  extensions (sessions/corporate actions, fixed/regulatory fees, fee tokens) are listed there.
- `docs/DECIMAL.md` compares integer ticks, `rust_decimal`, `fastnum`, `bigdecimal`, `decimal`
  and `fixed`, with microbenchmarks, an 18-decimal BTC range analysis and a migration plan. `Order`
  now uses 18-scale `Fixed` integers end-to-end (`last_trade_price`, events, depth/BBO, market data,
  wire v5, snapshot v9). `SymbolConfig` is replicated and enforced for the one symbol a
  process/cluster trades; multi-symbol isolation is deployment-level (one process per symbol).
- `docs/MATCH_RESULTS.md` describes match-result delivery. Phases 1a–3 are implemented: events in
  `/write`, a replicated bounded event log in snapshots (v9), `POST /events` +
  `GET /events/status`, and `client_order_id` submission dedupe. `--match-event-log` remains a
  node-local debugging sink. Phase 4 (long-poll, gRPC streaming, MQ bridge) is still a proposal.
