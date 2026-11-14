# Project Structure

```
src/
├── main.rs                  # Server entry point (flags, env, storage config)
├── client.rs                # Smart client with leader forwarding, retries and timeouts
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
│   ├── health.rs            # /health and /ready probes
│   ├── console.rs           # Embedded console and guarded actions
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
├── dump_snapshot.rs         # JSONL dump of the current snapshot
├── replay_orders.rs         # Replay orders and compare match-result JSONL
└── replay_orders_events.rs  # Replay orders and compare events from POST /events
demos/
├── order_system/            # Standalone order desk (separate crate)
├── market_data_system/      # Standalone Binance-style WebSocket market-data service
└── k8s_control_plane/       # Reference Kubernetes control plane (separate crate)
tests/
├── cluster/
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
    ├── test_symbols.rs        # One-symbol config, gateway symbol guard and symbol lock
    ├── test_fees.rs           # Maker/taker fees through API, events, feed and restart
    ├── test_fee_assets.rs     # QQQ/USD and BTC/USDT tick/lot, quote volume and fees
    ├── test_request_limits.rs # Hostile input and request-size limits
    ├── test_batch_write.rs    # Batch writes in one Raft entry
    ├── test_health.rs         # /health and /ready probes
    ├── test_console.rs        # Embedded console and guarded leadership actions
    ├── test_feature_matrix.rs # Public-API order-type matrix and lifecycle events
    ├── test_client.rs         # Client retries, typed errors and failover forwarding
    └── test_failure.rs        # Fail-stop on Raft/WAL errors and SIGKILL recovery
└── replay/
    ├── README.md              # Scenario matrix, ports and regeneration notes
    ├── run-all.sh             # Run every checked-in replay case
    ├── limit_orders/          # GTC price-time priority and partial rest
    ├── ioc_orders/            # IOC partial fills and cancellations
    ├── fok_orders/            # FOK rejection and full fills
    ├── gtd_orders/            # GTD expiry and already-expired rejection
    ├── day_orders/            # Day close purge and no-close behaviour
    ├── market_orders/         # Market sweeps and cancelled remainders
    ├── post_only_orders/      # Post-only rests and crossing rejections
    ├── iceberg_orders/        # Visible tranche and hidden reserve
    ├── stop_market_orders/    # Stop-market election and child orders
    ├── stop_limit_orders/     # Stop-limit election and child orders
    ├── amend_orders/          # Amend priority rules and rejections
    ├── cancel_orders/         # Cancel lifecycle and parked stops
    ├── fee_orders/            # Maker/taker fees, rebates and both fee assets
    ├── fee_quote_orders/      # ETF/fiat quote fees and quote-volume normalization
    ├── fee_overflow_orders/   # Notional overflow and structured error reporting
    └── fee_overflow_base_orders/ # Overflow with an exact base fee
benches/
├── matchengine.rs           # Order-book microbenchmarks
├── book_replay.rs           # Book-only sweeps, cancels and BBO with p50/p99
├── level_book.rs            # Per-level bucket prototype comparison
├── apply_path.rs            # Apply-path heap allocations per entry
├── e2e.rs                   # Submit -> commit -> apply -> response
├── feed_load.rs             # Market-data feed load test
├── storage.rs               # WAL append/read/recovery
├── p0_bench.rs              # Lock/scan A/B benchmarks
└── bench_support/           # Shared warm/cold p50/p99 timing harness
```
