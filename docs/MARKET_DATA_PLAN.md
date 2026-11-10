# Market-data egress — implementation plan

Status: **implemented for U1–U6, U8 and the U11–U14 reference service; U7 deferred; U9, U10 and U15 partial.** Design rationale lives in
[`AERON_SOLUTION.md`](./AERON_SOLUTION.md) Part II (§11–26). This file turns that design into
ordered, testable units of work. The consumer side of U7 — a **separate market-data system** that
subscribes to the Aeron multicast feed and serves Binance-style WebSocket streams — is designed in
[`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md), and its implementation is planned below
as **U9–U15**.

Scope of this implementation: the deterministic **feed pipeline** — tap, depth journal, shadow
book, message model, deterministic publisher, JSONL sink, leader gating, snapshots, `/read`
status and an end-to-end test. The **Aeron transport** is deliberately a later phase: it is an
adapter over the message bytes this plan produces, and adding a media driver is a separate
operational decision. That phase and the **separate market-data system** that consumes it are now
planned as U9–U15 below, from [`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md).

## Decisions already fixed (from AERON_SOLUTION Part II)

- Timestamps: the receiving leader stamps `Order.timestamp` at intake (overwriting client values)
  and every market-data record uses it. Never a publish-time clock.
- Identity and dedupe: `(symbol_id, stream_id, market_seq, sub_seq)`, anchored on the replicated
  order/mutation sequence (the taker order sequence for a `Place`) with a deterministic per-entry
  `sub_seq`. `(log_index, ordinal)` stays the audit key. Implemented in U8.
- Matching is untouched: the tap only records values each book operation already has; all
  derivation, encoding and IO run on a separate tokio task.
- Delivery: leader-only by default; snapshot + deltas; BBO and depth totals per applied entry; one
  uncapped `TRADE` per taker; `GAP` only on a hard failure. Trades are never sampled.
- Multi-symbol ready schema (`symbol_id` in every message). One process/cluster trades exactly one
  symbol, so the value is the configured process symbol; multi-symbol means separate deployments.

## Work units

### U1 — Depth journal in `OrderBook` (matching path)

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

### U3 — `src/market_data` core (off the matching path)

New module with no new dependencies:

- **Config**: log path, depth levels, `status_every` (default 100), lifecycle record cap, bar
  intervals (default `1s`, `1m`), queue capacity, leader-only flag.
- **Message model**: `TRADE` (one per taker, all fills), `BBO`, `DEPTH_SNAPSHOT`, `DEPTH_DELTA`
  (post-entry totals), `LIFECYCLE` (one per entry), `STATUS`, `GAP`, `BAR` (closed + partial) — the
  catalogue in §12/§16.
- **Wire codec** (`wire.rs`): 64-byte header (magic `AMKT`, version 6, type, flags, `market_seq`,
  log position, epoch, leader, ts, symbol, `sub_seq`) plus bodies, little-endian, matching §16.
  Version 5 encodes every price/quantity/quote volume/fee as a scale-18 `i128` and carries the
  fee asset per side. `encode` has a strict reference `decode` that rejects malformed frames;
  golden round-trip tests cover every body type.
- **Shadow book**: per-side price levels keyed by `Fixed` (scale 18); applies journal deltas,
  tracks order counts, BBO and the last trade.
- **Processor**: drains frames, updates the shadow, and emits one deterministic batch per entry:
  all fills in one `TRADE`, one BBO per changed book, post-entry depth totals, one lifecycle
  message with `rested` shed first, OHLC bars closed + live, status every `status_every` sequences,
  gap markers. No emission while not leader; a snapshot resets the shadow, sets the exclusive
  boundary and replays frames captured during the reset.
- **Sinks**: `MarketDataSink` trait, `JsonlSink` (debug, node-local) and a null sink. The trait is
  the seam the Aeron adapter will implement.
- **Tap**: bounded `std::sync::mpsc::SyncSender<Ingest>`; `try_send` never blocks `apply`; overflow
  increments a counter and requests a `GAP`.

**Done when:** unit tests cover shadow deltas, BBO, bar windows, lifecycle shedding, gaps,
snapshots, and every wire message round-trips its header/body fields.

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
  CLI flags: `--market-data-log`, `--market-data-depth`, `--market-data-status-every`,
  `--market-data-bar-intervals-ms`, `--market-data-leader-only`.
- `build_match_engine_node` starts the processor task, installs the tap, spawns a task watching
  `raft.metrics()`: on leadership gain, push a fresh snapshot then enable publishing; on loss,
  disable. `MatchEngineNode::shutdown` stops both.
- New `tests/cluster/test_market_data.rs`: single node with a JSONL sink; place, fill, cancel and
  amend; assert trade/BBO/depth/bar/status lines and the status endpoint.

**Done when:** the integration test passes and the cluster suite stays under ~15 s.

### U6 — Docs, benchmarks, gates

- README/README_CN, `AGENTS.md`, `TODO.md`; update the AERON Part II status banner.
- Bench: `market-data tap off/on` and journal cost per mutation (the "matching must not regress"
  gate).

**Done when:** `cargo test`/`cargo bench` are green and the docs describe actual behaviour.

### U8 — Deterministic sequencing (done)

Every message must carry an identity that is identical on leader and followers, monotonic, and
stable across restart and failover: `(symbol_id, stream_id, market_seq, sub_seq)`.

- **`market_seq`** is the sequence assigned in `apply`. For a `Place` it is the taker order sequence
  (`OrderBook::sequence`), already replicated and snapshotted. Extend the same counter to every
  mutating request: `Place` keeps it as the order sequence, `Cancel`/`PurgeExpired` consume one as
  the event id, and `Amend` consumes one for the event (using it as the reinserted order's sequence
  when priority is lost). Stop cascades anchor to the electing taker.
- **`sub_seq`** is the 0-based message index within the anchor, produced by deterministic
  entry-driven boundaries: **all fills of a taker in one TRADE message (uncapped)**, one depth
  message per entry, one BBO per changed entry, bars closed on the entry whose trade rolls the
  window. No wall-clock flush in the replicated path.
- **All mutating requests** (`Place`, `Cancel`, `Amend`, `PurgeExpired`) consume a sequence and run
  the automatic GTD purge at their intake timestamp; `Day` still needs the explicit close.
- **Leader-only keys are frame-derived**, so followers do not need to derive for key equality; a
  new leader resets from a snapshot (boundary = next `market_seq`) and continues the same keys.
- **Wire**: `market_seq` replaces `stream_seq`, `sub_seq` takes two reserved bytes, `version = 5`.
  `STATUS` is deterministic every `status_every` sequences; `GAP` is diagnostic. `wire::decode` is
  the reference consumer and rejects corrupt/truncated frames.
- **Acceptance**: two processors fed the same frames emit identical messages; a takeover snapshot
  continues the same keys; one TRADE carries every fill; no `Instant` in the data path; wire golden
  tests and integration assertions cover the keys.
- **Status: done.**

### U7 — Deferred (documented, not in this change)

- **Aeron adapter**: now planned as **U10** below, per
  [`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md) §4.2 (feature `market-data-aeron`,
  one exclusive publication per stream, embedded driver in tests).
- **Archive** recording/replay and the snapshot-request stream.
- **Multi-symbol deployment**: one process/cluster per symbol; the message schema already carries
  `symbol_id` for routing and dedupe across deployments.
- **Aeron Cluster / Raft transport**: untouched by this work (Part I gate).

## Work units — the market-data system over Aeron + WebSocket (U9–U15)

These units realize [`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md): the engine publishes
the feed to Aeron multicast (U9–U10) and a separate `marketdata/` crate consumes it and serves
Binance-style WebSocket streams (U11–U15). Function-point IDs (FP-…) are the agreed breakdown for
planning; each unit lists the points it delivers.

| Unit | Scope | Function points |
|---|---|---|
| U9 | engine feed prerequisites | FP-E1, FP-E2, FP-E3 |
| U10 | Aeron publication adapter (feature `market-data-aeron`) | FP-E4–E8, FP-T1 |
| U11 | service crate, ingestion, assembly | FP-S1–S5 |
| U12 | WS hub: streams, conflation, snapshot-on-subscribe | FP-H1–H4, FP-P4 |
| U13 | client protocol and session lifecycle | FP-P1–P3, FP-P5–P7 |
| U14 | resilience, ops, scale-out | FP-R1–R4, FP-O1–O3, FP-X1 |
| U15 | end-to-end integration, benchmarks, docs | FP-T3, FP-T4 |

Order and parallelism: **U9 first** (small, unblocks both sides). After U9, **U10** (engine) and
**U11–U13** (service, driven by a channel source with no media driver) can proceed in parallel;
U14 builds on U11–U13; only **U15** needs U10 and U14 together. Deferred P2/P3 work is listed
after the units.

**Reference implementation:** `demos/market_data_system/` implements U11–U14 (JSONL and Aeron
sources, assembler, 100/1000 ms conflation, Binance-style WS, limits, health/metrics). It is
multi-symbol: one instance runs a feed/state per `--symbol` and shares one WebSocket hub, so a
connection can hold several symbols' streams and switch symbols with live
`UNSUBSCRIBE`/`SUBSCRIBE` on the same socket. On the engine side, `start_with_sink`, the
`market-data-aeron` `AeronSink` (embedded or external driver, one exclusive publication per
stream, periodic reset snapshot, idle heartbeat and snapshot-request stream 2010) and the
periodic reset snapshot are in place; U9's empty depth deltas and bar `q` and U15's `ws_fanout`
numbers remain open — the service derives depth contiguity and kline quote volume itself.

### U9 — Engine feed prerequisites for distribution (P0)

Make the feed consumable by Binance-style clients without touching matching.

- **Per-entry depth contiguity (FP-E1):** the processor emits one `DEPTH_DELTA` per mutating
  entry, with an empty level list when no visible depth changed (rejections, parked-stop
  activity, a `Day` purge of stops). The catalogue table below becomes "one message per entry,
  possibly empty". Golden and processor tests cover the empty case.
- **Custom sink seam (FP-E2):** `MarketDataHandle::start_with_sink(config, sink)`; `log_path`
  keeps selecting `JsonlSink`, and a caller may install a custom sink (Aeron in U10, test sinks
  in tests) alongside it.
- **Bar quote volume (FP-E3):** `BarRecord` gains summed quote notional for the kline `q` field.
  This is a wire body change, so the wire version bumps v6 → v7 with updated golden frames and a
  strict decoder that rejects v6; the JSONL serde output gains the field additively.

**Done when:** every mutating entry yields exactly one depth message (empty allowed, rejections
and parked stops included); two processors fed identical frames still emit identical messages;
`start_with_sink` installs a test sink; bar goldens carry `q`; wire v7 rejects v6; no
matching-path change (`cargo bench --bench matchengine` gate).

### U10 — Aeron publication adapter (P1a, feature `market-data-aeron`)

The engine side of the market-data system: publish the deterministic feed to Aeron multicast.

- Feature `market-data-aeron`: optional `rusteron-client` + `rusteron-media-driver` dependencies,
  default off — the default build and `cargo test` still need no driver.
- `src/market_data/aeron.rs`: `AeronSink` implementing `MarketDataSink` — `wire::encode` each
  message, one **exclusive publication per stream** (2001–2007) so a depth snapshot cannot
  head-of-line-block a trade, publications created lazily on leadership gain and closed on loss.
- A dedicated publication thread owns the `!Send` handles, fed by the processor task through a
  bounded handoff; `try_offer` with retry under backpressure; gap counters. A stalled publication
  slows the processor and degrades the feed to `GAP` — never an `apply` stall (AERON §17).
- **Periodic snapshot re-broadcast (FP-E6):** re-publish the reset `DEPTH_SNAPSHOT` every
  `--market-data-aeron-snapshot-every-ms` (default 30 000, 0 off) so consumers can (re)join
  without waiting for an election.
- **Idle heartbeat (FP-E7):** a diagnostic `STATUS` with a reserved `sub_seq` every
  `--market-data-aeron-heartbeat-ms` (default 1000) when nothing was applied, carrying
  epoch/leader/watermark.
- **Snapshot request (U7):** stream 2010 carries a fixed 24-byte subscriber request; the node
  captures a fresh snapshot (clamped to its configured depth) and queues it through the same
  processor path as the periodic re-broadcast.
- CLI flags (FP-E8): `--market-data-aeron-channel`, `--market-data-aeron-stream-base` (2001),
  `--market-data-aeron-dir`, `--market-data-aeron-embedded-driver`,
  `--market-data-aeron-snapshot-every-ms` and `--market-data-aeron-heartbeat-ms`.
- Tests (FP-T1): embedded driver publish/receive per stream (golden bytes round-trip);
  backpressure → `GAP` and no `apply` stall; leadership gain/loss opens/closes publications;
  periodic snapshot and heartbeat cadence.

**Done when:** a subscriber decodes every golden frame byte-for-byte; the cluster suite passes
with the feature off; `cargo bench --bench matchengine` shows no regression with it on.

### U11 — Market-data service: crate, ingestion, assembly (P1b-1)

The consumer side begins: a new crate that turns Aeron frames into assembled state.

- New crate `marketdata/` (sibling of `demos/order_system/`, own `Cargo.toml`), binary
  `matchengine-marketdata`, CLI per `MARKET_DATA_WEBSOCKET.md` §13.2, CI fmt/Clippy/test jobs like
  `demos/order_system/` has (FP-S1).
- `FrameSource` trait (`Sourced::{Message, Loss}`) with a channel-backed source for driverless
  tests and `AeronSource`: one subscription per stream id, fragment reassembly, `wire::decode`, a
  dedicated subscriber thread owning the `!Send` handles, a bounded handoff queue whose overflow
  becomes a gap (FP-S2).
- Assembler: dedupe by `(symbol_id, stream_id, market_seq, sub_seq)`; apply
  snapshot/delta/BBO/bar/status; continuity watermark; gap policy — invalidate depth, keep
  latest-value streams, re-sync on the next periodic snapshot (FP-S3, FP-S4).
- Caches as `watch<Arc<…>>`: depth map, top-N, BBO, current bars, watermark (FP-S5).

**Done when:** channel-driven unit tests cover dedupe (duplicate frames, two Aeron sessions),
snapshot reset, delta totals, watermark and gap→re-sync; an embedded-driver test receives engine
frames through `AeronSource`; the crate's CI jobs are green.

### U12 — Market-data service: WS hub and streams (P1b-2)

Turn assembled state into Binance-shaped stream content.

- Serialize-once fan-out: per stream key (name + speed) a `broadcast<Bytes>` channel, lazily
  created and pruned; a registry behind one `std::sync::Mutex` never held across `.await`
  (FP-H1).
- Conflation: `@depth@100ms`/`@1000ms` windows from `tokio::time::interval`, merged `U..u`
  ranges with last-write-wins levels, empty window emits nothing; partial-depth caches refreshed
  per window (FP-H2).
- Realtime streams published per entry: `@trade`, `@sweep`, `@bookTicker`, `@kline_<interval>`,
  `@status`, `@lifecycle` (FP-H4).
- Payload encoders with golden JSON fixtures: `Fixed` decimal strings, fee fields, extension
  fields (`L`, `S`, `ts`, `ms`, `qv`) (FP-P4).
- Snapshot-on-subscribe: `depthSnapshot`, top-N, `bookTicker`, current kline pushed from the
  caches at subscribe (FP-H3).

**Done when:** every stream's golden payload matches its fixture; conflation and
snapshot-on-subscribe unit tests pass; a channel-driven instance serves every P1 stream.

### U13 — Market-data service: client protocol and sessions (P1b-3)

The Binance-parity client surface.

- Endpoints `GET /ws`, `/ws/<stream>`, `/stream?streams=…` and the combined-stream envelope
  wrapped around shared bytes (FP-P1, FP-P5).
- Stream-name parser: catalogue, speeds, kline intervals, `depth5/10/20`; repeatable
  `--symbol name:id:base:quote` specs validated against every frame (FP-P2).
- Control protocol: `SUBSCRIBE`/`UNSUBSCRIBE`/`LIST_SUBSCRIPTIONS` with id echo, error codes 0–4,
  10 inbound messages/s, 1024-stream cap, 4 KiB frame cap, `SET_PROPERTY` refused (FP-P3).
- Sessions spawned onto the main runtime via a captured `Handle` (never on actix worker threads);
  ping every 180 s, pong timeout 600 s, 24 h rotation, close codes 1000/1008/1011/1013 (FP-P6).
- Backpressure classes: `LatestValue` drain-to-latest vs `Sequenced` FIFO (1024 entries);
  `Lagged` → resync (depth) or close 1013 (FP-P7).

**Done when:** the protocol/limit tests of `MARKET_DATA_WEBSOCKET.md` §14 pass; a
`tokio-tungstenite` client maintains a correct local book through place/fill/amend/cancel using
Binance rules 3–6 against a channel-driven instance.

### U14 — Market-data service: resilience, ops, scale-out (P1b-4)

Survive every failure the design lists, and be operable.

- Engine failover: epoch change + `RESET` snapshot → rebuild caches, push `resync` +
  `depthSnapshot`, **keep client connections**; dedupe deposed-leader stragglers (FP-R1).
- Service restart: state rebuilt from the periodic snapshot + deltas; `/ready` gates load
  balancer routing (FP-R2).
- Graceful shutdown: stop watch → sessions close 1001 → assembler drains → Aeron thread
  releases → HTTP stops (FP-R3).
- Slow-instance recovery: Aeron loss or handoff overflow → gap → self re-sync; Max multicast
  flow control assumed, Min documented as forbidden (FP-R4).
- `/health`, `/ready` (snapshot current + heartbeat fresh), metrics (connections, messages sent,
  slow-consumer closes, resyncs, assembler gaps, watermark lag), config validation
  (symbol/id vs frames, bar intervals vs engine, channel/stream-base match) (FP-O1–O3).
- Two instances behind one load balancer serve identical stream content (FP-X1).

**Done when:** the §14 resilience rows pass; a leadership change keeps a client's connection up
with a correct book after re-sync; two instances produce identical payloads for the same frames.

### U15 — End-to-end integration, benchmarks, documentation (P1b-5)

Prove the whole path and record the numbers.

- End-to-end test (FP-T3): embedded Aeron driver, one engine node publishing (U10), one service
  instance, a `tokio-tungstenite` client asserting trade/bookTicker/depth/kline frames in order
  through place/fill/amend/cancel, a forced gap, and a leadership change — the client's
  connection stays up across the failover. A two-symbol case switches `UNSUBSCRIBE`/`SUBSCRIBE`
  on one socket and asserts `LIST_SUBSCRIPTIONS` and one connection in `/metrics` (implemented in
  `demos/market_data_system/tests/ws.rs`).
- Benchmarks (FP-T4): engine tap/publication cost with the feed on/off (the
  "matching must not regress" gate) and the service `ws_fanout` bench (N connections ×
  `bookTicker`/`depth@100ms`, serialize-once reuse, per-connection send cost, assembler memory).
  Results recorded in [`PERFORMANCE.md`](./PERFORMANCE.md).
- Docs: README/README_CN/AGENTS/TODO updated for the new crate, flags and runbook (media driver,
  channel, flow-control strategy, retention sizing).

**Done when:** the end-to-end test and both benches land; the docs describe actual behaviour;
CI is green including the new crate.

### Deferred (P2/P3, from MARKET_DATA_WEBSOCKET.md §16)

- `@aggTrade` price-grouped sweeps; `@miniTicker`/`@ticker` 24 h rolling window (service-local).
- `GET /marketdata/depth` REST snapshot endpoint (REST+WS sync parity).
- Per-IP connection throttling in-process (option); fleet autoscaling runbook.
- Multi-symbol routing by `symbol_id`; per-DC relays; the Kafka/NATS bridge variant.
- From U7: the snapshot-request stream 2010. Archive recording/replay is implemented in the
  reference service with `--archive-dir`/`--replay-dir` and `tests/archive.rs`.

### Test matrix additions (U9–U15)

| Layer | Tests |
|---|---|
| Engine processor (U9) | per-entry depth contiguity incl. empty deltas; identical output across two processors; bar `q` goldens; wire v7 golden + v6 rejection |
| Engine Aeron adapter (U10) | golden bytes per stream; embedded driver round-trip; backpressure → `GAP`, no `apply` stall; leadership lifecycle; snapshot/heartbeat cadence; stream-2010 request round-trip |
| Service ingestion (U11) | `FrameSource` channel driver; dedupe (duplicates, two sessions); snapshot reset; delta totals; watermark; gap→re-sync; heartbeat timeout |
| Service hub (U12) | conflation windows; serialize-once; snapshot-on-subscribe; golden payloads per stream |
| Service protocol (U13) | stream-name parsing; control protocol and error codes; limits (ping/pong, 24 h, caps); backpressure classes; Binance rules 3–6 as literal cases |
| Service resilience (U14) | failover keeps connections; restart rebuild; graceful shutdown; two instances identical content |
| End-to-end (U15) | engine + service + WS client through trades, gap and leadership change |

## Message catalogue and streams (reference)

| Stream | Body | Notes |
|---|---|---|
| 2001 | `TRADE` | one per taker, all fills, never sampled |
| 2002 | `BBO` | one per book-changing entry |
| 2003 | `DEPTH_SNAPSHOT` | takeover/startup reset + recovery path |
| 2004 | `DEPTH_DELTA` | post-entry totals per touched level |
| 2005 | `LIFECYCLE` | one per entry; `rested` shed first above the cap |
| 2006 | `STATUS` | every `status_every` (default 100) sequences; gap flags |
| 2007 | `BAR` | trade OHLCV, closed + live (`partial`) |
| 2010 | `SNAPSHOT_REQUEST` | subscriber → engine, fixed 24-byte request (`MSRQ`, v1) |

Header (64 B): magic `AMKT`, version `2`, type, flags (`SNAPSHOT|RESET|GAP|END_OF_BATCH|
PARTIAL_BAR|BAR_CLOSE`; bit 4 reserved), `market_seq`, `log_index`, `ordinal`, body length, epoch
(Raft term), leader id, `ts_ms`, `symbol_id`, `sub_seq`, reserved.

## Test matrix

| Layer | Tests |
|---|---|
| Book | journal per mutation; journal off costs nothing; amend deltas |
| Events | side on every kind; amended/fill JSON fields |
| Wire | each message header + body encoding, flags, lengths; reference decoder round-trips every body; a pinned v6 golden frame; malformed-frame rejection |
| Processor | shadow/BBO, per-entry depth totals, bar windows (closed/live), lifecycle shedding, gap, snapshot reset, identical keys across replicas |
| Apply | tap frame carries events/deltas/timestamp; status counters |
| End-to-end | JSONL node: trade, BBO, depth, bar, status; leader-only gate |

## Working rules

- Each unit lands with its tests and a doc touch where behaviour is user-visible.
- U1–U6 added no crates. The later exactness work added `i256` for price × quantity. U10 adds the
  feature-gated `rusteron-client`/`rusteron-media-driver` pair on the engine (default off); U11
  adds the separate `marketdata/` crate (`rusteron-client`, `actix-ws`, with `tokio-tungstenite`
  as a dev-dependency), which never touches the engine binary.
- The feed state (journal, shadow book, bars) is transient and absent from snapshots. U1–U6 kept
  `SNAPSHOT_VERSION` 3; later exactness/symbol/fee work bumped it to 11 for the replicated
  order/event fields, single-symbol precision configuration, fee data and schema-v11 order types,
  not for feed state.

## Progress

| Unit | Status | Notes |
|---|---|---|
| U1 | **done** | `OrderBook::depth_journal`, `DepthChange`, opt-in recording, tests per mutation |
| U2 | **done** | `MatchEvent.side`; leader stamps `Order.timestamp` at intake; GTD test made clock-relative |
| U3 | **done** | `src/market_data/{mod,wire}.rs`: shadow book, processor, sinks, tap, deterministic message keys, 26 module unit tests |
| U4 | **done** | `open_with_sinks`, tap frames, `capture_market_data_snapshot`, `marketdata_status`, status assertion |
| U5 | **done** | `MarketDataConfig` on `StorageConfig`, all CLI flags (incl. `--market-data-leader-only`), leader watcher, source-gated tap, end-to-end test on 21303 |
| U6 | **done** | README/README_CN/AGENTS/TODO/AERON updates; bench `rest 1k bids + journal` |
| U8 | **done** | deterministic `(market_seq, sub_seq)` across replicas; all mutations sequenced; auto GTD purge; one TRADE per taker; wire v7 encoder + reference decoder |
| U9 | **partial** | `start_with_sink` seam done; per-entry empty depth deltas and bar quote volume still open (the service derives contiguity and kline `q`) |
| U10 | **partial** | `AeronSink` (rusteron 0.2.13, embedded/external driver, one publication per stream, blocking publisher task, periodic reset snapshot, idle heartbeat, stream-2010 request subscription) and the external-driver runbook are done; Linux external-driver/multicast fan-out validation and leadership-scoped publication lifecycle remain open |
| U11 | **done (demo)** | `demos/market_data_system/`: `FrameSource` (JSONL + Aeron + channel), per-symbol feeds, assembler, dedupe/watermark/gap policy, caches |
| U12 | **done (demo)** | shared WS hub: serialize-once broadcast, 100/1000 ms conflation, snapshot-on-subscribe, Binance payloads, multi-symbol keys |
| U13 | **done (demo)** | Binance client protocol: raw/combined endpoints, live `SUBSCRIBE`/`UNSUBSCRIBE` across symbols on one connection, 1024-stream cap, ping/pong, 24 h rotation |
| U14 | **partial** | health/readiness, metrics, graceful shutdown, stateless multi-instance design; per-instance parity test and `/metrics` runbook still open |
| U15 | **partial** | engine→Aeron→service→WS end-to-end test and crate README done; `ws_fanout` benchmark and numbers are published (16/64 clients, 110-158k delivered frames/s on macOS loopback); Linux pinned run remains open |
| U7 | **partial** | Archive recording/replay is done in the reference service (`--archive-dir`/`--replay-dir`, `tests/archive.rs`); the snapshot-request stream 2010 is implemented on both engine and service, with an embedded-driver round-trip test. Multi-symbol is deployment-level. The Aeron adapter moved to U10 |

## Verification (2026-10-28)

| Check | Result |
|---|---|
| `cargo test` | **155 unit + 52 integration** tests pass (2026-11-04; includes exactness, symbol-config, fee, quote-volume, fee-asset and periodic-snapshot tests) |
| `cargo check --all-targets` | **0 warnings** |
| `cargo test --test cluster test_market_data` | passed; checks `market_seq` anchors for place/amend/cancel and the uncapped trade message |
| U8 determinism | two processors fed identical frames emit identical messages; a takeover snapshot continues the same keys; wire v7 golden tests (scale-18 `i128`) |
| automatic purge | `Place`/`Cancel`/`Amend` purge due GTD at intake time; `Day` only via `PurgeExpired`; guarded by the earliest-expiry index (no scan without GTD) |
| `matchengine-raft --help` | all feed flags present, including `--market-data-status-every`, `--market-data-leader-only` and `--market-data-aeron-heartbeat-ms` |
| Snapshot/log formats | U8 itself kept `SNAPSHOT_VERSION` 3; later work bumped it to 4 (event window + dedupe map), 5 (fixed-point order prices/quantities), 6 (last trade + match events fixed-point), 7 (single-symbol precision configuration), 8 (maker/taker fee rates and fill fees), 9 (asset metadata, tick/lot, volume units and fee assets), 10 (`MatchErrors` replacing the boolean fee-overflow flag) and 11 (owner identity, market-to-limit, trailing stops, pegs and STP). `market_seq` lives in the existing `OrderBook::sequence` |
| Dependencies | `i256 0.2.5` added for exact notional multiplication; no other dependencies |

Bench (best-of-5 per round, after the earliest-expiry guard): rest 1k bids **40.1 µs**, rest 1k +
journal **42.8 µs** (~7% with the tap on), sweep 1k/100 levels 49.1 µs, deep 20k sweep 929.5 µs,
partial 36.7 µs, IOC 69.6 µs, iceberg 36.9 µs, amend 69.2 µs, cancel 69.4 µs. The same suite on
`23a3f71` (before U8) measured 37.3 / 39.5 / 46.9 / 913.4 / 34.0 / 64.6 / 35.2 / 67.0 / 67.8 µs;
the difference is inside this machine's ~10% run-to-run spread. The automatic GTD purge is O(1) on
a book with no GTD orders and only scans when a deadline is due; the sweep checks maker TIFs only
when the book actually holds GTD orders.

### Sequencing (resolved by U8)

Messages are identified by `(symbol_id, stream_id, market_seq, sub_seq)`:

- `market_seq` is the replicated mutation sequence; `Place` uses the taker order sequence and
  `Cancel`/`Amend`/`PurgeExpired` each consume one. Stop cascades anchor to the electing taker.
- All fills of a taker are in one `TRADE` message; other products emit once per entry.
- Status is emitted when `market_seq % status_every == 0`; gap is diagnostic and uses the
  reserved `sub_seq = u16::MAX`.
- The takeover snapshot is an exclusive boundary at the next sequence; its reset BBO uses
  `sub_seq = 1` so the first post-snapshot frame's BBO at `sub_seq = 0` is not deduplicated away.
- Tests compare two processors fed the same frames and a snapshot takeover; the wire is version 6.


### Gaps found during verification (fixed)

1. **`--market-data-leader-only` was in U5 but not implemented.** Added (bare flag means true;
   `--market-data-leader-only=false` feeds every replica).
2. **U3's "every wire message" lacked a `STATUS` body test.** Golden test added.
3. **U4's "status JSON matches the counters" was untested.** The apply-level test now asserts the
   tap's `frames`/`dropped_frames` JSON.
4. **U5's integration test did not assert status lines or snapshot ordering.** It now requires a
   `status` message and checks that no trade/BBO/depth delta precedes the reset snapshot.
5. **Leadership handover could publish a delta before the reset snapshot, or lose a frame applied
   between capture and gate-open.** Fixed by capturing and enqueueing the snapshot under the
   state-machine lock, opening the gate only afterwards, and gating the tap at the source so
   follower frames never queue ahead of a takeover snapshot.

U1–U6 acceptance criteria are met. U7 archive recording/replay is implemented; the
snapshot-request stream and multi-symbol deployment remain. The header epoch is the current Raft
term and `leader_id` is the known leader (0 when none); the leader watcher updates both on every
metrics change, so reset snapshots and following frames carry the current leadership generation.
