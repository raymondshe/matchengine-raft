# Market-data system — Aeron multicast egress and Binance-style WebSocket distribution

Status: **implemented as a reference service.** Two systems are specified here:

1. the **matchengine** gains only the Aeron publication adapter (U7 of
   [`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md)) and broadcasts its existing deterministic feed
   on Aeron **multicast**;
2. a **separate market-data system** (its own crate and process, like `demos/order_system/`) subscribes to those
   Aeron streams, rebuilds the derived state and serves **Binance-style WebSocket streams** to
   browsers and other clients.

The engine never serves a WebSocket. Client concerns — fan-out, conflation, slow consumers,
connection limits, snapshot-on-subscribe — belong entirely to the market-data system, which is
stateless, restartable and horizontally scalable: more instances subscribe to the same multicast
group, and the engine's publish cost is independent of the client count.

Implementation language and runtime for both systems: **Rust on Tokio** (§4.7–§4.12). The engine
side stays on its existing multi-thread runtime with one dedicated publication thread; the
market-data system is a new binary built on Tokio + `actix-web`/`actix-ws`, with Aeron handles
isolated on their own thread.

Last updated: 2026-11-05.

Reference implementation: `demos/market_data_system/` (Aeron ingestion via rusteron 0.2.13 plus a
JSONL fallback; Binance-style WebSocket service; multi-symbol per instance with one shared hub).

Related documents: [`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md) (the implemented feed),
[`AERON_SOLUTION.md`](./AERON_SOLUTION.md) Part II §11–26 (the Aeron egress design this builds
on), [`MATCH_RESULTS.md`](./MATCH_RESULTS.md) (the pull-based replicated event log),
[`DECIMAL.md`](./DECIMAL.md) (scale-18 `Fixed` rendering),
[`DEMO_ORDER_SYSTEM.md`](./DEMO_ORDER_SYSTEM.md) (the precedent for a separate crate),
[`AGENTS.md`](../AGENTS.md).

## 1. Summary

```text
┌───────────────────────────── matchengine (unchanged core) ─────────────────────────────┐
│ apply ──try_send──► tap queue ──► market-data processor (deterministic, per entry)     │
│                                        │ MarketDataSink::publish                       │
│                                        ▼                                               │
│                        AeronSink (new, feature `market-data-aeron`)                    │
│                          wire v7 frames → one publication per stream 2001..2007        │
└──────────────────────────────┬──────────────────────────────────────────────────────────┘
                               │ Aeron multicast (or unicast relay)
                               ▼
┌───────────────── market-data system (new crate + process, N instances) ────────────────┐
│ Aeron subscriptions ──► decode wire v7 ──► assembler (depth map · BBO · bars ·          │
│ watermark · dedupe by (symbol, stream, market_seq, sub_seq))                            │
│        │                                                                               │
│        ▼                                                                               │
│ WS hub: caches (watch) · conflation (100 ms/1000 ms) · serialize once → broadcast      │
│        │                                                                               │
│        ├── actix-ws sessions: Binance stream names, SUBSCRIBE control, snapshots       │
│        └── GET /marketdata/depth (snapshot parity)                                     │
└──────────────────────────────┬──────────────────────────────────────────────────────────┘
                               ▼
                gateway (TLS, auth, LB) ──► wss clients
```

- The engine publishes **once per stream per applied entry**, regardless of how many clients or
  market-data instances exist; Aeron multicast does the fan-out to instances, each instance fans
  out to its own WebSocket clients.
- **Client capacity scales horizontally** (§4.13): instances are homogeneous and stateless, every
  instance receives the full feed, and the marginal cost of a client is one queue plus socket
  writes — the engine's publish cost and each instance's upstream cost are constants. A fleet of
  N instances serves ≈ N × per-instance connections behind one load balancer, with no sticky
  sessions, no sharding and no coordination protocol.
- The market-data system is a **pure consumer**: it holds no Raft state, writes nothing to the
  engine, and can be restarted or scaled at any time — state is rebuilt from the feed
  (snapshot + deltas), exactly the recovery model the Aeron design already defines for consumers
  (§18 of the Aeron document).
- Clients see **Binance's protocol shape**: stream names (`eurusd@depth@100ms`), a
  SUBSCRIBE/UNSUBSCRIBE control protocol, combined streams, the `U`/`u`/`lastUpdateId`
  diff-depth synchronization contract, `bookTicker`, partial depth, klines, ping/pong and 24-hour
  connection rotation (§2).

## 2. Reference model: Binance spot market streams

Binance facts below are from Binance's public spot stream documentation; they are the **reference
model**, not a compatibility contract. Where our identity model differs, this document says so
explicitly (§2.6).

### 2.1 Connection model

- Base endpoints: `wss://…/ws/<streamName>` for a single raw stream and
  `wss://…/stream?streams=<s1>/<s2>/<s3>` for combined streams.
- Combined stream payloads are wrapped: `{"stream":"<streamName>","data":<rawPayload>}`.
- Stream names are lowercase; a connection may subscribe at connect time (URL) or live
  (control messages).
- A single connection can listen to up to **1024 streams**; new connections are throttled per IP
  (**300 connection attempts per 5 minutes**).
- A connection is valid for **24 hours**; the server disconnects after that and clients are
  expected to reconnect and re-subscribe.
- The server sends a **ping frame every 3 minutes**; if no **pong** is received within
  **10 minutes**, the connection is dropped. Unsolicited pongs from clients are allowed.
- Servers restart; clients must handle reconnect and re-synchronize state.

### 2.2 Subscription protocol

Control messages are JSON text frames with an echo id:

```json
{"method":"SUBSCRIBE","params":["btcusdt@aggTrade","btcusdt@depth"],"id":1}
{"result":null,"id":1}
{"method":"UNSUBSCRIBE","params":["btcusdt@depth"],"id":312}
{"result":null,"id":312}
{"method":"LIST_SUBSCRIPTIONS","id":3}
{"result":["btcusdt@aggTrade"],"id":3}
{"error":{"code":0,"msg":"Unknown property"},"id":2}
```

### 2.3 Stream catalogue (spot)

| Stream | Payload | Speed |
|---|---|---|
| `<symbol>@trade` | one executed trade | realtime |
| `<symbol>@aggTrade` | same-price fills of one order aggregated | realtime |
| `<symbol>@kline_<interval>` | OHLCV candle, `x` marks the final update | 1000 ms |
| `<symbol>@miniTicker` / `<symbol>@ticker` | 24 h rolling window statistics | 1000 ms |
| `!miniTicker@arr` / `!ticker@arr` | all symbols, arrays | 1000 ms |
| `<symbol>@bookTicker` | best bid/ask with update id `u` | realtime |
| `<symbol>@depth<levels>@<speed>` | partial book snapshot, `levels` ∈ 5/10/20 | 1000 ms or 100 ms |
| `<symbol>@depth@<speed>` | diff depth: `U`..`u` update-id range, absolute level quantities | 1000 ms or 100 ms |

Kline intervals: `1s 1m 3m 5m 15m 30m 1h 2h 4h 6h 8h 12h 1d 3d 1w 1M`.

### 2.4 Local order book maintenance (the contract we keep verbatim)

Binance's documented procedure for maintaining a local book from the diff stream:

1. Open the diff-depth stream and **buffer** events.
2. Fetch a REST depth snapshot; note its `lastUpdateId`.
3. **Drop** every buffered event with `u <= lastUpdateId`.
4. The **first applied** event must satisfy `U <= lastUpdateId + 1` **and** `u >= lastUpdateId + 1`.
5. Every subsequent event must have `U == previous u + 1`; a larger `U` means events were missed —
   discard the local book and start over (re-snapshot).
6. Level quantities in diff events are **absolute**: `"0"` deletes the level.

### 2.5 What we copy

- Stream naming, lowercase symbols, URL and live subscription, combined-stream envelope.
- The `U`/`u`/`lastUpdateId` diff-depth contract, including rules 3–6 above, verbatim.
- `bookTicker`, partial-depth and kline payload shapes (with documented extensions).
- Server ping / pong timeout / 24-hour lifetime / per-connection stream cap / close codes.
- Snapshot + delta as the recovery model; updates carry **absolute quantities**, never increments.

### 2.6 What we deliberately do differently

| Binance | Ours | Why |
|---|---|---|
| Integer trade id `t` | `(L, t)` = (Raft log index, ordinal) | fills are identified by `EventId`; there is no separate replicated trade counter |
| `aggTrade` id `a` | `@sweep` stream (one message per taker, all fills) in P1; price-grouped `@aggTrade` in P2 | one taker may sweep several price levels, which does not fit Binance's single-price aggregate shape |
| REST snapshot required for depth sync | snapshot is pushed **on the WS at subscribe time**; an HTTP snapshot endpoint is optional parity | removes the REST round-trip and the snapshot/stream race for our clients; Binance rules still hold |
| 1024 streams per connection | 1024 (configurable) | Binance parity; one connection can hold several symbols' streams |
| Edge serves thousands of symbols | one symbol per engine deployment/cluster; one service instance aggregates several engines | engine contract stays one symbol per cluster; the service has one feed/state per configured symbol behind a shared hub |
| `E`/`T` event times from the exchange | intake timestamps replicated in the log | the feed's rule: never a publish-time clock (§13 of the Aeron design) |
| JSON only | JSON first; binary (wire v7 / an SBE-like format) is a later option | Binance itself now offers SBE streams; our wire v7 already exists on the Aeron edge |

## 3. Goals and non-goals

### Goals

- **Engine isolation:** matching, Raft, WAL and snapshots are untouched; the engine's only new
  work is publishing already-derived messages to Aeron (off the matching path, behind a feature
  flag). Client count and client behaviour cannot affect `apply` — the tap contract is preserved
  end to end.
- **One publish, many consumers:** Aeron multicast fans the feed out to every market-data
  instance; instances fan out to their own WebSocket clients. The engine does O(1) work per entry
  regardless of audience.
- **Independent lifecycle:** the market-data system is stateless and restartable; it can be
  upgraded, restarted, scaled or killed without any engine or Raft involvement.
- **Horizontal scale-out:** when WebSocket connections outgrow a single node, more instances are
  added behind a load balancer; capacity grows linearly with instance count while the engine's
  publish cost stays constant (§4.13).
- **Binance protocol shape** so Binance-style client libraries and book-maintenance tutorials work
  with minimal changes.
- **Deterministic content:** every payload is derived from replicated state
  (`(symbol_id, stream_id, market_seq, sub_seq)` identity); two instances fed the same frames
  produce identical stream content. Only conflation *grouping* is instance-local.
- **Recovery everywhere:** engine failover, instance restart, feed gap, slow consumer and 24-hour
  rotation all recover through snapshot + delta with monotone update ids.

### Non-goals

- WebSocket service from the engine process (explicitly rejected — §17).
- Order entry, account streams or any write path over WebSocket. The engine's public write surface
  stays HTTP `/write`/`/write_batch`; Binance's *WebSocket API* is a separate product and is out of
  scope.
- Authentication, TLS or public-internet exposure: the WS edge sits behind the gateway, and the
  Aeron channel is trusted-network-only, exactly like the engine's HTTP/gRPC edge (§11).
- Durable replay/retention on the market-data path (Aeron is not store-and-forward; the Raft log
  and the replicated event window remain the record — see `MATCH_RESULTS.md`).
- Multiple books in one engine process; the wire carries `symbol_id`, and one service instance
  consumes several single-symbol engine deployments (one feed/state per symbol).
- Exactly-once delivery. The contract is monotone update ids, snapshot + delta, client-side
  re-sync on discontinuity — the same as Binance.

## 4. System architecture

### 4.1 Deployment view

```text
   engine cluster (Raft: 1..N nodes, one symbol)
   ┌────────────┐   ┌────────────┐   ┌────────────┐
   │  leader    │   │ follower   │   │ follower   │     leader-only publishing by
   │  publish ──┼──►│ (no pub.)  │   │ (no pub.)  │     default; `=false` lets every
   └─────┬──────┘   └────────────┘   └────────────┘     node publish identical frames
         │ exclusive publications, one per stream 2001..2007
         ▼
   Aeron media driver · multicast group (or unicast relay)
         │  UDP multicast: 239.x.x.x:base+3000+k  (Aeron §15 port convention)
   ┌─────┴────────┐   ┌─────────────┐   ┌─────────────┐
   │ market-data  │   │ market-data │   │ market-data │   N independent instances;
   │ instance 1   │   │ instance 2  │   │ instance N  │   each: subscribe → assemble →
   │  WS + HTTP   │   │  WS + HTTP  │   │  WS + HTTP  │   Binance WS hub
   └─────┬────────┘   └──────┬──────┘   └──────┬──────┘
         └───────────────────┴─────────────────┘
                             ▼
                 gateway: TLS, client auth, LB, per-IP throttling
                             ▼
                    wss clients (browsers, bots)
```

- The JSONL debug sink (`--market-data-log`) remains node-local and independent.
- One engine deployment = one symbol = one multicast channel/stream set. A service instance may
  subscribe to several channels (one per configured symbol) and route by `symbol_id`; its shared
  hub lets one WebSocket connection hold streams from any of them.

### 4.2 Engine side — the Aeron publication adapter (U7 realized)

This is the deferred U7 unit of `MARKET_DATA_PLAN.md`, specified by `AERON_SOLUTION.md` §15–§20
and only lightly extended here.

- **Sink:** `AeronSink` implements the existing `MarketDataSink` trait in
  `src/market_data/aeron.rs`, behind the `market-data-aeron` feature
  (`rusteron-client` + `rusteron-media-driver`, default off so the default build and `cargo test`
  need no driver). Installed through a new `MarketDataHandle::start_with_sink(config, sink)` seam
  (today the sink is chosen from `log_path` only); the JSONL sink remains available alongside.
- **Publications:** one **exclusive publication per stream** (2001–2007) so a large depth snapshot
  cannot head-of-line-block a trade or a status frame; created lazily on leadership gain, closed on
  loss (the Aeron design's §15 rules). Term lengths per §15 (1–4 MiB) set the retention window.
- **Channel:** `aeron:udp?endpoint=<group>:<port>|interface=<iface>` multicast by default, with
  ports following the existing convention (`http_port + 3000 + stream index`), overridable by
  flag. Where the network forbids multicast (common in cloud VPCs), the same sink publishes N
  unicasts or a single relay subscribes — the market-data system is unchanged either way, because
  it only consumes.
- **Frames:** the existing wire v7 format, unchanged (golden-tested with a strict reference
  decoder). The market-data system reuses `wire::decode` from the engine crate via a path
  dependency, exactly like `demos/order_system/` reuses the client.
- **Backpressure:** the processor task is off the matching path and *may* slow down; per the
  Aeron design's §17 rule, a stalled publication slows the processor, the bounded tap queue fills
  and the feed degrades to `GAP` while consensus continues. `apply` latency is independent of the
  feed. (The publication thread retries `try_offer` under backpressure; it never blocks `apply`.)
- **New: periodic snapshot re-broadcast.** Today consumers can only (re)join on a reset snapshot
  pushed at startup or on a leadership change (§18 of the Aeron design). A separate market-data
  system that starts, restarts or gaps mid-stream must not wait for the next election. The engine
  therefore re-publishes the reset `DEPTH_SNAPSHOT` (+ refreshed BBO/bar caches) on a configurable
  cadence (`--market-data-aeron-snapshot-every-ms`, default 30 000, `0` disables). Snapshots are
  idempotent resets anchored at the next `market_seq`; a **continuous** consumer verifies boundary
  continuity and ignores them, so only empty or gapped consumers apply them.
- **New: idle heartbeat.** With no trades there are no frames, and a consumer cannot distinguish
  silence from death. The engine publishes a diagnostic `STATUS` keepalive with a reserved
  `sub_seq` (like `GAP`'s `u16::MAX`) every `--market-data-aeron-heartbeat-ms` (default 1000)
  whenever nothing was applied, carrying epoch/leader/watermark. It is node-local and diagnostic —
  it never participates in the deterministic key space.
- **New: snapshot request (stream 2010).** A service that subscribes before the cached depth is
  ready, or marks depth stale after a gap, sends a fixed 24-byte `MSRQ` v1 request
  (`symbol_id`, `depth_levels`, `request_id`) on stream base + 9. The engine captures the book
  under the normal state-machine lock (clamped to its configured snapshot depth) and queues a
  reset snapshot; the next periodic snapshot remains a fallback. The request is diagnostic input,
  never replicated state.
- **Contiguity (P0, the one processor change):** Binance's rule 5 (`U == previous u + 1`) requires
  the update id to advance by exactly one per emitted diff. `market_seq` advances by one per
  mutating entry, but the processor today emits a `DEPTH_DELTA` only for entries with touched
  levels (rejections, parked stops, a `Day` purge of stops, a parked stop order: no visible depth
  change → no message). The processor must emit **one depth delta per mutating entry, with an
  empty level list when nothing changed**, so every consumer can use Binance's contiguity rule
  verbatim. Cost: a ~64-byte empty message per no-change entry, on the market-data path only.
  `MARKET_DATA_PLAN.md` §15's "one message per entry with touched levels" becomes "one message
  per entry, possibly empty".

### 4.3 Market-data system — ingestion from Aeron

The market-data system is the new crate `demos/market_data_system/` (sibling of `demos/order_system/`, own `Cargo.toml`, binary
`matchengine-marketdata`), depending on `matchengine-raft` by path (for `wire`, `MarketMessage`
and the record types), `rusteron-client`, `actix-web`/`actix-ws` and `tokio`.

- **Subscriptions:** one Aeron `Subscription` per stream id (2001–2007) on each engine's channel,
  with one source per configured symbol. Aeron reassembles fragmented publications; the subscriber
  decodes only complete frames through the existing `wire::decode`.
- **`!Send` isolation:** rusteron handles are not `Send`. A **dedicated subscriber thread** (an OS
  thread, not a tokio task) owns the Aeron context and all subscriptions, polls with an idle
  strategy, decodes frames and hands complete `MarketMessage`s to the async side through a
  bounded, non-blocking handoff. It never blocks: a full handoff queue drops the message, counts
  it and raises the assembler's gap flag (multicast Max flow control plus the term retention
  window already tolerate a briefly slow consumer; a consumer that falls beyond retention gets
  Aeron's own loss signal, which takes the same gap path).
- **Test seam:** ingestion sits behind a small trait so the hub and protocol layers are testable
  without a media driver:

```rust
/// One decoded wire v7 message, or a transport-level loss signal.
pub enum Sourced { Message(MarketMessage), Loss { last_seen: EventId, dropped: u64 } }

pub trait FrameSource: Send + 'static {
    async fn next(&mut self) -> io::Result<Sourced>;
}
```

  `AeronSource` (subscriber thread + handoff queue) implements it in production;
  `ChannelSource` (a plain tokio mpsc) implements it in unit and integration tests. This mirrors
  how the engine's tap is tested through a channel.

### 4.4 Market-data system — state assembly

The **assembler** is the only writer of the derived state. It consumes `Sourced` items and:

- **dedupes** by `(symbol_id, stream_id, market_seq, sub_seq)` — required for at-least-once
  transport, re-delivery after NAK retransmits, and the `leader_only=false` mode where every node
  publishes identical frames (multiple Aeron session ids on the same stream);
- applies `DEPTH_SNAPSHOT` (reset: rebuild the depth map at the boundary; `RESET` flag observed),
  `DEPTH_DELTA` (absolute post-entry totals), `BBO`, `BAR`, `TRADE`, `LIFECYCLE`, `STATUS`
  (watermark), `GAP`/`Loss` (gap policy below);
- tracks the **continuity watermark** (last applied `market_seq` per stream) which drives both the
  WS update ids and the gap policy;
- on a **gap** (dropped frames, Aeron loss, or a `U`-discontinuity once contiguity is dense):
  invalidate the depth map, keep serving latest-value streams (BBO, bars) which are unaffected,
  mark depth subscriptions for re-snapshot, and wait for the next periodic snapshot (§4.2) —
  bounded by `snapshot-every-ms` — instead of coupling to the engine.

The assembler owns the same shadow-state pattern the engine's processor uses (a second
shadow-of-shadow), seeded by `DEPTH_SNAPSHOT` and maintained by deltas. It never talks to the
engine over HTTP: the service is decoupled except for the Aeron channel and the pair-name config
(§5.2).

### 4.5 Market-data system — the WS hub

Downstream of the assembler, the hub turns assembled state into Binance-shaped streams:

- **caches** published as `tokio::sync::watch` channels of `Arc` payloads (depth map/top-N, BBO,
  current bar per interval, watermark) — sessions read the latest value without locks or hub round
  trips;
- **conflation**: `@depth@100ms` / `@depth@1000ms` windows merge all deltas whose update id falls
  in the window into one `depthUpdate` with `U` = first id, `u` = last id (exactly Binance's
  interval semantics); a window with no activity emits nothing; `bookTicker` is realtime; klines
  update when a trade rolls the bar. Windows are **instance-local** (a `tokio::time::interval`),
  so two instances may group the same content differently — acceptable and Binance-like, since
  clients key off `U`/`u`, never off wall-clock cadence;
- **serialize once**: each stream message is encoded to JSON `Bytes` once and shared by every
  subscriber (a clone is a refcount); combined-stream envelopes wrap the shared bytes;
- **fan-out**: per stream key (name + speed) a `tokio::sync::broadcast` channel, created lazily on
  the first subscriber and pruned when the last receiver drops.

### 4.6 Snapshot-on-subscribe

Subscribing to a stateful stream immediately pushes the cached state before deltas:

| Stream | Snapshot pushed on subscribe |
|---|---|
| `@depth` | one `depthSnapshot` frame (§6.5) with `lastUpdateId`, then `depthUpdate` deltas |
| `@depthN` | the current top-N partial-depth frame |
| `@bookTicker` | the current best bid/ask frame |
| `@kline_<i>` | the current (open) bar, then updates |
| `@trade`, `@sweep`, `@lifecycle`, `@status` | nothing (pure event streams) |

Caches come from the assembler (fed by the feed's reset snapshots and deltas), so the engine is
never contacted per subscription and a burst of subscribers cannot contend with anything.

### 4.7 Rust/Tokio implementation model — crates and baseline

Both systems are plain async Rust on Tokio; no actor framework, no async-std, no threads created
by framework magic.

| Piece | Choice | Notes |
|---|---|---|
| Engine Aeron publication | `rusteron-client` behind feature `market-data-aeron` | as planned by U7 and Aeron §20; default build stays pure Rust, `cargo test` needs no driver |
| Engine publication thread | one OS thread owning all `!Send` rusteron handles | fed by the processor task through a bounded handoff; retries `try_offer` under backpressure (§4.2) |
| Service WebSocket | `actix-ws` 0.3 | the **non-actor futures API** (`actix_ws::handle(req)` → `HttpResponse + MessageStream + Session`) on the service's own actix-web listener |
| Service HTTP | `actix-web` 4 | snapshot endpoint, health, metrics |
| Service Aeron subscription | `rusteron-client` on a dedicated thread | §4.3; `FrameSource` is the seam |
| Client (tests only) | `tokio-tungstenite` | dev-dependency of the service crate and of `tests/` |
| Tasks | `tokio::spawn` on each process's multi-thread runtime | the service binary uses `#[tokio::main(flavor = "multi_thread")]` like the engine |
| Channels | `tokio::sync::{mpsc, broadcast, watch}` | plus the two thread↔task handoff queues (§4.8) |
| Timers | `tokio::time::{interval, sleep_until, Instant}` | conflation, ping, pong timeout, 24 h rotation; the subscriber/publisher threads use their own idle strategies |
| Shared buffers | `bytes::Bytes` | serialize once, refcount everywhere |
| Locks | one `std::sync::Mutex` for the service's stream registry, never held across `.await` | everything else is single-owner task state, `watch`, or atomics |
| MSRV | Rust 1.89, edition 2024 | the feature-gated rusteron deps and the new crate must satisfy the CI `msrv` job |

No `spawn_blocking` anywhere in the data path: both threads (publisher, subscriber) are dedicated
OS threads that never run async code; all async work is pure tokio tasks.

### 4.8 Task and channel topology (market-data system)

```text
dedicated OS thread                     main multi-thread Tokio runtime
┌──────────────────────────┐   bounded  ┌────────────────────────────────────────────┐
│ Aeron subscriptions      │   handoff  │ one feed task per symbol                   │
│ (one source per symbol)  │──queue────►│   dedupe · depth map · BBO · bars ·         │
│ poll · decode wire v7    │  (drop→gap)│   watermark · gap policy                   │
│ (rusteron, !Send)        │            │   caches: watch<Arc<Payload>>              │
└──────────────────────────┘            │   conflation: interval(100ms/1000ms)       │
                                        │   serialize once → Bytes                   │
                                        │ shared registry: per stream-key            │
                                        │   broadcast<Bytes>                         │
                                        │        │                ▲                  │
                                        │ session task ◄───────────│ subscribe       │
                                        │   socket halves · control frames           │
                                        │   mpsc<Outbound> ◄ forwarder tasks         │
                                        │   ping interval · pong deadline · 24 h     │
                                        └────────────────────────────────────────────┘
```

Channel inventory (every hop is non-blocking at the sender):

| Channel | Type | Capacity | Full/overflow behavior |
|---|---|---|---|
| Aeron thread → assembler | bounded handoff queue | 8192 | drop + count + gap flag; assembler re-syncs from the next periodic snapshot |
| assembler → subscribers | `broadcast<Bytes>` per stream key | 1024 | `send` ignores no-receiver errors; lagging receivers get `Lagged` (§4.10 policy) |
| forwarder → session writer | `mpsc<Outbound>` | 1024 | `try_send` fails → session marked slow → close `1013` |
| caches | `watch<Arc<Payload>>` | 1 (latest) | `send_replace`; readers `borrow().clone()` — never blocks, never misses |
| lifecycle | `watch<bool>` stop | 1 | assembler, hub and all sessions select on it; driven by shutdown |

The stream registry (`HashMap<StreamKey, broadcast::Sender<Bytes>>`) sits behind the single
`std::sync::Mutex`: the assembler publishes through it and sessions subscribe/unsubscribe through
it; the lock guards a lookup plus a `Sender::send`/clone — both synchronous, never held across an
`.await`.

### 4.9 The assembler/hub task

One feed task per symbol owns that symbol's derived state (sketch):

```rust
struct Assembler {
    state: FeedState,                     // depth map, BBO, bars, watermark, dedupe ring
    caches: HubCaches,                    // watch senders
    registry: Arc<Mutex<StreamRegistry>>,
    pending: HashMap<Speed, DepthWindow>, // conflating U..u ranges
}

async fn run(mut self, mut source: Box<dyn FrameSource>, mut stop: watch::Receiver<bool>) {
    let mut fast = tokio::time::interval(Duration::from_millis(100));
    let mut slow = tokio::time::interval(Duration::from_millis(1000));
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            item = source.next() => self.apply(item),   // dedupe, update state, publish realtime
            _ = fast.tick() => self.flush_depth(Speed::Ms100),
            _ = slow.tick() => self.flush_depth(Speed::Ms1000),
        }
    }
}
```

`apply` maintains the caches (a `DEPTH_SNAPSHOT` with `RESET` rebuilds the depth map and refreshes
every watch cache; a delta updates level totals; a `BAR` refreshes that interval's cache) and
publishes realtime streams (`trade`, `sweep`, `bookTicker`, `kline`, `status`, `lifecycle`)
immediately: serialize once into `Bytes`, then `registry.get(key).send(bytes)`. `flush_depth`
merges the pending window into one `depthUpdate` and refreshes the partial-depth caches.

### 4.10 Session and forwarder tasks

Each connection spawns one session task plus one small forwarder task per subscription (all on the
service's main runtime):

```rust
async fn session(mut stream: MessageStream, mut sink: Session, ctx: SessionCtx) {
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(ctx.queue);
    let mut subs = Subscriptions::new();                      // StreamKey -> forwarder handle
    let mut ping = tokio::time::interval(ctx.ping_every);
    let rotate_at = tokio::time::Instant::now() + Duration::from_secs(24 * 3600);
    let mut pong_deadline = tokio::time::Instant::now() + ctx.pong_timeout;
    loop {
        tokio::select! {
            _ = ctx.stop.changed()                      => { let _ = sink.close(CloseCode::GoingAway); break; }
            _ = ping.tick()                             => { let _ = sink.ping(b""); }
            _ = tokio::time::sleep_until(pong_deadline) => { let _ = sink.close_reason(1006, "pong timeout"); break; }
            _ = tokio::time::sleep_until(rotate_at)     => { let _ = sink.close_reason(1000, "24h limit"); break; }
            frame = stream.next() => match frame {
                Some(Ok(Message::Pong(_))) => pong_deadline = tokio::time::Instant::now() + ctx.pong_timeout,
                Some(Ok(Message::Text(raw))) => subs.apply_control(&raw, &out_tx, &ctx),  // §5.3, rate-limited
                Some(Ok(_)) => {}                                   // Ping/Close handled by actix-ws
                Some(Err(_)) | None => break,
            },
            out = out_rx.recv() => match out {
                Some(Outbound::Frame(bytes)) => { if sink.text(bytes).is_err() { break; } }
                Some(Outbound::Resync)      => { /* push cached snapshot frames */ }
                None => break,
            },
        }
    }
    subs.abort_all();                                            // drops broadcast receivers
}

async fn forward(mut rx: broadcast::Receiver<Bytes>, out: mpsc::Sender<Outbound>, class: Class) {
    loop {
        match rx.recv().await {
            Ok(mut bytes) => {
                if class == Class::LatestValue {                  // bookTicker / depthN / miniTicker
                    while let Ok(newer) = rx.try_recv() { bytes = newer; }   // drain-to-latest
                }
                if out.try_send(Outbound::Frame(bytes)).is_err() { break; } // session slow/closed
            }
            Err(broadcast::error::RecvError::Lagged(_)) if class == Class::LatestValue => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => {       // sequenced stream missed frames
                let _ = out.try_send(Outbound::Resync);            // depth: re-snapshot; else close 1013
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
```

The forwarder is where the §9 policy lives: `LatestValue` streams are conflated by draining to the
newest frame before one `try_send` (a burst collapses to one queue slot); `Sequenced` streams never
skip. `SUBSCRIBE` also pushes the stream's cached snapshot (§4.6) straight onto `out_tx`.

### 4.11 What runs where — the actix worker trap

The service runs its own `HttpServer` on its multi-thread runtime, and actix-server still hosts
each HTTP connection handler on a **dedicated worker thread with its own current-thread runtime**
(the same topology the engine's audit in `PERFORMANCE.md` describes). A WebSocket session must
therefore **not** be driven inline in the route handler or spawned with plain `tokio::spawn` from
there — that would pin long-lived sessions to HTTP worker threads and starve request handling.
The route handler performs the handshake and hands the session over:

```rust
#[get("/ws")]
async fn ws(req: HttpRequest, body: web::Payload, hub: web::Data<HubHandle>) -> HttpResponse {
    let (response, stream, session) = match actix_ws::handle(&req, body) {
        Ok(it) => it,
        Err(_) => return HttpResponse::BadRequest().finish(),
    };
    hub.runtime_handle().spawn(session(stream, session_sink, hub.session_ctx()));
    response
}
```

`HubHandle` captures `tokio::runtime::Handle::try_current()` at startup and stores it in app data;
handlers stay cheap and sessions share the multi-thread scheduler.

### 4.12 Shutdown, ownership and the no-blocking rules

- **Service shutdown:** SIGTERM flips the `watch<bool>` stop; sessions close sockets (`1001`) and
  abort forwarders; the assembler drains and exits; the Aeron thread observes a stop flag between
  polls and releases its subscriptions; the HTTP server stops last. Restart is safe: state is
  rebuilt from the next periodic snapshot (§4.2).
- **Engine shutdown:** unchanged `MatchEngineNode::shutdown()` semantics; the Aeron sink closes
  its publications with the feed, and the leader watcher already closes the gate on leadership
  loss.
- **Ownership:** the assembler solely owns derived state; sessions own their socket halves,
  subscription set and outbound queue; caches cross the boundary only as `watch` channels of
  `Arc` payloads and atomics. Everything on the async side is `Send + 'static` — the only `!Send`
  objects (rusteron handles) are deliberately confined to the two dedicated threads.
- **No-blocking rules:** `try_send` at every async hop, `broadcast::send` is synchronous,
  `watch` never blocks, the registry `Mutex` is never held across an `.await`, and the Aeron
  threads poll with idle strategies rather than blocking on the async side.
- **Clocks:** `tokio::time` is used only for conflation windows, ping/pong and the 24 h rotation —
  all instance-local mechanics. Payload timestamps remain the replicated intake timestamps; no
  publish-time clock ever labels data.

### 4.13 Horizontal scale-out

When WebSocket connections grow beyond a single node's capability, the answer is more instances —
nothing else changes. Instances are **homogeneous and independent**: every instance subscribes to
the same multicast streams and serves the complete stream catalogue for the symbol, so client
capacity is additive and no instance ever depends on another.

```text
                          ┌─ instance 1 · 256 conns ─┐
engine (O(1) publish) ────┤─ instance 2 · 256 conns ─┤──── LB ──── clients (wss)
      multicast group     └─ instance N · 256 conns ─┘
```

- **Marginal client cost is one queue + socket writes.** Upstream ingestion (one Aeron
  subscription per stream) and serialization (once per stream message, shared `Bytes`) are
  per-instance **constants**, independent of connection count. Adding a client adds only its
  per-connection FIFO and its socket writes; adding an instance adds one more subscription and
  one more copy of the (small) assembler state. The engine's publish cost never changes.
- **Any instance can serve any client.** There is no stream affinity or sharding to route around:
  every instance receives every stream, so the load balancer can use least-connections balancing
  at connect time, and a reconnecting client can land anywhere and re-sync from the
  subscribe-time snapshot (§4.6). Sticky sessions are unnecessary.
- **Joining an instance is cheap and bounded.** A new instance subscribes, assembles state from
  the periodic snapshot (§4.2) plus the deltas after it, and reports `/ready` only when current;
  the load balancer then routes new connections to it. Join time is bounded by
  `--market-data-aeron-snapshot-every-ms` (default 30 s) plus replay of the deltas since the
  snapshot — no engine involvement, no Raft knowledge, no client-visible event.
- **Leaving an instance is a non-event.** The load balancer drains it (sockets close `1001`);
  clients reconnect to surviving instances and re-sync from the subscribe-time snapshot.
  Autoscaling can drive the instance count from connection count, CPU or outbound queue depth
  with no coordination protocol between instances.
- **Blast radius is one instance.** A slow or malicious client consumes only its own queue (§9).
  A slow *instance* is shielded by multicast Max flow control: it falls behind, gaps and re-syncs
  itself from the next snapshot without pacing the engine or any other instance (the Min
  strategy would stall the publisher for everyone and must not be used — §18).
- **Capacity arithmetic is linear.** Per connection: its FIFO (1024 pointers), shared payload
  refcounts and socket buffers; per instance: the writer tasks' socket writes of
  already-serialized bytes. With the §15 budget (roughly one core per ~256 active connections at
  default limits on a busy book), a fleet of N instances serves ≈ N × `--max-connections`
  clients — 10 instances ≈ 2 500, 40 ≈ 10 000. Raise `--max-connections` (and the process file
  descriptor limit) for larger instances; the limit is an abuse guard, not the design's ceiling.
- **The knobs that actually bound an instance** are file descriptors, socket write bandwidth
  (bytes/s × connections) and writer-task CPU. None of them is shared state, and none requires an
  engine change.

Geographic reach is the one thing multicast does not provide by itself: instances must sit where
the channel reaches. Cross-DC distribution uses one relay (or the P3 Kafka/NATS bridge) per DC
subscribing once and re-publishing locally — the WS tier is unchanged.

## 5. Endpoints, stream names and control protocol

All served by the **market-data system** (not the engine). One instance may consume several
engine deployments (one symbol each) and serve them from a shared hub; a load balancer may front
many instances.

### 5.1 URLs

- `GET /ws` — bare connection; subscribe with control messages.
- `GET /ws/<streamName>` — single raw stream (Binance parity).
- `GET /stream?streams=<s1>/<s2>/…` — combined streams with the `{"stream":…,"data":…}` envelope.
- `GET /health`, `GET /ready` — instance liveness; ready means the assembler has a snapshot and a
  current heartbeat (so a load balancer never routes clients to an instance still syncing).

### 5.2 Stream names

`<symbol>` is the lowercase concatenation of the traded asset codes (`eurusd`, `btcusdt`). The wire
carries only `symbol_id`, so the symbol name and id are **service configuration**
(`--symbol eurusd:0:EUR:USD`, repeatable), validated against every frame's `symbol_id`; frames for
an unconfigured id are rejected as a configuration error. (Asset codes are deliberately not on the
wire; replicating them in every frame would bloat it for one field a deployment sets once.)

The subscription set belongs to the **connection**, not to a server-side current symbol. A client
can subscribe streams for several symbols on one socket and route frames by `s`; switching a UI's
symbol is `UNSUBSCRIBE` of the old symbol's topics plus `SUBSCRIBE` of the new ones on the same
connection.

| Stream name | Source stream(s) | Binance equivalent | Phase |
|---|---|---|---|
| `<symbol>@trade` | 2001 `TRADE` (per fill) | `@trade` | P1 |
| `<symbol>@sweep` | 2001 `TRADE` (per taker, all fills) | — (extension) | P1 |
| `<symbol>@aggTrade` | price-grouped 2001 | `@aggTrade` | P2 |
| `<symbol>@bookTicker` | 2002 `BBO` | `@bookTicker` | P1 |
| `<symbol>@depth@100ms` / `@1000ms` | 2004 `DEPTH_DELTA` conflated | `@depth@speed` | P1 |
| `<symbol>@depth5/10/20@100ms` / `@1000ms` | assembler cache from 2003/2004 | `@depthN@speed` | P1 |
| `<symbol>@kline_<interval>` | 2007 `BAR` | `@kline_<interval>` | P1 |
| `<symbol>@miniTicker` / `@ticker` | service 24 h rolling window | `@miniTicker` / `@ticker` | P2 |
| `<symbol>@lifecycle` | 2005 `LIFECYCLE` | — (extension, ops) | P1 |
| `<symbol>@status` | 2006 `STATUS` + heartbeat | — (extension) | P1 |
| `resync` notice | service on gap/reset | — (extension) | P1 |

Kline intervals accept the Binance strings (`1s 1m 3m 5m 15m 30m 1h 2h 4h 6h 8h 12h 1d 3d 1w 1M`)
plus `<ms>ms` for any configured bar interval; they must match the engine's
`--market-data-bar-intervals-ms` values, which the service also takes as config.

### 5.3 Control protocol

Exactly Binance's messages: `SUBSCRIBE`, `UNSUBSCRIBE`, `LIST_SUBSCRIPTIONS`, each with `params`
and an echoed `id`; replies `{"result":null,"id":N}` / `{"result":[…],"id":N}` /
`{"error":{"code":C,"msg":"…"},"id":N}`. `SET_PROPERTY` is not supported.

Error codes: `0` unknown stream or bad stream name, `1` invalid request shape, `2` subscription
limit exceeded, `3` rate limit exceeded, `4` instance not ready (assembler syncing).

### 5.4 Combined streams

`GET /stream?streams=eurusd@bookTicker/btcusdt@depth@100ms` wraps every payload (the
`stream` field identifies the symbol and stream):

```json
{"stream":"eurusd@bookTicker","data":{"u":135,"s":"eurusd","b":"1.085","B":"800","a":"1.086","A":"500"}}
```

### 5.5 Rejecting and closing

- Unknown symbol or stream → `{"error":{"code":0,"msg":"unknown stream"},"id":N}` (no close).
- Malformed control JSON → error code 1; repeated abuse → close 1008.
- Inbound text frames beyond control messages are ignored; inbound frame size cap 4 KiB.
- Close codes used by the server: `1000` (24 h limit or shutdown), `1008` (policy), `1011`
  (internal error), `1013` (slow consumer — reconnect, re-subscribe and re-sync depth). Engine
  failover is **not** a close: the instance keeps serving and pushes a `resync` + snapshot (§8).

## 6. Payload catalogue (JSON)

All payloads are JSON **text** frames produced by the market-data system from decoded wire frames.
Prices/quantities are scale-18 `Fixed` rendered as decimal strings (the engine's JSON convention).
`E`/`T` are Unix-milliseconds **intake** timestamps carried in the log — never publish-time
clocks. Binance-named fields keep Binance semantics; extension fields are additive and ignorable
by Binance-shaped clients.

Extension fields used throughout: `L` (Raft log index), `S` (symbol id), `ts` (taker order
sequence), `ms` (maker order sequence), `qv` (gross quote notional).

### 6.1 `<symbol>@trade` — one fill (from `TRADE` records)

```json
{
  "e":"trade","E":1762300000123,"s":"eurusd","S":1,
  "t":0,"L":118,
  "p":"1.085","q":"400","qv":"434",
  "T":1762300000123,"m":false,
  "ts":1,"ms":0,
  "tf":"0.868","tfa":"USD","mf":"0.4","mfa":"EUR"
}
```

- `t` = ordinal of the fill inside its log entry; with `L` it is the unique, canonically ordered
  fill identity (`EventId`). Binance's single integer id does not exist in the engine.
- `m` = "buyer is the maker" (i.e. the aggressor sold), Binance semantics.
- `tf`/`tfa`/`mf`/`mfa` = taker/maker fee and fee asset for the fill (engine fee reporting).

### 6.2 `<symbol>@sweep` — one taker, all fills (P1); `@aggTrade` (P2)

```json
{
  "e":"sweep","E":1762300000123,"s":"eurusd","S":1,
  "u":135,"L":118,"T":1762300000123,"ts":1,"m":false,
  "fills":[{"t":0,"p":"1.085","q":"400","qv":"434","ms":0,
            "tf":"0.868","tfa":"USD","mf":"0.4","mfa":"EUR"}],
  "volume":"400","quoteVolume":"434"
}
```

One message per taker order is the engine's natural `TRADE` unit (uncapped fill list). Binance's
`aggTrade` (single price, summed quantity, first/last trade id) is derived in P2 by grouping a
sweep's fills by price: `a` = `taker_sequence + 1`, `f`/`l` = first/last ordinal, `p`/`q` = price
and summed quantity, with the extension `g` (group index) making `(a, g)` unique when one taker
sweeps several levels.

### 6.3 `<symbol>@bookTicker` — best bid/ask (from `BBO`)

```json
{"u":135,"s":"eurusd","S":1,"E":1762300000123,
 "b":"1.085","B":"800","a":"1.086","A":"500",
 "mid":"1.0855","spread":"0.001","last":"1.085"}
```

Binance spot shape (`u`,`s`,`b`,`B`,`a`,`A`) plus the engine's mid/spread/last-trade extensions.
`u` is the update id (§7.4). Pushed on every book-changing entry and once on subscribe.

### 6.4 `<symbol>@depth@<speed>` — diff depth (from `DEPTH_DELTA`, conflated)

```json
{"e":"depthUpdate","E":1762300000123,"s":"eurusd","S":1,
 "U":132,"u":135,
 "b":[["1.085","800"],["1.084","0"]],
 "a":[["1.086","500"]]}
```

- `U`/`u` = the update-id range covered (first/last `market_seq` of the window, §7.4).
- `b`/`a` = absolute visible quantities at the post-entry totals; `"0"` removes the level.
  Hidden iceberg reserve is never visible, as everywhere in the engine.
- Levels inside one message are ordered best-first; duplicate prices cannot occur.

### 6.5 Subscribe-time `depthSnapshot` (and P2 HTTP parity)

```json
{"e":"depthSnapshot","E":1762300000123,"s":"eurusd","S":1,
 "lastUpdateId":135,
 "bids":[["1.085","800","1"]],
 "asks":[["1.086","500","2"]]}
```

- `lastUpdateId` is the **last update id included** in the snapshot (§7.4); the third element per
  level is the order count (extension).
- Pushed on `@depth` subscribe, after a service gap, and after an engine failover reset.
- P2 adds `GET /marketdata/depth?levels=N` returning the same object for Binance-style REST+WS
  clients, served from the assembler cache (point-in-time consistent with its `lastUpdateId`).

### 6.6 `<symbol>@depthN@<speed>` — partial book depth (assembler cache)

```json
{"lastUpdateId":135,"bids":[["1.085","800","1"]],"asks":[["1.086","500","2"]]}
```

Binance partial-depth shape (no `e`/`E`), top N ∈ {5, 10, 20} per side, one frame per conflation
window with content as of the window's `u`.

### 6.7 `<symbol>@kline_<interval>` — OHLCV (from `BAR`)

```json
{"e":"kline","E":1762300000123,"s":"eurusd","S":1,
 "k":{"t":1762299600000,"T":1762299659999,"i":"1m",
      "o":"1.084","c":"1.085","h":"1.085","l":"1.084",
      "v":"400","n":1,"x":false,"q":"434","ts":1,"L":118}}
```

`t`/`T` = window start/end, `x` = bar closed (`BAR_CLOSE` vs `PARTIAL_BAR`), `n` = trade count,
`q` = summed quote notional (requires the P0 bar extension), `ts`/`L` = the electing taker's
sequence and log index. Updates are entry-driven (a trade that changes the bar), not on a timer.

### 6.8 `<symbol>@status` and `resync` (extensions)

```json
{"e":"status","E":1762300000123,"s":"eurusd","S":1,
 "lastUpdateId":135,"lastLogIndex":118,"lastOrdinal":0,
 "bookSequence":135,"pendingStops":0,"epoch":7,"leaderId":1,"ready":true}

{"e":"resync","s":"eurusd","reason":"gap","dropped":2}
```

`@status` is the deterministic cadence stream (2006) plus the idle heartbeat, with service
watermark fields; `resync` is a service-local notice sent on a gap or an engine reset, always
followed by a fresh `depthSnapshot` on depth subscriptions.

### 6.9 `<symbol>@lifecycle` (extension, ops)

One message per applied entry with the `LIFECYCLE` records (fill/rested/cancelled/triggered/
expired/rejected/amended, including fees and reasons). Intended for dashboards and debugging;
Binance has no equivalent, so it is namespaced as an extension and can be disabled by flag.

## 7. Order-book synchronization

### 7.1 Update ids

WS depth payloads use a 1-based **update id**: `updateId = market_seq + 1`. The engine's
`market_seq` is 0-based (first order is sequence 0) and advances by exactly one per mutating
entry; the P0 contiguity change (§4.2) guarantees one depth message per entry. Conflation turns
consecutive ids into inclusive `U`..`u` ranges, which is exactly Binance's interval semantics.

### 7.2 WS-only procedure (default, recommended)

1. Subscribe `<symbol>@depth@100ms`. The service immediately pushes `depthSnapshot` with
   `lastUpdateId` = the last id included in its cache.
2. Buffer any `depthUpdate` that arrives before the snapshot; then apply Binance rules 3–6
   verbatim:
   - drop `u <= lastUpdateId`;
   - the first applied message must satisfy `U <= lastUpdateId + 1 <= u`;
   - each subsequent message must have `U == previous u + 1`;
   - a jump (`U > previous u + 1`) or a `resync` notice means: drop the local book, wait for the
     next pushed snapshot (or re-subscribe) and start over.
3. Absolute quantities; `"0"` deletes a level.

### 7.3 REST + WS procedure (parity, P2)

Identical, but the snapshot comes from `GET /marketdata/depth?levels=N` before/after connecting,
as Binance's `api/v3/depth`. The assembler cache serves it, so the snapshot and the stream are
consistent by id, not by time.

### 7.4 Why `market_seq` is safe as the update id

- It is **replicated and snapshotted** (`OrderBook::sequence`), so it is identical on every node
  and survives restart; it never resets within a deployment.
- It advances by exactly one per mutating entry (rejections included), so the always-emit rule
  makes emitted ids **dense**: Binance's contiguity check is meaningful.
- `(log_index, ordinal)` stays available as `L`/`t` for audit-grade ordering; it is not used as
  the book update id because one entry can touch several levels.

## 8. Leadership, failover, reconnect

- **Engine leadership (leader-only publishing, the default):** the leader's sink publishes; the
  deposed leader closes its publications (its gate already closes on leadership loss). The new
  leader publishes a `RESET` `DEPTH_SNAPSHOT` at its boundary and continues the same `market_seq`
  space. The market-data system sees the epoch change + reset, rebuilds its caches and pushes a
  `resync` + `depthSnapshot` to depth subscribers — **clients keep their connections**; update ids
  stay monotone across the handover. A few in-flight frames from the deposed leader are committed
  data and dedupe by key (the Aeron design's §18 rule).
- **All-replicas publishing (`--market-data-leader-only=false`):** every node publishes identical
  frames on its own Aeron session; the assembler dedupes by
  `(symbol_id, stream_id, market_seq, sub_seq)` and failover is a non-event for clients.
- **Idle/health:** the heartbeat (§4.2) gives the assembler liveness and epoch/leader visibility
  without any Raft knowledge. A missing heartbeat beyond a configurable window makes the instance
  `/ready` false (so the load balancer drains it) and pushes a `resync` notice; when frames
  return, a snapshot re-syncs depth.
- **Market-data instance restart:** state is rebuilt from the next periodic snapshot + deltas; the
  load balancer only routes clients once `/ready` is true. Nothing on the engine side notices.
- **24-hour rotation:** every connection is closed with `1000` after 24 h, Binance-style; clients
  re-subscribe and re-sync. This bounds per-connection state growth and leaks.

## 9. Backpressure and slow consumers

Three layers, each with its own policy:

| Layer | Mechanism | Policy |
|---|---|---|
| Aeron transport | multicast flow control (Max strategy) + term retention (1–4 MiB per stream) | a briefly slow instance recovers via retransmit; an instance that falls beyond retention gets a loss signal → gap path |
| Aeron thread → assembler | bounded handoff queue (8192), `try_send` | drop + count + gap flag; depth re-syncs from the next periodic snapshot; latest-value streams continue |
| assembler → session | `broadcast` per stream key (1024) + per-session `mpsc` (1024) | `LatestValue` streams conflated by drain-to-latest; `Sequenced` streams never skip; `Lagged`/full queue → `resync` (depth) or close `1013` |

Message classes:

| Class | Streams | Why |
|---|---|---|
| **Latest-value** (conflatable) | `bookTicker`, `depthN`, `miniTicker`/`ticker` | only the newest frame matters; a burst collapses to one queue slot |
| **Sequenced** (must not drop silently) | `trade`, `sweep`, `kline`, `depth` diffs, `lifecycle`, `status` | dropping would corrupt a local book or miss a fill; overflow closes the connection |

The engine is insulated from all three: its publication retries under backpressure and at worst
the tap queue degrades to `GAP` while consensus continues (§4.2).

## 10. Limits and connection hygiene

| Limit | Default | Notes |
|---|---|---|
| Server ping interval | 180 s | Binance: 3 min |
| Pong timeout | 600 s | disconnect after 10 min without pong |
| Connection lifetime | 24 h | close 1000, Binance parity |
| Streams per connection | 1024 | Binance parity; covers several symbols on one socket |
| Connections per instance | 256 | plus per-IP connection throttling (300/5 min) at the gateway or, optionally, in-process |
| Inbound control messages | 10/s per connection | excess answered with error code 3 |
| Inbound frame size | 4 KiB | control frames only |
| Per-connection outbound FIFO | 1024 entries | overflow → close 1013 |
| Conflation speeds | 100 ms, 1000 ms | Binance spot speeds |

## 11. Security and trust model

Two trust boundaries, both matching the existing engine edge (AGENTS.md):

- **Engine ↔ market-data system (Aeron):** plaintext UDP on a trusted private network. Aeron has
  no TLS or authentication; the channel must not cross an untrusted network. This is the same
  model as the engine's HTTP/gRPC edge, which is deliberately internal behind a gateway.
- **Market-data system ↔ clients (WS/HTTP):** plaintext and unauthenticated by design; the
  supported deployment puts the gateway (TLS, client authentication, per-IP throttling, public
  shielding) in front. The WS surface is **read-only market data**: the only client→server text
  frames are the §5.3 control messages; nothing a client sends can reach the engine.
- Stream names and control payloads are strictly validated (length, charset, known names) before
  any allocation or lookup.
- `@lifecycle` exposes order metadata (sequences, prices, volumes, fees) exactly like the engine's
  `/events` API; `--no-lifecycle` hides the stream on a given instance.
- Each instance exposes counters (connections, messages sent, slow-consumer closes, resyncs,
  assembler gaps) at `GET /metrics`-style JSON for monitoring and abuse detection.

## 12. HTTP snapshot endpoint (P2, parity)

`GET /marketdata/depth?levels=N` (N ≤ 200) → the §6.5 object, served from the assembler cache.
Optional `GET /marketdata/time` → `{ "serverTime": <ms> }` for clock-skew checks. Both read-only,
same trust model. No other REST parity endpoints are planned (klines/tickers can be reconstructed
from streams; the durable source is the engine's replicated event log, not the feed).

## 13. Configuration

### 13.1 Engine flags (feature `market-data-aeron`)

| Flag | Default | Meaning |
|---|---|---|
| `--market-data-aeron-channel` | unset | enables the `AeronSink`; e.g. `aeron:udp?endpoint=239.1.2.3:24001\|interface=lo0` |
| `--market-data-aeron-stream-base` | 2001 | first stream id; the seven streams are base..base+6 |
| `--market-data-aeron-port-base` | `http_port + 3000` | per-stream endpoints follow the Aeron §15 convention |
| `--market-data-aeron-snapshot-every-ms` | 30000 | periodic reset-snapshot re-broadcast; 0 disables |
| `--market-data-aeron-heartbeat-ms` | 1000 | idle STATUS keepalive with a reserved `sub_seq`; 0 disables |
| existing `--market-data-*` | — | unchanged: depth, bars, status cadence, leader-only, JSONL log |

### 13.2 Market-data system flags

| Flag | Default | Meaning |
|---|---|---|
| `--addr` | `127.0.0.1:24700` | HTTP/WS listener |
| `--aeron-channel` | required | the engine channel to subscribe to; repeat per symbol |
| `--aeron-dir` | `AERON_DIR` | media-driver directory; repeat per symbol |
| `--aeron-stream-base` | 2001 | must match every engine |
| `--symbol` | required | `name:id:base:quote`, repeatable; the id is validated against frames |
| `--bar-intervals-ms` | `1000,60000` | must match the engines' bar intervals |
| `--max-connections` | 256 | per instance |
| `--max-streams` | 1024 | per connection |
| `--queue` | 1024 | per-connection sequenced FIFO |
| `--no-lifecycle` | off | hide the extension `@lifecycle` stream |
| `--ping-secs` | 180 | ping interval (pong timeout = 10 × ping) |
| `--pong-timeout-secs` | 600 | disconnect when no pong arrives within this window |
| `--heartbeat-timeout-secs` | 30 | mark a symbol not ready when no frame/heartbeat arrives |

## 14. Testing

| Layer | Tests |
|---|---|
| Engine: contiguity (P0) | every mutating entry yields exactly one depth message (empty allowed); rejections/parked stops produce the empty delta; two processors fed identical frames emit identical depth messages |
| Engine: Aeron adapter | `AeronSink` golden frames per stream via `wire::encode`; embedded driver (as U7 planned) publication/receipt; backpressure slows the processor and produces `GAP`, never an `apply` stall; periodic snapshot + heartbeat cadence; leadership gain/loss opens/closes publications |
| Engine: idle heartbeat | A reserved `HEARTBEAT_SUB_SEQ` STATUS is emitted only when idle and suppressed by any applied frame; the service treats a missing frame/heartbeat beyond `--heartbeat-timeout-secs` as not ready |
| Engine/service: stream 2010 | `MSRQ` v1 request round-trips through the engine subscription; the service sends one on an empty/stale depth cache or gap; the engine queues a requested reset snapshot |
| Service: `FrameSource` | `ChannelSource` drives every hub/protocol test with no media driver |
| Stream names | parse every catalogue name and speed; reject unknown pair/stream/interval/level/speed; `symbol_id` validation against frames |
| Control protocol | SUBSCRIBE/UNSUBSCRIBE/LIST round trips, id echo, error codes 0–4, rate limit, stream cap, malformed JSON, 4 KiB cap |
| Assembler | dedupe by key (duplicate frames, two Aeron sessions); snapshot reset; delta totals; watermark; gap → invalidate depth → re-sync on next snapshot; heartbeat timeout → not ready |
| Conflation | one window → one merged `U..u` message, last-write-wins levels; empty window → no message |
| Sync contract | snapshot + buffered diffs, Binance rules 3–6 as literal test cases, gap → resync + re-snapshot, empty-book snapshot (`lastUpdateId` 0) then first `U == 1` |
| Payloads | golden JSON for every stream (incl. fee fields, `Fixed` decimal strings); extension fields stable |
| Limits | ping/pong timeout, 24 h rotation, connection cap, FIFO overflow → 1013, latest-value conflation under burst |
| End-to-end | `demos/market_data_system/tests/ws.rs` drives snapshots, conflation, combined streams, control errors, two symbols held on one connection and a symbol switch on the same socket; `tests/aeron.rs` (feature `aeron`) publishes engine wire frames over IPC to a `tokio-tungstenite` client. A full place/fill/amend/cancel + forced-gap + leadership-change scenario is still open |
| Bench | engine: tap/publication cost with the feed on/off (gates "matching does not regress"); service: `ws_fanout` — N connections × `bookTicker`/`depth@100ms`, serialize-once reuse, per-connection send cost, assembler memory |

## 15. Performance budget

- **Engine:** publication cost is one `wire::encode` + one Aeron offer per stream message on the
  existing processor task — the same order as today's JSONL sink, with no fsync. The tap/journal
  cost is already gated by `cargo bench --bench matchengine`.
- **Aeron hop:** one UDP multicast per stream message; sub-millisecond on a LAN. The publication
  thread retries under backpressure without touching `apply`.
- **Service:** serialize once per stream message (`Bytes` shared); fan-out cost is a `broadcast`
  send plus per-connection socket writes. Target: 256 connections × `bookTicker` +
  `depth@100ms` on a busy book (≥ 1k updates/s) within one core.
- **Fleet:** capacity is linear in instance count (§4.13) — N instances ≈ N ×
  `--max-connections` clients, while the engine's publish cost and each instance's upstream
  cost stay constant. These figures are budgets to be replaced by measured numbers under the
  `docs/PERFORMANCE.md` discipline once the service exists.
- **Memory:** assembler state ≈ one depth map (levels × ~32 B) + bars + dedupe ring; per
  connection `queue × pointer` + shared payload bytes. Budget ≤ a few MiB per instance at the
  default limits.

## 16. Implementation plan

These phases are tracked as work units **U9–U15** in
[`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md), which maps each unit to the function points below.

### P0 — engine feed contiguity + sink seam (small)

1. Processor emits one depth delta per mutating entry (empty level list allowed); update
   `MARKET_DATA_PLAN.md` §15 wording and golden tests.
2. `MarketDataHandle::start_with_sink(config, sink)` so custom sinks (Aeron, tests) can be
   installed; the JSONL sink remains.
3. Bar records gain summed quote notional (`q`) for the kline stream.

**Acceptance:** existing feed tests pass with per-entry depth messages; two processors fed
identical frames still emit identical messages; no matching-path change.

### P1a — engine Aeron publication (U7)

1. `market-data-aeron` feature: `rusteron-client` + `rusteron-media-driver` deps (default off).
2. `src/market_data/aeron.rs`: `AeronSink` with one exclusive publication per stream, lazy create
   on leadership gain / close on loss, a dedicated publication thread owning the `!Send` handles,
   `try_offer` retry under backpressure, gap counters.
3. Periodic reset-snapshot re-broadcast and the idle heartbeat (§4.2), both flag-gated.
4. Embedded-driver tests: publish/receive per stream, backpressure → `GAP`, leadership lifecycle.

**Acceptance:** a subscriber decodes every golden frame; a stalled subscriber never stalls
`apply` (bench-verified); the feed suite stays green without the feature enabled.

### P1b — market-data system crate

1. `marketdata/` crate: CLI, `FrameSource` (Aeron + channel impls), assembler with dedupe/gap
   policy, watch caches, conflation, registry.
2. `actix-web`/`actix-ws` listener: `/ws`, `/ws/<stream>`, `/stream?streams=…`, control protocol,
   snapshot-on-subscribe, limits, ping/pong, 24 h rotation, close codes; `/health`, `/ready`.
3. Streams: `@trade`, `@sweep`, `@bookTicker`, `@depth@speed`, `@depth5/10/20@speed`,
   `@kline_<interval>`, `@status`, `@lifecycle`.
4. Tests: the §14 service rows driven by `ChannelSource`, plus the end-to-end embedded-driver
   integration with a real WS client; CI gains the crate's fmt/clippy/test jobs (as `demos/order_system/` has).

**Acceptance:** a browser-style client maintains a correct local book through
place/fill/amend/cancel, a forced gap and an engine leadership change, using only Binance rules
3–6, while the client's connection stays up across the failover. Two instances fed by the same
engine serve identical stream content (deterministic payloads; only conflation grouping may
differ) behind one load balancer — multi-instance fleets work from day one (§4.13).

### P2 — parity and resilience

1. `@aggTrade` (price-grouped sweeps), `@miniTicker`/`@ticker` (service-local 24 h rolling
   window, seeded from bars and fed by trades).
2. `GET /marketdata/depth` snapshot endpoint (REST+WS sync parity).
3. Per-IP connection throttling in-process (option); operational runbook (driver, channel,
   flow-control strategy, retention sizing).

### P3 — multi-symbol and WAN

1. One instance subscribing to several engine deployments' channels, routing by `symbol_id`
   (stream names already carry the pair).
2. Fleet operations at scale (§4.13): autoscaling policies, per-DC relays, and the optional
   Kafka/NATS bridge variant of the assembler for WAN distribution (the Aeron design's §19
   bridge).

## 17. Alternatives considered

| Alternative | Why not |
|---|---|
| **Engine serves WebSocket directly** (a WS hub as an in-process `MarketDataSink`) | couples client load, limits and upgrades to the matching engine; every client cost (connection, slow consumer, TLS edge) lands in the Raft node's process; the two-system split keeps the engine publish-only and lets the client tier scale independently |
| SSE (server-sent events) | no client→server subscribe channel, no combined streams, no Binance-shape parity; weaker reconnect semantics |
| gRPC server streaming (the engine already has tonic) | not browser-native, no Binance-style naming; fine for internal relays, not the public reference protocol |
| Aeron directly to clients | needs a media driver on every client; rejected in the Aeron assessment for the same reason |
| Kafka/MQTT to browsers | an extra broker inside the trust boundary; WS is the standard browser ingress (a Kafka bridge remains a P3 WAN option) |
| Polling `POST /events` | the audit-grade match-event log (cursor pages), not conflatable book/BBO/depth products, and polling does not scale for market data |
| Long-poll/REST only | rate-limited snapshots, no push; explicitly not the ask |

## 18. Risks and open questions

- **Multicast availability.** Cloud VPCs often disable multicast (the Aeron design already warns
  of this). Fallbacks — N unicast publications, or a single relay instance that others chain from
  — change nothing for the market-data system, only the engine's channel configuration.
- **Media driver operations.** Aeron needs a media driver per host (embedded in tests, a
  supervised process in production). This is U7's operational decision, now forced by this design;
  the feature flag keeps it opt-in.
- **rusteron maturity and `!Send`.** Handles are confined to dedicated threads on both sides; the
  `FrameSource`/`start_with_sink` seams keep everything else testable without Aeron. MSRV 1.89
  must hold for the feature-gated deps.
- **Flow-control strategy.** The default multicast Max strategy lets slow instances fall behind
  (then re-sync from a periodic snapshot); a Min strategy would stall the publisher for everyone
  and must not be used for market data.
- **Conflation is instance-local.** Two instances may group the same content differently; clients
  key on `U`/`u` and never on cadence, but multi-instance deployments should document it.
- **24 h ticker/miniTicker is service-local state.** After an instance restart the rolling window
  is rebuilt from bars + live trades; the first minutes are approximate. Acceptable for ticker
  semantics; the durable truth stays in the engine's event log.
- **Symbol name/id are configuration, not wire data.** Each `--symbol name:id:base:quote` must
  match its engine's symbol; a mismatch is detected (frames rejected) rather than silent.
- **Slow-consumer close (1013) vs Binance's silent 24 h rotate:** closing on overflow is more
  aggressive but honest; "conflate everything" would corrupt local books.
- **Resolved (2026-11-10):** late joiners can request a snapshot on stream 2010; the periodic
  broadcast remains the fallback. The remaining open questions are default diff-depth speed
  (100 ms vs 1000 ms), whether `@lifecycle` should be opt-in, whether the periodic snapshot cadence
  should adapt to trading activity, and binary (SBE-like / wire v7) WS frames for heavy consumers.
