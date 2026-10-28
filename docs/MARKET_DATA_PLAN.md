# Market-data egress — implementation plan

Status: **in progress.** Design rationale lives in
[`AERON_SOLUTION.md`](./AERON_SOLUTION.md) Part II (§11–26). This file turns that design into
ordered, testable units of work.

Scope of this implementation: the deterministic **feed pipeline** — tap, depth journal, shadow
book, message model, throttled publisher, JSONL sink, leader gating, snapshots, `/read` status and
an end-to-end test. The **Aeron transport** is deliberately the next phase: it is an adapter over
the message bytes this plan produces, and adding a media driver is a separate operational decision.

## Decisions already fixed (from AERON_SOLUTION Part II)

- Timestamps: the receiving leader stamps `Order.timestamp` at intake (overwriting client values)
  and every market-data record uses it. Never a publish-time clock.
- Identity and ordering: `(log_index, ordinal)` from the replicated event stream; timestamps bucket
  and label, they never order.
- Matching is untouched: the tap only records values each book operation already has; all
  derivation, encoding, throttling and IO run on a separate thread.
- Delivery: leader-only by default; snapshot + deltas; latest-value BBO; coalesced depth; batched
  trades; `GAP` only on a hard failure. Trades are never sampled.
- Multi-symbol ready schema (`symbol_id` in every message); the engine currently runs one book, so
  the value is `0` until the engine learns symbols.

## Work units

### U1 — Depth journal in `OrderBook` (matching thread)

Record the net visible-volume/order-count change of every book mutation without extra tree walks:

- `rest` → `+visible, +1 order`
- partial fill → `-matched, 0`
- full visible tranche → `-consumed, 0` plus `+refill` when an iceberg refills, `-1 order` when it
  is removed
- `cancel`/`purge_expired` of a resting order → `-visible, -1 order` (parked stops are not depth)
- `amend`: in-place reduction → `new - old, 0`; re-insert → `-old at old price`, `+new at new price`

Journal is opt-in (`enable_depth_journal`), transient (`#[serde(skip)]`, not in snapshots), and
`take_depth_journal` drains it per apply batch. Hidden iceberg reserve is never recorded.

**Done when:** unit tests cover every mutation and the journal stays empty when disabled.

### U2 — Events carry side; intake timestamps are authoritative

- Add `side: OrderSide` to `MatchEvent` (the order the event is about; maker side on fills is the
  opposite). Needed by the trade feed.
- Stamp `Order.timestamp` in the `/write` handler before proposing: `Place`, `Cancel` and `Amend`
  get the leader's wall clock in ms; `PurgeExpired` already carries `now_ms`. `apply` still never
  reads a clock.
- Update the GTD expiry test to derive its deadline from the stamped intake time.

**Done when:** event JSON/state-machine tests pass and the cluster suite is green with the new
stamping.

### U3 — `src/market_data` core (off the matching thread)

New module with no new dependencies:

- **Config**: log path, depth levels, BBO/depth/lifecycle/status intervals, trade/lifecycle batch
  caps, bar intervals (default `1s`, `1m`), queue capacity, leader-only flag.
- **Message model**: `TRADE` (batch), `BBO`, `DEPTH_SNAPSHOT`, `DEPTH_DELTA` (post-batch totals),
  `LIFECYCLE` (batch), `STATUS`, `GAP`, `BAR` (closed + partial) — the catalogue in §12/§16.
- **Wire encoder** (`wire.rs`): 64-byte header (magic `AMKT`, version, type, flags, stream seq,
  log position, epoch, leader, ts, symbol) plus bodies, little-endian, matching §16 exactly.
- **Shadow book**: per-side price levels keyed by positive `f64` bits; applies journal deltas,
  tracks order counts, BBO and the last trade.
- **Processor**: drains frames, updates the shadow, accumulates/coalesces, emits on token/interval
  budgets: trades batched (never sampled), BBO latest-value, depth deltas coalesced per level,
  lifecycle batched with `rested` shed first, OHLC bars closed + live, status heartbeat, gap
  marker. No emission while not leader; a snapshot resets the shadow and suppresses older frames.
- **Sinks**: `MarketDataSink` trait, `JsonlSink` (debug, node-local) and a null sink. The trait is
  the seam the Aeron adapter will implement.
- **Tap**: bounded `std::sync::mpsc::SyncSender<Ingest>`; `try_send` never blocks `apply`; overflow
  increments a counter and requests a `GAP`.

**Done when:** unit tests cover shadow deltas, BBO, depth coalescing, bar windows, lifecycle
shedding, gaps, snapshots, and every wire message round-trips its header/body fields.

### U4 — State-machine tap, snapshots and read status

- `StateMachineStore` accepts an optional `MarketDataTap`; after each applied entry it publishes
  `TapFrame { log_index, ts_ms, symbol_id, events, journal }` with `try_send`.
- `capture_snapshot(depth_levels)` builds a `SnapshotFrame` under the state-machine lock:
  last applied index, timestamp, top-N levels per side, last trade, book sequence, pending stops.
- `install_content` clears the journal; the tap is untouched (replicas keep applying).
- `/read marketdata_status` returns feed counters and the gate state.

**Done when:** an apply-level test sees the frame (events + deltas, correct timestamp) through a
channel tap, and the status JSON matches the counters.

### U5 — Node wiring, leader gating, integration test

- `StorageConfig` gains `market_data: Option<MarketDataConfig>` (builder + `MARKET_DATA_LOG`);
  CLI flags: `--market-data-log`, `--market-data-depth`, `--market-data-bbo-interval-ms`,
  `--market-data-bar-intervals-ms`, `--market-data-leader-only`.
- `build_match_engine_node` starts the processor thread, installs the tap, spawns a task watching
  `raft.metrics()`: on leadership gain, push a fresh snapshot then enable publishing; on loss,
  disable. `MatchEngineNode::shutdown` stops both.
- New `tests/cluster/test_market_data.rs`: single node with a JSONL sink; place, fill, cancel and
  amend; assert trade/BBO/depth/bar/status lines and the status endpoint.

**Done when:** the integration test passes and the cluster suite stays under ~15 s.

### U6 — Docs, benchmarks, gates

- README/README_CN, `GUIDE_cn.md`, `AGENTS.md`, `TODO.md`; update the AERON Part II status banner.
- Bench: `market-data tap off/on` and journal cost per mutation (the "matching must not regress"
  gate).

**Done when:** `cargo test`/`cargo bench` are green and the docs describe actual behaviour.

### U7 — Deferred (documented, not in this change)

- **Aeron adapter**: a `MarketDataSink` that encodes via `wire.rs` and offers to streams 2001+;
  `rusteron-client` + an embedded driver in tests, feature-gated (`market-data-aeron`).
- **Archive** recording/replay and the snapshot-request stream.
- **Multi-symbol routing**: `symbol_id` on orders and per-symbol books in the state machine; the
  message schema already carries the field.
- **Aeron Cluster / Raft transport**: untouched by this work (Part I gate).

## Message catalogue and streams (reference)

| Stream | Body | Notes |
|---|---|---|
| 2001 | `TRADE` batch | never sampled; `GAP` only on hard failure |
| 2002 | `BBO` | latest-value, coalesced |
| 2003 | `DEPTH_SNAPSHOT` | reset + recovery path |
| 2004 | `DEPTH_DELTA` | post-batch totals per touched level |
| 2005 | `LIFECYCLE` | batched; `rested` shed first under pressure |
| 2006 | `STATUS` | 100 ms heartbeat, gate state |
| 2007 | `BAR` | trade OHLCV, closed + live (`partial`) |

Header (64 B): magic `AMKT`, version `1`, type, flags (`SNAPSHOT|RESET|GAP|END_OF_BATCH|THROTTLED|
PARTIAL_BAR|BAR_CLOSE`), stream seq, `log_index`, `ordinal`, body length, epoch, leader id, `ts_ms`,
`symbol_id`, reserved.

## Test matrix

| Layer | Tests |
|---|---|
| Book | journal per mutation; journal off costs nothing; amend deltas |
| Events | side on every kind; amended/fill JSON fields |
| Wire | each message header + body encoding, flags, lengths |
| Processor | shadow/BBO, depth coalescing, bar windows (closed/live), lifecycle shedding, gap, snapshot reset |
| Apply | tap frame carries events/deltas/timestamp; status counters |
| End-to-end | JSONL node: trade, BBO, depth, bar, status; leader-only gate |

## Working rules

- Each unit lands with its tests and a doc touch where behaviour is user-visible.
- No new crates in U1–U6; the Aeron adapter is the only place that will add one.
- Snapshot/log formats do not change (journal and feed state are transient); `SNAPSHOT_VERSION`
  stays 3.

## Progress

| Unit | Status | Notes |
|---|---|---|
| U1 | **done** | `OrderBook::depth_journal`, `DepthChange`, opt-in recording, tests per mutation |
| U2 | **done** | `MatchEvent.side`; leader stamps `Order.timestamp` at intake; GTD test made clock-relative |
| U3 | **done** | `src/market_data/{mod,wire}.rs`: shadow book, processor, sinks, tap, 11 unit tests |
| U4 | **done** | `open_with_sinks`, tap frames, `capture_market_data_snapshot`, `marketdata_status` |
| U5 | **done** | `MarketDataConfig` on `StorageConfig`, CLI flags, leader watcher, end-to-end test on 21303 |
| U6 | **done** | README/README_CN/AGENTS/TODO/AERON updates; bench `rest 1k bids + journal` |
| U7 | deferred | Aeron adapter, Archive, multi-symbol engine |

