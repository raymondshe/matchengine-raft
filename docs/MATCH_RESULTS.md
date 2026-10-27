# Delivering match results to the order / clearing system

Status: **partly implemented.** `apply` no longer throws match results away: it turns them into
[`MatchEvent`]s and publishes them to a pluggable `MatchResultSink`
(`src/match_result/mod.rs`). The default sink is a no-op; `--match-event-log <path>` installs a
file sink that appends JSON lines for tests and debugging. Everything needed for durable,
replayable delivery to clearing (a replicated event log, a cursor API, idempotent submission) is
still a design proposal, described below.

[`MatchEvent`]: ../src/match_result/mod.rs

Date: 2026-10-27 · Baseline: `master` at `70a9368` (`a44d9ae` plus README typo fixes: durable WAL +
snapshots, gRPC default transport, `OrderBook::place_order` fixed for issue #1).

---

## 1. What happens today

A `Place` request travels through Raft and is applied by every node:

| Step | Location | What is kept |
|---|---|---|
| Client submits the order | `POST /write` → `MatchEngineClient::write` | — |
| Entry is replicated | `log_store.rs` WAL | `MatchEngineRequest::Place { order }` |
| State machine applies it | `StateMachineStore::apply` | — |
| Matcher runs | `OrderBook::place_order` → `match_bid_order` / `match_ask_order` | returns `Vec<MatchResult>` |
| Results become events | `MatchEvent::for_place` / `for_cancel` | `Fill`, `Rested`, `Cancelled` with `(log_index, ordinal)` ids |
| Events reach the sink | `MatchResultSink::publish` | dropped by default (`NoopSink`); JSONL with `FileLogSink` |
| Response is built | `MatchEngineResponse { value: Some(sequence) }` | the taker's sequence number |
| Response reaches the writer | `ClientWriteResponse.data` over `/write` | only the writer sees it |

With the default sink the only trace of a trade is still:

1. the mutated order book (resting orders now have smaller volumes or are gone), and
2. a sequence number string returned to whoever submitted the order.

The file sink adds a node-local JSONL trail, but it is not replicated, every replica writes its own
copy, and replaying the log after a restart writes the replayed events again. It is a debugging
aid, not a clearing feed.

Followers apply the same entries but their responders are `None`, so they silently drop the response
too (`core/sm/worker.rs` attaches a responder only to entries someone is waiting on).

Consequences:

- **Clearing never learns about fills.** It sees an order book after the fact but not who traded with
  whom, at what price, or in what order.
- **The submitter is a single point of loss.** If the client dies after the write commits, the fills
  are gone from the system's point of view.
- **A sequence number is not a fill identity.** It identifies the taker, not a trade, and cannot be
  used to deduplicate or resume a stream.
- **Retries are indistinguishable.** `/write` can time out after commit; resubmitting creates a
  second order and a second set of fills, with no idempotency key to collapse them.
- **No replay.** There is no way for an auditor or a down consumer to catch up.

The matcher data itself is already good enough to build this on: `MatchResult` carries the taker,
the maker and the matched volume, and `apply` runs on every replica, so the fills are computed
deterministically everywhere. Since phase 1a they are turned into `MatchEvent`s and handed to the
node's sink, but the default sink drops them, so clearing still has no feed.

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

* `StateMachineStore::open(dir)` installs `NoopSink`; `open_with_sink(dir, Arc<dyn MatchResultSink>)`
  installs any other sink. Clones of the store share the sink.
* `build_match_engine_node` opens a `FileLogSink` when `StorageConfig::match_event_log` is set
  (`--match-event-log`, env `MATCH_EVENT_LOG`), stamping the node id on every line.
* A sink failure is logged at `warn` and otherwise ignored: applying an entry must never fail (or
  diverge) because a sink is unreachable. The state machine is unchanged by sink selection.

The file sink is deliberately **not** a clearing feed: it is node-local, not replicated, not
fsynced, and replay after a restart writes the replayed events again. It exists so the events can
be inspected while the durable design below is built. The trait is the extension point for that
design: a replicated event log, an MQ bridge or a sidecar can each be a sink (or consume the log)
without changing `apply`.

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

pub enum MatchEventKind { Rested, Fill, Cancelled }  // JSON: "rested", "fill", "cancelled"

pub struct MatchEvent {
    pub event_id: EventId,
    pub kind: MatchEventKind,
    /// Sequence of the order this event is about.
    pub order_sequence: u64,
    /// The resting order's sequence on a fill.
    pub counterparty_sequence: Option<u64>,
    /// Maker price on a fill, order price otherwise.
    pub price: f64,
    /// Filled volume, resting remainder, or the open volume a cancel removed.
    pub volume: f64,
}
```

Notes:

- `(log_index, ordinal)` is the dedupe key and the cursor a consumer would resume from. It is stable
  across restarts and snapshot installs, and needs no counter in `StateMachineData`.
- Fills keep their matching order; the `Rested` event, when the taker leaves a remainder, is last in
  the entry.
- `Order` already carries `id: u64` (client-supplied) and `sequence` (assigned by the book). Events
  carry sequences because that is what `cancel()` keys on; do not rely on `id` for identity.
- For a `Fill`, `price` is the maker's price and `volume` is `MatchResult.matches`. Public price
  improvement stays derivable from the taker order.
- `Cancelled` carries the volume that was still open in the book, not the volume originally
  submitted.

If multi-instrument support arrives later, add an `instrument: String` field. Whether ids stay
global or become per-instrument is then a storage decision, not a sink API change.

### 4.3 Producing events (implemented)

`StateMachineStore::apply` builds the events and publishes them:

```rust
MatchEngineRequest::Place { order } => {
    let mut o = *order;
    let matches = inner.orderbook.place_order(&mut o);
    // `place_order` only writes the sequence back, so read the remainder from the book.
    let resting_volume = inner.orderbook.resting_volume(&o);
    let events = MatchEvent::for_place(entry.log_id.index, &o, &matches, resting_volume);
    self.publish_match_events(&events);

    MatchEngineResponse { value: Some(o.sequence.to_string()) }
}
```

`Cancel` publishes a `Cancelled` event only when `cancel()` actually removed an order; `Set`,
`Blank` and membership entries publish nothing. `OrderBook::resting_volume` exists precisely so the
`Rested` event reports the volume stored in the book instead of `submitted - filled`, which can
differ in the last floating-point bit.

All of this is pure state-machine work up to the sink call; the sink is the only side effect.

### 4.4 Immediate response

`MatchEngineResponse` grows additively:

```rust
pub struct MatchEngineResponse {
    pub value: Option<String>,        // existing: sequence or Set value
    pub sequence: Option<u64>,        // explicit taker sequence
    pub events: Vec<MatchEvent>,      // new
    pub events_truncated: bool,       // new: response holds only the first N events
}
```

Proposed response shape (not implemented yet — today `/write` still returns only `value`):

```json
{"Ok": {
  "log_id": {"leader_id": {"term": 3, "node_id": 1}, "index": 42},
  "data": {
    "value": "7",
    "sequence": 7,
    "events": [
      {"event_id": {"log_index": 42, "ordinal": 0}, "kind": "fill",
       "order_sequence": 7, "counterparty_sequence": 3,
       "price": 100.0, "volume": 10.0}
    ],
    "events_truncated": false
  },
  "membership": null
}}
```

Cap the number of events embedded in a response (e.g. 512). A taker sweeping the whole book can
produce thousands of fills; the rest are still in the event log, and the response can set
`events_truncated: true`. This keeps `/write` bodies and HTTP limits bounded.

### 4.5 Durable, bounded log

Additions to `StateMachineData` (which is what `to_content()`/snapshots serialise):

```rust
pub struct StateMachineData {
    pub data: BTreeMap<String, String>,
    pub orders: Vec<Order>,
    pub sequence: u64,
    pub events: VecDeque<MatchEvent>, // retained window, ordered by event_id
    pub evictions_total: u64,
}
```

- No event counter is needed: `EventId` is the log position plus an ordinal, so the window needs no
  extra state to reconstruct and restart/learner replay assigns the same ids.
- Retention is a constant or a `--max-events`/env setting; default suggestion: **262 144 events**
  (postcard-encoded at roughly 50–70 B/event, so on the order of 10–20 MB per node — measure the real
  size before committing to a number).
- `oldest_event_id()` is `events.front().map(|e| e.event_id)`; `last_event_id()` is the back.
- Eviction only trims the window; ids never repeat.
- The event window is part of the snapshot, so restart and learner install recover it exactly. A new
  learner receives only the retained window — that is expected, and the read API advertises
  `oldest_event_id` so consumers can detect the gap.

Snapshot format: `SNAPSHOT_VERSION` must go from 1 to 2. Postcard encodes struct fields positionally,
so a v2 decoder reading a v1 payload fails on the missing fields (`postcard::from_bytes` reports an
unexpected end of input). Two options:

1. **Reject v1 with a clear error** (simplest; acceptable for a demo, but requires wiping data).
2. Decode a versioned enum (`V1(StateMachineDataV1) | V2(StateMachineDataV2)`) and migrate.

Recommendation: bump the version, reject older files with an explicit message, and document the
migration if this repo ever stops being an example. This is nearly free: `read_snapshot_file` already
checks the version in the header and fails with `unsupported snapshot version {version}` before
decoding, so a v1 file on disk is rejected loudly rather than silently misparsed.

### 4.6 Read API and consumer protocol

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
- `limit` is capped server-side (e.g. 1000) so one call cannot allocate unbounded memory.
- Use the leader (`/events` can be a linearizable read) for clearing; follower reads are fine for
  dashboards where staleness is acceptable. Add the `X-Raft-Leader`/forwarding behaviour used by the
  other write endpoints.
- Optional phase: long-poll `wait_ms` (≤ 1000) so consumers do not spin; implemented with a
  `Notify` signalled after `apply` releases the state-machine lock.
- Optional phase: gRPC `StreamEvents` using the same cursor, mirroring `AppendStream`.

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

### 4.7 Idempotent submission

Returning events does not by itself make retries safe. Two independent dedupe layers are needed:

- **Submission:** add `client_order_id: Option<String>` to `Order` (or a wrapper request) and keep a
  bounded map `client_order_id -> order_sequence` in the state machine. A replayed request with a
  known key returns the original sequence and its events instead of placing a second order. Bound
  the map (same retention approach) and document the eviction window.
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
 "retained_events": 1499, "evictions_total": 0, "retention_limit": 262144}
```

Alert on evictions: they mean a consumer is falling behind and a resync is imminent. Also log the
event rate per apply batch at debug level, consistent with the existing `tracing` usage.

---

## 5. Failure modes

| Failure | Effect | Handling |
|---|---|---|
| Sink returns an error | That batch is missing from the sink | Logged at `warn`; `apply` and the state machine are unaffected. The file sink keeps the lines it already wrote |
| Node restarts | Replayed entries are published again | Expected for the debug file sink; dedupe on `event_id` (a durable event log in phase 2 removes the duplicates from the feed) |
| Writer crashes after commit | Response lost | Fills are in the event log; clearing pulls them by cursor |
| Leader steps down before replying | Caller gets `ForwardToLeader` | Retry with the same `client_order_id`; submission dedupe returns the original sequence |
| Follower restarts | — | Snapshot + tail replay rebuilds the identical event log |
| Learner installs a snapshot | It holds only the retained window | API reports `oldest_event_id`; consumers that need older data must already have it |
| Retention overflow | Events evicted | `evictions_total` increments; consumer sees `cursor < oldest_event_id` and resyncs |
| Duplicate delivery | Consumer sees an event twice | Dedupe on `event_id` |
| Huge sweep (many fills) | Large response | Response capped with `events_truncated`; full set available from `/events` |
| Slow consumer | — | Pull-based, so it cannot block `apply`; only its own cursor lags |
| Snapshot from an older binary | v1 payload | Version check rejects it with a clear error (or the versioned enum migrates it) |

---

## 6. API compatibility

- `MatchEngineResponse` keeps `value` (sequence string for `Place`, value for `Set`) so existing
  callers and scripts keep working. New fields are additive and serde ignores unknown fields on the
  consumer side.
- `MatchResult` itself stays as-is for the matcher; `MatchEvent` is the wire/storage type. This keeps
  the matching code independent of the clearing schema.
- `/events` is new; nothing is renamed. If a breaking v2 is ever needed, version the route rather
  than changing `/write`.
- `Order` gains `client_order_id: Option<String>` with `#[serde(default)]`, so old payloads still
  deserialize.

---

## 7. Test plan

Implemented today (`cargo test --lib`, `cargo test --test cluster test_match_results`):

- `apply` publishes `Rested`, `Fill` and `Cancelled` events to the configured sink, with the right
  order/counterparty sequences and the book's exact volumes.
- A cancel of an order that is not in the book publishes nothing.
- A failing sink is logged and does not fail `apply` or change the state.
- `FileLogSink` writes one JSON object per event with `node_id`, appends across reopen, and writes
  nothing for an empty batch.
- `resting_volume` reports the stored remainder, `None` for a fully filled or unplaced order.
- End-to-end (`tests/cluster/test_match_results.rs`): a node started with `--match-event-log`
  records rest/fill/cancel in log order and the `/write` response is unchanged.

Still to write with phases 2–4:

- Event ids are monotonic and gap-free within the retained window.
- A partial fill emits exactly one `Fill`; the remainder emits exactly one `Rested`.
- An exact fill emits `Fill` only, no `Rested`.
- Retention: push `MAX_EVENTS + N`, assert `oldest_event_id` advanced by N and ids never repeat.
- Determinism: apply the same entry list to two fresh `StateMachineStore`s and compare event streams
  field by field (`Order::PartialEq` ignores `id`, so compare ids explicitly).
- Submission dedupe: same `client_order_id` twice → one order, same sequence and events.

Integration (`tests/cluster`), beyond the file-sink test:

- `POST /write Place` returns the same events as `POST /events` for that range, on both transports.
- Restart a node after a write; `/events` still returns the fills and new ids continue after the
  replayed ones.
- Snapshot a node, purge the log, add a learner; the learner serves the retained events and reports
  the correct `oldest_event_id`.
- Kill the submitting client after commit; a second client reads the fills from `/events`.
- Retention overflow on a short window: consumer sees the gap signal.
- Two consumers with independent cursors both receive every event exactly once after dedupe.

Property test: replaying a random request stream twice yields identical event id/field sequences.

---

## 8. Phases and effort

| Phase | Work | Effort |
|---|---|---|
| 1a ✅ | `MatchEvent` + `EventId`; build events in `apply`; `MatchResultSink` with `Noop`/`Memory`/`File` sinks; `--match-event-log`; unit and end-to-end tests | done |
| 1b | Return events in `MatchEngineResponse`; cap response size with `events_truncated` | 0.5–1 day |
| 2 | Ring buffer + snapshot v2 + `oldest_event_id`/`last_event_id`; `/events` and `/events/status`; restart/snapshot/retention tests | 2–3 days |
| 3 | `client_order_id` submission dedupe with bounded map | 1–2 days |
| 4 | Optional: long-poll, gRPC `StreamEvents`, MQ bridge sidecar | 2–3 days |

Phases 1a–2 close the TODO for fills and cancels and are independently shippable. Phase 3 is required
before clearing can trust the feed under retries. Phase 4 is operational convenience.

---

## 9. Decision

Adopt **A + B**:

1. Return events from `/write` so the submitter has read-your-writes (phase 1b).
2. Keep a bounded, replicated event log in the state machine, snapshotted with the state, served by
   a cursor API (phase 2).
3. Keep delivery out of the replicated path: `apply` only publishes to a `MatchResultSink`
   (implemented, phase 1a), the call is best-effort and must never change state or fail the apply,
   and production delivery consumes the durable log rather than doing network IO inline.
4. Make submission idempotent with `client_order_id` so retries do not double-trade.

The sink trait is the seam that makes the remaining phases replaceable: an event-log-backed sink, an
MQ bridge or a sidecar can be swapped in at startup without touching `OrderBook` or `apply`.

This satisfies R1–R10 with no new consensus machinery, no change to the Raft log entry format, and no
new failure domain. The cost is a bounded amount of extra state per snapshot and one new read
endpoint.

---

## 10. Open questions

1. **Retention default** — 262k events is a guess; measure postcard size and pick a memory budget.
2. **Scope of v1 events** — answered for now: `Rested`, `Fill` and `Cancelled` are all emitted.
   Revisit if clearing only wants fills and the extra volume matters.
3. **Consumer placement** — in-cluster sidecar pulling `/events`, or an external service? Affects
   whether long-poll/streaming is worth building.
4. **Timestamp** — is a client-supplied ingest timestamp acceptable to clearing, given it is
   untrusted, or is log-order-only sufficient?
5. **Cancel identity** — `cancel()` currently keys on `(price, sequence)`, which clearing has from
   the events, but humans think in `order id`; consider an order-id index as a follow-up.
6. **Multi-instrument** — global `event_id` vs per-instrument; revisit when a second book exists.
7. **Snapshot migration** — reject v1 or write a `V1 | V2` decoder? Decide before the first
   deployment that has data worth keeping.
