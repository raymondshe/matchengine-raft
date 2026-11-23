# Project Structure

The repository is a set of five sibling projects. Four are Rust crates, each with its own
`Cargo.toml`, lockfile and README; `trading_terminal/` is a standalone React/TS frontend with its
own Vite build and no Rust dependency. Repository-wide docs, scripts and Dockerfiles stay at the root.

```text
.
├── match_engine/                 # Raft-replicated matching engine
│   ├── Cargo.toml
│   ├── src/                      # engine, order book, storage, network, market data
│   ├── proto/raft.proto          # gRPC Raft service
│   ├── examples/                 # dump and replay tools
│   ├── benches/                  # book, apply, storage and e2e benchmarks
│   ├── tests/cluster/            # cluster, recovery, snapshot and failure tests
│   ├── tests/replay/             # checked-in order-type replay matrix
│   ├── fuzz/                     # cargo-fuzz crate (outside the main package)
│   ├── docker/                   # engine image definitions
│   └── web/console/              # embedded operational console assets
├── k8s_control_plane/            # CRD manager, operator, renderer and GUI
│   ├── Cargo.toml
│   ├── src/
│   ├── static/
│   ├── deploy/                   # CRDs, RBAC and operator manifests
│   └── examples/                 # example shard catalogs
├── market_data_system/           # Aeron/NATS/JSONL consumer, WebSocket service and TDengine bars
│   ├── Cargo.toml
│   ├── src/history/              # TDengine REST client, bar builder, query and writer task
│   ├── src/config.rs             # --history and --tdengine-* flags
│   ├── tests/history.rs          # end-to-end test gated on TDENGINE_URL
│   ├── docker/k8s-history.Dockerfile
│   ├── tests/
│   ├── benches/
│   └── web/
├── order_system/                 # standalone demo order desk
│   ├── Cargo.toml
│   ├── src/
│   └── web/
├── trading_terminal/             # React/TS spot terminal (no Rust, own Vite build)
│   ├── src/
│   ├── nginx/                    # same-origin /md and /demo/api proxies
│   └── Dockerfile
├── docs/                         # cross-project design, API, runbook and performance notes
├── scripts/                      # cluster, lab and demo orchestration
├── .github/workflows/            # CI, fuzz and benchmark workflows
├── README.md
├── README_CN.md
├── AGENTS.md
├── TODO.md
└── LICENSE
```

## Engine layout (`match_engine/`)

```text
src/
├── main.rs                  # Server entry point (flags, env, storage config)
├── client.rs                # Leader-aware client with retries and timeouts
├── lib.rs                   # Node composition, servers and shutdown
├── app.rs                   # Application state
├── order_book/              # Matching engine, fixed-point values and conformance tests
├── market_data/             # Tap, shadow book, wire format and Aeron egress
├── match_result/            # Event ids, sinks and replicated event-log integration
├── network/                 # HTTP API, management, console and gRPC Raft transport
└── store/                   # Segmented WAL, state machine, snapshots and config
```

Engine commands run from `match_engine/` (or with `--manifest-path match_engine/Cargo.toml` from
the repository root). The standalone projects have the same convention with their own manifests.
