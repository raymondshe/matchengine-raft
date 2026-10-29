# Market-data egress — implementation plan

Status: **implemented for U1–U6 and U8; U7 deferred.** Design rationale lives in
[`AERON_SOLUTION.md`](./AERON_SOLUTION.md) Part II (§11–26). This file turns that design into
ordered, testable units of work.

Scope of this implementation: the deterministic **feed pipeline** — tap, depth journal, shadow
book, message model, deterministic publisher, JSONL sink, leader gating, snapshots, `/read`
status and an end-to-end test. The **Aeron transport** is deliberately the next phase: it is an adapter over
the message bytes this plan produces, and adding a media driver is a separate operational decision.

## Decisions already fixed (from AERON_SOLUTION Part II)

- Timestamps: the receiving leader stamps `Order.timestamp` at intake (overwriting client values)
  and every market-data record uses it. Never a publish-time clock.
- Identity and dedupe: `(symbol_id, stream_id, market_seq, sub_seq)`, anchored on the replicated
  order/mutation sequence (the taker order sequence for a `Place`) with a deterministic per-entry
  `sub_seq`. `(log_index, ordinal)` stays the audit key. Implemented in U8.
- Matching is untouched: the tap only records values each book operation already has; all
  derivation, encoding and IO run on a separate thread.
- Delivery: leader-only by default; snapshot + deltas; BBO and depth totals per applied entry; one
  uncapped `TRADE` per taker; `GAP` only on a hard failure. Trades are never sampled.
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

- **Config**: log path, depth levels, `status_every` (default 100), lifecycle record cap, bar
  intervals (default `1s`, `1m`), queue capacity, leader-only flag.
- **Message model**: `TRADE` (one per taker, all fills), `BBO`, `DEPTH_SNAPSHOT`, `DEPTH_DELTA`
  (post-entry totals), `LIFECYCLE` (one per entry), `STATUS`, `GAP`, `BAR` (closed + partial) — the
  catalogue in §12/§16.
- **Wire encoder** (`wire.rs`): 64-byte header (magic `AMKT`, version 2, type, flags, `market_seq`,
  log position, epoch, leader, ts, symbol, `sub_seq`) plus bodies, little-endian, matching §16.
- **Shadow book**: per-side price levels keyed by positive `f64` bits; applies journal deltas,
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
- **Wire**: `market_seq` replaces `stream_seq`, `sub_seq` takes two reserved bytes, `version = 2`.
  `STATUS` is deterministic every `status_every` sequences; `GAP` is diagnostic.
- **Acceptance**: two processors fed the same frames emit identical messages; a takeover snapshot
  continues the same keys; one TRADE carries every fill; no `Instant` in the data path; wire golden
  tests and integration assertions cover the keys.
- **Status: done.**

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
| 2001 | `TRADE` | one per taker, all fills, never sampled |
| 2002 | `BBO` | one per book-changing entry |
| 2003 | `DEPTH_SNAPSHOT` | takeover/startup reset + recovery path |
| 2004 | `DEPTH_DELTA` | post-entry totals per touched level |
| 2005 | `LIFECYCLE` | one per entry; `rested` shed first above the cap |
| 2006 | `STATUS` | every `status_every` (default 100) sequences; gap flags |
| 2007 | `BAR` | trade OHLCV, closed + live (`partial`) |

Header (64 B): magic `AMKT`, version `2`, type, flags (`SNAPSHOT|RESET|GAP|END_OF_BATCH|
PARTIAL_BAR|BAR_CLOSE`; bit 4 reserved), `market_seq`, `log_index`, `ordinal`, body length, epoch,
leader id, `ts_ms`, `symbol_id`, `sub_seq`, reserved.

## Test matrix

| Layer | Tests |
|---|---|
| Book | journal per mutation; journal off costs nothing; amend deltas |
| Events | side on every kind; amended/fill JSON fields |
| Wire | each message header + body encoding, flags, lengths |
| Processor | shadow/BBO, per-entry depth totals, bar windows (closed/live), lifecycle shedding, gap, snapshot reset, identical keys across replicas |
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
| U3 | **done** | `src/market_data/{mod,wire}.rs`: shadow book, processor, sinks, tap, deterministic message keys, 20 module unit tests |
| U4 | **done** | `open_with_sinks`, tap frames, `capture_market_data_snapshot`, `marketdata_status`, status assertion |
| U5 | **done** | `MarketDataConfig` on `StorageConfig`, all CLI flags (incl. `--market-data-leader-only`), leader watcher, source-gated tap, end-to-end test on 21303 |
| U6 | **done** | README/README_CN/AGENTS/TODO/AERON updates; bench `rest 1k bids + journal` |
| U8 | **done** | deterministic `(market_seq, sub_seq)` across replicas; all mutations sequenced; auto GTD purge; one TRADE per taker; wire v2 |
| U7 | deferred | Aeron adapter, Archive, multi-symbol engine |

## Verification (2026-10-28)

| Check | Result |
|---|---|
| `cargo test` | **90 unit + 22 integration** tests pass |
| `cargo check --all-targets` | **0 warnings** |
| `cargo test --test cluster test_market_data` | passed; checks `market_seq` anchors for place/amend/cancel and the uncapped trade message |
| U8 determinism | two processors fed identical frames emit identical messages; a takeover snapshot continues the same keys; wire v2 golden tests |
| automatic purge | `Place`/`Cancel`/`Amend` purge due GTD at intake time; `Day` only via `PurgeExpired`; guarded by the earliest-expiry index (no scan without GTD) |
| `matchengine-raft --help` | all feed flags present, including `--market-data-status-every` and `--market-data-leader-only` |
| Snapshot/log formats | U8 itself kept `SNAPSHOT_VERSION` 3; the later match-event work bumps it to 4 (event window + dedupe map). `market_seq` lives in the existing `OrderBook::sequence` |
| Dependencies | none added |

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
- Tests compare two processors fed the same frames and a snapshot takeover; the wire is version 2.


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

U1–U6 acceptance criteria are met. U7 (Aeron adapter, Archive replay, multi-symbol engine) remains
deferred; `epoch`/`leader_id` are still 0 on the wire until the adapter epoch is wired.
