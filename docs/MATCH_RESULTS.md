# Delivering match results to the order / clearing system

Status: **phases 1a–3 implemented.** `apply` turns results into [`MatchEvent`]s, returns the first
512 in the `/write` response, appends them to a bounded event window that is part of snapshot v9,
and serves that window through `POST /events` and `GET /events/status`. `client_order_id` makes
submissions idempotent across retries. `--match-event-log <path>` remains a node-local JSONL
debugging sink. Phase 4 (long-poll, gRPC streaming, MQ bridge) is the remaining proposal.

[`MatchEvent`]: ../src/match_result/mod.rs

Date: 2026-10-29 · Baseline: `master` at `7d34b20` (durable WAL +
snapshots, gRPC default transport, `OrderBook::place_order` fixed for issue #1).

---

## 1. What happens today

A `Place` request travels through Raft and is applied by every node:

| Step | Location | What is kept |
|---|---|---|
| Client submits the order | `POST /write` → `MatchEngineClient::write` | `Place { order, client_order_id }` |
| Entry is replicated | `log_store.rs` WAL | the full request, including the idempotency key |
| State machine applies it | `StateMachineStore::apply` | order book + bounded event window + submission map |
| Matcher runs | `OrderBook::execute_order` | returns fills, rests, cancels, triggers |
| Results become events | `MatchEvent::for_*` | `Fill`, `Rested`, `Cancelled`, `Triggered`, `Expired`, `Rejected`, `Amended` with `(log_index, ordinal)` ids |
| Events are recorded | `StateMachineStoreInner::record_events` | replicated, snapshotted ring (bounded by `--max-events`) |
| Response is built | `MatchEngineResponse::applied` | sequence + first 512 events |
| Response reaches the writer | `ClientWriteResponse.data` over `/write` | read-your-writes for the submitter |
| Consumers catch up | `POST /events` | cursor-based pages from the replicated window |

The event log, not the response, is the durable feed:

- clearing can read fills even if the submitting client dies;
- a consumer resumes from `(log_index, ordinal)` and dedupes on the same id;
- `client_order_id` collapses retries before they become a second order;
- a slow consumer cannot block `apply`: it pulls pages from the state machine.

The file sink is now only an observability option (`--match-event-log`); it is node-local and not a
clearing feed. Followers still apply every entry and build the same event window, even though only
the writer receives the immediate response.

---

## 2. Requirements for a "proper" solution

| # | Requirement | Why |
|---|---|---|
| R1 | **Durable** | A committed fill must survive the writer, the leader and a full restart |
| R2 | **Deterministic** | Every replica must derive byte-identical state from the same log; a sink may only observe, never influence, what is applied |
| R3 | **Ordered** | Clearing needs one total order per instrument; Raft log order is the natural one |
| R4 | **Exactly-once observable** | At-least-once delivery plus a stable dedupe key; consumers must be able to resume |
| R5 | **Identified** | Every event needs a monotonic id and a link to the order and to the Raft log position |
| R6 | **Bounded** | Retention cannot grow the snapshot without limit; lagging consumers need an explicit gap signal |
| R7 | **Non-blocking** | A slow consumer must never stall consensus or `apply` |
| R8 | **Read-your-writes** | The submitting client should see its own fills without a second round trip |
| R9 | **Idempotent submission** | A retried `Place` must not create a second order (otherwise R4 is unachievable end to end) |
| R10 | **Compatible** | Existing `/write` and `/read` callers must keep working |

R2 is the constraint that eliminates the obvious "publish to Kafka in `apply`" design: `apply` is
replayed on restart and on every follower, so any network side effect there would be duplicated and
could block the state machine.

---

## 3. Options

### A. Return matches in the write response only

Change `_mr` to `mr`, put the fills into `MatchEngineResponse`, expose them via
`ClientWriteResponse.data`. (The event extraction part of this is now done — `MatchEvent::for_place`
and the sink — but the response still carries only the sequence.)

- **Pros:** tiny change, no new storage, no new API, response is free because every replica computes
  the fills anyway.
- **Cons:** only the writer sees them; lost if the writer dies; no catch-up for clearing; a large
  sweep can produce a huge response.
- **Verdict:** necessary but not sufficient. It should be phase 1 of any design.

### B. Replicated, bounded event log inside the state machine (recommended)

`apply` appends the events it produces to a ring buffer that lives in the state machine, is included
in snapshots, and is served by a cursor-based read API. The write response also returns the events
for the submitting client.

- **Pros:** durable, deterministic, ordered by construction, survives leadership changes; consumers
  resume with a cursor; no side effect during apply; retention is explicit.
- **Cons:** events are replicated and snapshotted, so retention costs memory and snapshot size; a
  consumer that falls off the retained window needs a resync procedure.
- **Verdict:** the right default for this repo.

### C. Publish to a message queue from `apply`

- **Cons:** violates R2. `apply` runs on every replica and is replayed after restart, so messages are
  duplicated per node and per replay; a slow broker blocks consensus; ordering across leaders is not
  guaranteed.
- **Verdict:** do not do this. Any MQ bridge must be a consumer of B, not a hook in `apply`.

### D. Tail the Raft log externally and re-run the matcher

An external process reads `log/*.wal` and re-executes the matching logic.

- **Pros:** no state-machine changes.
- **Cons:** duplicates the matcher and the order book, so any divergence (issue #1 is a good example)
  silently corrupts clearing; matcher version changes break replay; needs access to snapshot state
  anyway.
- **Verdict:** only as an audit tool, not as the primary feed.

### E. Separate Raft group for results

- **Cons:** two consensus groups to order against each other; cross-group ordering is unsolvable
  without extra coordination.
- **Verdict:** no.

### Comparison

| | Durable | Deterministic | Catch-up | Complexity | Delivery |
|---|---|---|---|---|---|
| A. Response only | no | yes | no | very low | once, to writer |
| **B. Replicated event log** | **yes** | **yes** | **yes** | medium | at-least-once + dedupe |
| C. MQ in apply | no | **no** | no | low | duplicated |
| D. External replay | yes | fragile | yes | high | at-least-once |
| E. Second Raft group | yes | yes | yes | very high | at-least-once |

Recommendation: **A + B**. Return events immediately and keep them in a bounded replicated log.

---

## 4. Recommended design

### 4.1 Pluggable processing (implemented)

`StateMachineStore::apply` computes events but delegates what happens to them to a sink chosen at
startup, so an implementation can be replaced without touching the matching engine or the state
machine:

```rust
pub trait MatchResultSink: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn publish(&self, events: &[MatchEvent]) -> io::Result<()>;
}
```

Built-in implementations (`src/match_result/mod.rs`):

| Sink | Behaviour | Use |
|---|---|---|
| `NoopSink` | Drops events | Default; identical to the old behaviour |
| `FileLogSink` | Appends one JSON object per event to a file | Tests and debugging (`--match-event-log <path>`) |
| `MemorySink` | Keeps events in a `Vec` | Unit tests that assert what `apply` produced |

Wiring:

* `StateMachineStore::open(dir)` installs `NoopSink`; `open_with_sinks(dir, sink, tap)` or
  `open_with_sinks_and_retention(..., max_events)` installs any other sink and the retention limit.
  Clones of the store share the sink.
* `build_match_engine_node` opens a `FileLogSink` when `StorageConfig::match_event_log` is set
  (`--match-event-log`, env `MATCH_EVENT_LOG`), stamping the node id on every line.
* A sink failure is logged at `warn` and otherwise ignored: applying an entry must never fail (or
  diverge) because a sink is unreachable. The state machine is unchanged by sink selection.

The file sink is deliberately **not** a clearing feed: it is node-local, not replicated, not
fsynced, and replay after a restart writes the replayed events again. The durable feed is the
replicated event log described below; the sink trait remains the extension point for local
observability (or an in-process consumer) without changing `apply`.

### 4.2 Event model (implemented)

Raft orders the input, so the state machine can assign output ids deterministically. Identity is the
applied log position plus an ordinal, which needs no extra state in snapshots and is identical on
every replica:

```rust
// src/match_result/mod.rs
pub struct EventId {
    pub log_index: u64,   // Raft log index of the entry that produced the event
    pub ordinal: u32,     // 0-based position inside that entry
}

// JSON: "rested", "fill", "cancelled", "triggered", "expired", "rejected", "amended"
pub enum MatchEventKind { Rested, Fill, Cancelled, Triggered, Expired, Rejected, Amended }

pub struct MatchEvent {
    pub event_id: EventId,
    pub kind: MatchEventKind,
    /// Sequence of the order this event is about.
    pub order_sequence: u64,
    /// Side of that order (the maker's side is the opposite on a fill).
    pub side: OrderSide,
    /// The resting order's sequence on a fill.
    pub counterparty_sequence: Option<u64>,
    /// On an amendment: the sequence the order had before it.
    pub previous_sequence: Option<u64>,
    /// Maker price on a fill, order price otherwise.
    pub price: Fixed,
    /// Filled volume, resting remainder, the open volume a cancel/expiry
    /// removed, the stop's open volume on a trigger, the submitted volume on a
    /// rejection, or the new open volume on an amendment.
    pub volume: Fixed,
    /// Taker fee on a fill; zero for non-fill events and fee overflow.
    pub taker_fee: Fixed,
    /// Maker fee on a fill; zero for non-fill events and fee overflow.
    pub maker_fee: Fixed,
    /// True when the fill's notional or fee overflowed; fee fields are zero.
    pub fee_overflow: bool,
    /// Why a `Rejected` (or `Expired`) event happened; omitted when absent.
    pub reason: Option<RejectReason>,
}
```

Notes:

- `(log_index, ordinal)` is the dedupe key and the cursor a consumer would resume from. It is stable
  across restarts and snapshot installs, and needs no counter in `StateMachineData`.
- Fills keep their matching order; the `Rested` event, when the taker leaves a remainder, is last in
  the entry.
- `Order` already carries `id: u64` (client-supplied) and `sequence` (assigned by the book). Events
  carry sequences because that is what `cancel()` keys on; do not rely on `id` for identity.
- For a `Fill`, `price` is the maker's price and `volume` is `MatchResult.matches`. Both are
  `Fixed` scale-18 values and serialize as decimal strings. Public price improvement stays
  derivable from the taker order.
- `Cancelled` carries the volume that was still open in the book, not the volume originally
  submitted.
- Fill events carry exact `taker_fee`/`maker_fee`. The taker uses its taker rate, the resting
  maker uses its maker rate; positive = charge, negative = rebate. `fee_overflow` marks a notional
  or fee outside the scale-18 range; the fee amounts are then zero and must not be used.

Multi-instrument isolation is deployment-level: each symbol gets its own process/cluster and
event sequence, so no per-instrument cursor split is needed inside one engine.

### 4.3 Producing events (implemented)

`StateMachineStore::apply` builds the events, records them in the replicated window, and publishes
them to the sink:

```rust
MatchEngineRequest::Place { order, client_order_id } => {
    // Duplicate client id: return the original sequence and retained events.
    if let Some(record) = client_order_id.and_then(|id| inner.submissions.get(id)) {
        return inner.duplicate_response(record);
    }

    let mut o = *order;
    let outcome = inner.orderbook.execute_order(&mut o);
    let events = /* fills, rests, cancels, triggers, expiries, rejections */;
    inner.record_events(&events);
    inner.record_submission(client_order_id, o.sequence, log_index, events.len());
    self.publish_match_events(&events);

    MatchEngineResponse::applied(o.sequence, &events)
}
```

The real arm is slightly longer because it also anchors automatic-purge events and the
market-data frame; the window and submission map are updated before the sink call, so a sink
failure cannot lose them. `Cancel`, `Amend` and `PurgeExpired` use the same
`record_events`/`MatchEngineResponse::applied` path.

`Cancel` publishes a `Cancelled` event only when `cancel()` actually removed an order; `Blank`
and membership entries publish nothing. `OrderBook::resting_volume` exists precisely so the
`Rested` event reports the volume stored in the book instead of `submitted - filled`, which can
differ in the last floating-point bit.

### 4.4 Immediate response (implemented)

`MatchEngineResponse` is additive; all new fields default when absent, so old payloads still
deserialize:

```rust
pub struct MatchEngineResponse {
    pub value: Option<String>,        // existing: sequence string
    pub sequence: Option<u64>,        // explicit taker sequence
    pub events: Vec<MatchEvent>,      // first 512 events of the entry
    pub events_truncated: bool,       // true when the entry produced more
}
```

Implemented response shape:

```json
{"Ok": {
  "log_id": {"leader_id": {"term": 3, "node_id": 1}, "index": 42},
  "data": {
    "value": "7",
    "sequence": 7,
    "events": [
      {"event_id": {"log_index": 42, "ordinal": 0}, "kind": "fill",
       "order_sequence": 7, "counterparty_sequence": 3,
       "price": "100", "volume": "10",
       "taker_fee": "0.4", "maker_fee": "0.1", "fee_overflow": false}
    ],
    "events_truncated": false
  },
  "membership": null
}}
```

The cap is `MatchEngineResponse::MAX_EVENTS = 512`. A taker sweeping the whole book can produce
thousands of fills; the rest are still in the event log and the response sets
`events_truncated: true`. This keeps `/write` bodies and HTTP limits bounded.

### 4.5 Durable, bounded log (implemented)

Additions to `StateMachineData` (which is what `to_content()`/snapshots serialise):

```rust
pub struct StateMachineData {
    pub orders: Vec<Order>,
    pub sequence: u64,
    pub events: Vec<StoredMatchEvent>, // retained window, ordered by event_id
    pub events_evicted: u64,
    pub submissions: Vec<SubmissionRecord>, // client_order_id -> sequence/log index
    pub submissions_evicted: u64,
}
```

- No event counter is needed: `EventId` is the log position plus an ordinal, so the window needs no
  extra state to reconstruct and restart/learner replay assigns the same ids.
- Retention is `--max-events` / `MAX_EVENTS`, default **262 144 events**. The client-order-id map
  has its own fixed `MAX_SUBMISSIONS` cap (262 144): dedupe affects matching, so it must not depend
  on a node-local event-retention flag. `StoredMatchEvent` mirrors `MatchEvent` without
  `skip_serializing_if`, because postcard is positional and cannot decode a skipped optional field.
- `oldest_event_id()` is `events.front().map(|e| e.event_id)`; `last_event_id()` is the back.
- Eviction only trims the window; ids never repeat.
- The event window is part of the snapshot, so restart and learner install recover it exactly. A new
  learner receives only the retained window — that is expected, and the read API advertises
  `oldest_event_id` so consumers can detect the gap.

Snapshot format: `SNAPSHOT_VERSION` is **9**; v9 adds base/quote asset metadata, tick/lot rules,
volume units, fee assets and the fee policy, v8 adds maker/taker fee rates to orders and fee fields
to match events, v7 adds the single-symbol precision configuration, v6 stores the last trade price
and match events as fixed-point values, v5 stored order prices/quantities as fixed-point integers,
and v4 added the event window and submission map on top of the v3 order/stop fields. Postcard encodes struct fields positionally, so an older decoder
fails on the missing fields (`postcard::from_bytes` reports an unexpected end of input). Two options:

1. **Reject v2 with a clear error** (simplest; acceptable for a demo, but requires wiping data).
2. Decode a versioned enum (`V2(StateMachineDataV2) | V3(StateMachineDataV3)`) and migrate.

The implemented choice is to reject older files with an explicit message. This is nearly free:
`read_snapshot_file` checks the version in the header and fails with
`unsupported snapshot version {version}` before decoding, so a v3 file on disk is rejected loudly
rather than silently misparsed.

### 4.6 Read API and consumer protocol (implemented)

```
POST /events
{"after": {"log_index": 100, "ordinal": 3}, "limit": 500}

→ 200
{"events": [ ... ],
 "next_after": {"log_index": 140, "ordinal": 1},
 "oldest_event_id": {"log_index": 1, "ordinal": 0},
 "last_event_id": {"log_index": 140, "ordinal": 1},
 "has_more": true}
```

- Omitting `after` means "from the oldest retained event".
- Cursors compare by `EventId`'s derived ordering: `(log_index, ordinal)`.
- `limit` must be between 1 and 1000; out-of-range values are rejected with `400` rather than
  silently clamped, so one call cannot allocate unbounded memory.
- `/events` performs a linearizable read via a read linearizer; the client forwards to the leader
  on `ForwardToLeader`. `/events/status` is a local read for dashboards.
- Implemented in `src/network/api.rs` with `EventsRequest`/`EventsResponse`/`EventsStatus`; the
  client exposes `events()` and `events_status()`.
- Phase 4: long-poll `wait_ms` (≤ 1000) and gRPC `StreamEvents` using the same cursor.

Consumer contract (this is what makes R4 achievable):

1. Process a batch idempotently, keyed on `event_id` (`INSERT ... ON CONFLICT DO NOTHING` or a
   dedupe table).
2. Persist the cursor **in the same transaction** as the side effects.
3. On restart, resume from the cursor.
4. If `cursor < oldest_event_id`, events were evicted: stop and resync out of band. There is no
   way to reconstruct evicted trades from the book, so the consumer must load an operator-provided
   clearing snapshot at some `event_id >= oldest_event_id` and replay from there. The server's job
   is only to make the gap loud, not to invent history.
5. Duplicates are normal (at-least-once); dedupe handles them.

An MQ bridge is just another consumer: a small sidecar reads `/events`, publishes to Kafka/NATS keyed
by `event_id`, and commits its cursor after the broker acknowledges. Crucially, this runs outside
`apply`.

### 4.7 Idempotent submission (implemented)

Returning events does not by itself make retries safe. Two independent dedupe layers are needed:

- **Submission:** `MatchEngineRequest::Place` carries an optional `client_order_id`. The state
  machine keeps a bounded map `client_order_id -> SubmissionRecord { order_sequence, log_index,
  event_count }`; a replay returns the original sequence and the retained events for that log
  index instead of placing a second order. The map is snapshotted and bounded by the fixed
  `MAX_SUBMISSIONS` constant (independent of `--max-events`, because dedupe affects matching).
  Keeping the key on the request (not `Order`) preserves `Order: Copy` in the matching loop.
  The edge validates the key before proposing: 1–128 bytes from `[A-Za-z0-9._:-]`, with empty
  strings rejected (omit the field to disable idempotency).
- **Consumption:** dedupe on `event_id` as above.

Without the first layer, an HTTP timeout after commit produces two orders that genuinely trade
twice; clearing dedupe cannot fix that because both are real fills.

### 4.8 Time and ordering

Do **not** call `SystemTime::now()` inside `apply`: it differs between replicas and breaks R2.
Ordering is Raft log order, which is all clearing needs for FIFO processing. If a human-readable
timestamp is required, either:

- let the ingress gateway stamp the request (`client_ts_ms`) and carry it through the log entry, or
- record the leader's apply time outside the state machine (observability only, never used for
  ordering or idempotency).

### 4.9 Observability

Add `GET /events/status`:

```json
{"oldest_event_id": {"log_index": 1, "ordinal": 0},
 "last_event_id": {"log_index": 140, "ordinal": 1},
 "retained_events": 1499, "evictions_total": 0, "retention_limit": 262144,
 "submission_retention_limit": 262144, "submission_evictions_total": 0}
```

Alert on evictions: they mean a consumer is falling behind and a resync is imminent. Also log the
event rate per apply batch at debug level, consistent with the existing `tracing` usage.

---

## 5. Failure modes

| Failure | Effect | Handling |
|---|---|---|
| Sink returns an error | That batch is missing from the sink | Logged at `warn`; `apply` and the state machine are unaffected. The file sink keeps the lines it already wrote |
| Node restarts | Replayed entries are applied again | The event window is rebuilt identically from the snapshot + log; the debug file sink may duplicate lines, the durable feed does not |
| Writer crashes after commit | Response lost | Fills are in the event log; clearing pulls them by cursor |
| Leader steps down before replying | Caller gets `ForwardToLeader` | Retry with the same `client_order_id`; submission dedupe returns the original sequence |
| Follower restarts | — | Snapshot + tail replay rebuilds the identical event log |
| Learner installs a snapshot | It holds only the retained window | API reports `oldest_event_id`; consumers that need older data must already have it |
| Retention overflow | Events evicted | `evictions_total` increments; consumer sees `cursor < oldest_event_id` and resyncs |
| Duplicate delivery | Consumer sees an event twice | Dedupe on `event_id` |
| Huge sweep (many fills) | Large response | Response capped with `events_truncated`; full set available from `/events` |
| Slow consumer | — | Pull-based, so it cannot block `apply`; only its own cursor lags |
| Snapshot from an older binary | v8 or earlier payload | Version check rejects it with a clear error; v9 is the current asset-aware snapshot |

---

## 6. API compatibility

- `MatchEngineResponse` keeps `value` (the sequence string for `Place`) so existing callers and
  scripts keep working. New fields are additive and serde ignores unknown fields on the
  consumer side.
- `MatchResult` itself stays as-is for the matcher; `MatchEvent` is the wire/storage type. This keeps
  the matching code independent of the clearing schema.
- `/events` is new; nothing is renamed. If a breaking v2 is ever needed, version the route rather
  than changing `/write`.
- `MatchEngineRequest::Place` gains `client_order_id: Option<String>` with `#[serde(default)]`, so
  old JSON payloads still deserialize. It lives on the request rather than `Order` to keep
  `Order: Copy` in the matching loop.
- Postcard is positional, so older Raft log entries (without the field) and v3 snapshots are not
  decodable by this build. The example deployment must start from a fresh data directory; there is
  no on-disk migration.

---

## 7. Test plan

Implemented today (`cargo test --lib`, `cargo test --test cluster test_match_results`, `test_events`):

- `apply` publishes `Rested`, `Fill` and `Cancelled` events to the configured sink, with the right
  order/counterparty sequences and the book's exact volumes.
- The order-type paths publish their own kinds: `Triggered` (a parked stop was elected; its child's
  fills follow in the same entry, then a `Cancelled` for any remainder), `Expired` (an automatic
  GTD purge at a mutating request's intake time, or an explicit `PurgeExpired`), `Amended` (a
  resting order changed price/volume; the event carries both the previous and the new sequence) and
  `Rejected` (FOK unfillable, post-only crossing, stop already crossed, amend of an
  unknown/unsupported order, invalid shape). Expired events come first in an entry, and ordinals
  keep increasing across the whole entry, so a stop cascade stays one ordered batch.
- A cancel of an order that is not in the book publishes nothing.
- A failing sink is logged and does not fail `apply` or change the state.
- `FileLogSink` writes one JSON object per event with `node_id`, appends across reopen, and writes
  nothing for an empty batch.
- `resting_volume` reports the stored remainder, `None` for a fully filled or unplaced order.
- End-to-end (`tests/cluster/test_match_results.rs`): a node started with `--match-event-log`
  records rest/fill/cancel in log order and the `/write` response is unchanged; a second node
  checks `Triggered` + child fill, `Expired` from `PurgeExpired`, and the new `/read` keys.

Implemented with phases 2–3:

- Event ids are monotonic and gap-free within the retained window; retention pushes the oldest
  ids out and `/events/status` reports the eviction count.
- A partial fill emits exactly one `Fill` plus one `Rested`; an exact fill emits `Fill` only.
- Submission dedupe: the same `client_order_id` twice returns one order, the same sequence and the
  retained events; the map survives a snapshot/reopen.
- `POST /write Place` returns the same events as `POST /events` for that range, and the cursor
  pages both events exactly once (`tests/cluster/test_events.rs`).
- Diffing two processors fed the same frames covers the market-data side; the match-event side is
  deterministic by construction (`EventId` from the applied log, no clock).

Still to write (mainly phase 4 / operational tests):

- Restart a full node after a write; `/events` returns the fills and new ids continue after the
  replayed ones (snapshot persistence is unit-tested).
- Snapshot a node, purge the log, add a learner; the learner serves the retained events and reports
  the correct `oldest_event_id`.
- Kill the submitting client after commit; a second client reads the fills from `/events`.
- Two consumers with independent cursors both receive every event exactly once after dedupe.
- Property test: replaying a random request stream twice yields identical event id/field sequences.

---

## 8. Phases and effort

| Phase | Work | Effort |
|---|---|---|
| 1a ✅ | `MatchEvent` + `EventId`; build events in `apply`; `MatchResultSink` with `Noop`/`Memory`/`File` sinks; `--match-event-log`; unit and end-to-end tests | done |
| 1b ✅ | Events in `MatchEngineResponse`; 512-event cap with `events_truncated` | done |
| 2 ✅ | Bounded event window + snapshot v4–v9 + `oldest_event_id`/`last_event_id`; `/events` and `/events/status`; restart/snapshot/retention tests | done |
| 3 ✅ | `client_order_id` submission dedupe with bounded map | done |
| 4 | Optional: long-poll, gRPC `StreamEvents`, MQ bridge sidecar | 2–3 days |

Phases 1a–3 are implemented; phase 4 is operational convenience.

---

## 9. Decision (implemented)

Adopted **A + B**:

1. Return events from `/write` so the submitter has read-your-writes (phase 1b).
2. Keep a bounded, replicated event log in the state machine, snapshotted with the state, served by
   a cursor API (phase 2).
3. Keep network delivery out of the replicated path: `apply` records events in the state machine
   and publishes to a `MatchResultSink` (best-effort; a sink can never fail or change the apply).
   Production delivery consumes the durable log rather than doing network IO in `apply`.
4. Make submission idempotent with `client_order_id` so retries do not double-trade.

The sink trait remains the seam for local debug/observability; the replicated event log is the
durable feed. An MQ bridge or sidecar consumes `/events` without touching `OrderBook` or `apply`.

This satisfies R1–R10 with no new consensus machinery and no new failure domain. The Raft log entry
format gains the optional `client_order_id`; the cost is a bounded amount of extra state per snapshot
and two read endpoints (`/events`, `/events/status`).

---

## 10. Open questions

1. **Retention default** — 262k events is now the `--max-events` default; measure the real postcard
   footprint on the target deployment and tune it.
2. **Scope of v1 events** — answered for now: `Rested`, `Fill`, `Cancelled`, `Triggered`, `Expired`,
   `Rejected` and `Amended` are all emitted. Revisit if clearing only wants fills and the extra
   volume matters.
3. **Consumer placement** — in-cluster sidecar pulling `/events`, or an external service? Affects
   whether long-poll/streaming is worth building.
4. **Timestamp** — resolved for market data: the receiving leader stamps `Order.timestamp` at
   intake (client values are overwritten), so it is log-carried and trustworthy. Clearing still
   needs to decide whether it wants that intake time in the event feed or log order alone.
5. **Cancel identity** — `cancel()` currently keys on `(price, sequence)`, which clearing has from
   the events, but humans think in `order id`; consider an order-id index as a follow-up.
6. **Multi-instrument** — one process/cluster per symbol; event ids are per deployment, not per
   book inside one process.
7. **Snapshot migration** — v9 rejects older snapshots. Add a versioned decoder only if a
   deployment has data worth carrying across a future format change.
