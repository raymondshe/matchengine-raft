# Demo order desk

A standalone order-entry WebUI for a running `matchengine-raft` node. It runs as a **separate
process** in `demo/`; the engine binary is not modified.

The demo has two pieces:

- an Actix Web BFF (`demo/src/`) that calls the engine's public HTTP API through the leader-aware
  `MatchEngineClient`,
- a dependency-free HTML/CSS/JavaScript UI (`demo/web/`) that reuses `web/console/app.css` and adds
  only the trading-desk layout.

## Run

Start and initialize an engine first:

```shell
cargo run --bin matchengine-raft -- \
  --id 1 \
  --http-addr 127.0.0.1:24301 \
  --data-dir /tmp/matchengine-raft-demo \
  --instance-prefix demo \
  --snapshot-interval-ms 0

curl -s -X POST http://127.0.0.1:24301/init \
  -H 'Content-Type: application/json' -d '[]'
```

Then start the demo process:

```shell
cargo run --manifest-path demo/Cargo.toml -- \
  --addr 127.0.0.1:24300 \
  --engine 127.0.0.1:24301 \
  --engine-id 1
```

Open <http://127.0.0.1:24300/demo/>.

- `--addr`: demo WebUI/BFF address.
- `--engine`: HTTP API address of the running engine node.
- `--engine-id`: that node's Raft id.

The engine must already be initialized and have a leader before orders are accepted. The demo can
start earlier; it shows a "no leader" banner until the engine is ready. Point `--engine` at a
leader for the freshest book; if pointed at a follower, writes/events still forward, but local
`/read` views may lag.

## Demo flow

1. **Symbol** — review or configure the one symbol before the first accepted order.
2. **Trade** — place a limit/market order, watch depth/open orders, and see the write result and its
   immediate events.
3. **Full book** — press **Refresh full book** to load every resting order, every aggregated price
   level and parked stops. This is a manual debug view; it is not part of the 1 s poll.
4. **Events** — follow the replicated `/events` cursor, filter by kind/side/order, click an event
   for its raw JSON, pause/resume, and reset the cursor. A retained-window gap is reported instead
   of silently skipped.

Fee fields take decimal rate fractions, not basis points: `0.001` = 10 bps = 0.1%, and
`0.002` = 20 bps = 0.2%. The maker side pays the maker rate; the taker side pays the taker rate.

## Safety

- The engine and demo listeners are plaintext and unauthenticated; run them on loopback or a
  trusted private network behind a gateway.
- Demo writes require `X-Demo-Action: submit`, but that is not authentication.
- Orders are real replicated writes. There are no balances or settlement; fees are reported by the
  engine. Reset a demo by restarting the engine with a fresh `--data-dir`.

## Checks

```shell
cargo test --manifest-path demo/Cargo.toml
cargo clippy --manifest-path demo/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path demo/Cargo.toml -- --check
```
