# Historical bar generation and query system

Status: **phase P0 implemented in `market_data_system/`; P1–P5 remain design.** This document
specifies the historical path added to the existing `market_data_system`: it turns the engine's
deterministic trade stream into dense, durable, queryable klines and serves them through the same
process and Binance-compatible API — with the guarantees Binance does not provide (a mutation
sequence, gap repair, explicit revisions, and one store behind both history and replay).

P0 implements the TDengine schema, the trade-derived `1s`/`1m`/`5m` writer, exact-decimal text I/O,
`GET /klines` (including read-time `INTERVAL` rollups for `3m`…`3d`), and WS subscribe replay
seeded from TDengine. Targets that are whole multiples of 5 minutes roll up from `bars_5m` when it
has data (`30m` → `5m` → `1m`), and fall back to `bars_1m` on older deployments.

**Lab deployment ownership (2026-11-25):** the global terminal service is a **read-only querier**
(`--history-read-only`): it serves `/klines` from TDengine but never taps trades or writes rows.
Each per-symbol fast `MarketDataService` owns the writer for the symbols it receives, so a bar
series is materialized by the deterministic owner of that feed rather than once per client-facing
replica. Upserts are keyed by `(symbol, ts)`, so overlapping lifecycle-test symbols remain
idempotent; the single-Lease writer/failover contract in §4.2 remains the production target.

Deliberately deferred: the trade spool and Lease failover, `bar_gaps` / `bar_revisions` /
`bar_imports`, `GET /bars`, the archive/REST importers, time-zone and `1w`/`1M` windows, and the
three-dnode deployment. The sections below describe the complete target; §17 marks which phase each
piece belongs to.

The store is **TDengine**. Bars are written as exact `DECIMAL(38,18)` rows and read back as exact
decimal text; no value in the bar path passes through a binary float or a 96-bit client decimal
(§7.2). The measured ingest, query, rollup and precision results are in §12.

It builds directly on the API investigation in
[`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII, which measured the
reference behaviour and its quirks.

Last updated: 2026-11-23.

Related documents: [`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII
(the Binance contract and its gaps), [`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md) (the engine
feed this consumes), [`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md) (the market-data
service and its subscribe-time replay), [`MATCH_RESULTS.md`](./MATCH_RESULTS.md) (the replicated
event log used for hole detection), [`DECIMAL.md`](./DECIMAL.md) (scale-18 `Fixed`),
[`STORAGE.md`](./STORAGE.md) (the engine's own WAL/snapshot conventions, which bars no longer
reuse), [`AGENTS.md`](../AGENTS.md).

## 1. Summary

```text
 writer (1 active, Lease-guarded)                    TDengine (3 dnodes, REPLICA 3)
 ┌─────────────────────────────────────────┐        ┌─────────────────────────────────────┐
 │ feed tap ─► builder ─► 1s/1m/5m rows ───┼─upsert►│ bars_1s bars_1m bars_5m bar_state │
 │      │          │                       │        │ bar_gaps  bar_revisions  bar_imports│
 │      │          └─► WS hub (live)       │        └─────────────────────────────────────┘
 │      └─► trade spool (local, 7 d)       │                        ▲
 │ engine /events ─► hole check            │                        │ SELECT … CAST … INTERVAL
 └─────────────────────────────────────────┘                        │
                                                                    │
 readers (N, stateless) ── GET /klines, GET /bars, WS snapshot ─────┘
```

Eight decisions carry the design:

1. **Reuse the market-data service, do not fork it.** The historical path is added to
   `market_data_system`: feed sources, symbol catalogue, protocol payloads, session/control
   protocol, conflation, `/info`, `/ready`, `/metrics` and the archive/replay sources are the
   existing code. Only the bar builder, the TDengine writer and the range query are new.

2. **Bars are a derived view of trades, never the source of truth.** The durable fact is the fill;
   a bar is a deterministic aggregation of fills over a window. That makes every interval exact,
   makes revisions tractable (re-derive one window), and removes the dependency on the engine's
   `Bar` emission, which currently suppresses each window's first emit
   ([`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII §5, `TODO.md` P2 #11).

3. **Sparse at rest, dense at the edge.** Only windows that actually traded are stored. Empty
   intervals are synthesised on read by carrying the previous close forward — exactly the behaviour
   measured in Binance's archive (4.9% of a sampled `1s` day were flat carry-forward bars). This
   keeps "no trade here" as a fact instead of a million identical zero-volume rows, while the wire
   contract stays Binance-dense.

4. **TDengine is the system of record.** There is no custom segment format, no SQLite catalog, no
   manifest, no open-day checkpoint and no reader cache. The writer upserts bars into TDengine;
   every reader queries TDengine. That removes the entire shared-object-store tier and the
   bootstrap gap that came with it: a reader that starts at 14:00 reads today's morning from the
   same table the writer has been filling since midnight.

5. **Exact decimals end to end.** The engine uses scale-18 `Fixed` (`i128`); TDengine stores
   `DECIMAL(38,18)` exactly; the write path emits canonical 18-decimal literals; the read path
   casts to `VARCHAR` and parses with the same strict scale-18 parser. Neither `f64` nor
   `rust_decimal` (96-bit, 28-29 digits) is allowed to decode a bar value. §7.2 states the
   contract and the failure it prevents.

6. **A mutation sequence, so a client can prove it missed nothing.** Binance klines carry no
   sequence; a dropped frame is undetectable
   ([`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII §1). Every bar
   carries `seq` = the engine `market_seq` that last changed it, and `rev`; `GET /bars?since=`
   repairs a gap deterministically.

7. **Sealed means sealed; corrections are audited revisions.** A bar becomes readable as final only
   after the event-time watermark passes its window end. A later correction is an audited upsert
   that increments `rev`, never a silent overwrite — the opposite of the archive's undocumented file
   replacement.

8. **One writer, many stateless readers.** Exactly one process holds the Lease and writes. Readers
   hold no bar state, mount no volume and cache nothing: TDengine is the cache. Readers scale with
   an HPA, and a reader restart is a reconnect, not a hydration. The writer/querier failure and
   recovery contract is spelled out in §4.2.1.

## 2. What we keep from the Binance contract, and what we fix

The wire shape is Binance-compatible so existing clients, chart libraries and replay fixtures work
unchanged. The additions are backward-compatible fields and endpoints.

| Binance behaviour (measured or documented) | This design | Why |
|---|---|---|
| Kline frames have no sequence number; a dropped frame looks normal | `seq` on every frame = the engine `market_seq` that produced the update; `GET /bars?since=<seq>` repairs; `410` with the repair floor when the cursor is too old | Gap detection and repair become possible |
| The open bar is re-pushed on a 1-2 s timer; quiet markets still emit | Same cadence, plus an explicit `seq` per push | Clients can distinguish "no change" from "no frame" |
| Empty intervals are dense flat bars carrying the previous close | Same output, produced at read time from sparse storage; the native shape marks `synthetic: true` | Contract fidelity without storing noise |
| `x` is the only finality signal; `closed` can be missed | `x` plus `rev` plus an `X-Bar-Sealed-Through` header on every response | A client can verify it is reading final data |
| Prices and volumes are JSON strings with 8 decimals, parsed by whatever the client uses | Canonical 18-decimal strings end to end; the store, the read path and the wire agree bit for bit | No silent rounding between the engine and the chart |
| History is split across REST (1000-bar cap), `uiKlines`, the WS API and the archive, with different quirks | One store (TDengine), one `GET /klines`, WS replay, and one importer; `limit` stays 1000 for compatibility, with a native `since`/`limit` extension for internal clients | One contract, no per-source special cases |
| Archive files are replaced silently when wrong | `bar_revisions` row with both values, source and time; `rev` increments | Auditable corrections |
| `f`/`L` are exchange trade ids with no defined durability | `f`/`L` are our deterministic `(log_index, ordinal)` trade ids in the native shape (the Binance shape renders the log index), so every bar is traceable to the replicated log | Bar ↔ trade ↔ log replay is exact |
| `timeZone` accepted on REST only; offset streams on WS | `timeZone` on REST and `@+HH:MM` on WS, both anchored by the same window function, timestamps always UTC | One anchoring implementation |
| No `1s` history in the live API before the interval existed | Imported archive + derived live bars cover the same key space; the store does not care where a bar came from | Uniform depth |

## 3. Goals and non-goals

### Goals

- G1. **Exact, deterministic bars.** For a given trade set and interval, the resulting bar is
  bit-identical on every replica and after any replay. No wall clock in the aggregation path, and
  no lossy numeric conversion anywhere between the engine and the wire.
- G2. **Dense, Binance-shaped reads.** `GET /klines` returns the twelve-element array with the same
  defaults (500) and cap (1000), including a flat bar for every empty interval in the requested
  range, and the current open bar with `x=false`.
- G3. **Durable, bounded storage with HA.** A symbol-year of `1s` fits in the low gigabytes;
  TDengine keeps three replicas; a restart resumes from a persisted trade cursor; a revision is an
  audited upsert.
- G4. **Provable completeness.** A live consumer can detect and repair a missed frame; a batch
  consumer can verify a range is sealed; a query that spans a known ingestion gap returns `409`
  with the gap rather than a silently flat stretch.
- G5. **Bulk seeding from `data.binance.vision`** and tail correction from REST, both idempotent
  and both funnelled through one merge rule.
- G6. **No matching-path impact.** The service consumes the asynchronous feed; a slow or dead
  historical writer must never back-pressure `apply`.

### Non-goals

- N1. Order-book reconstruction, tick data serving, or depth history. (The trade spool is a
  catch-up and revision artifact; no tick API is exposed. The engine's `--match-event-log` and
  `POST /events` remain the debug and event paths.)
- N2. Balances, settlement or fee accounting; fees travel in trades but are not aggregated here.
- N3. Multi-venue consolidation or symbol mapping.
- N4. Sub-second intervals other than `1s`.
- N5. A general time-series database. TDengine is used as a store with a fixed schema; no arbitrary
  SQL, joins or downsampling policies beyond the interval set are exposed.
- N6. Modifying the engine's replicated state or wire format. This is a consumer.

## 4. Architecture

### 4.1 Deployment view

The historical path is **added to `market_data_system`**, not built as a second service. The
existing process already owns the feed sources, the symbol catalogue, the Binance protocol codecs,
the session/control protocol, the hub, `/info`, `/ready` and `/metrics`. A separate process would
duplicate all of it and become a second, subtly different view of the same market — exactly the
failure mode the one-store rule exists to prevent.

| Layer | Reused as-is | Added |
|---|---|---|
| Ingest | `--nats-*`, `--aeron-*`, `--replay-dir`, `--archive-dir`, the `source/` decoders | a trade tap into the builder |
| Config | `--symbol name:id:base:quote`, interval set, `/ready` semantics | `--tdengine-*`, `--seal-lag-ms`, `--spool-days`, `--lease-name` |
| Encoding | `protocol.rs` kline payload, scale-18 `Fixed` rendering | dense fill, `limit`/`timeZone`, `seq`/`rev` |
| Serving | `/ws`, `/ws/<stream>`, `/stream`, `/info`, `/ready`, `/metrics`, conflation, 1024-stream cap | `GET /klines`, `GET /bars`, snapshot from TDengine |
| Ingest jobs | `--archive-dir` / `--replay-dir` sources | `data.binance.vision` importer, REST tail |
| Storage | — | TDengine schema, batched upserts, revision/gap tables, local trade spool |
| Coordination | — | Lease-guarded single writer, stateless readers |

| Environment | Topology |
|---|---|
| Local / OrbStack demo | One single-dnode TDengine container (a planned `k8s_control_plane/deploy/tdengine.yaml`), one `--role=writer` pod, one or more `--role=reader` pods |
| Kubernetes (phase 1) | TDengine 3 dnodes + 3 mnodes with `REPLICA 3`; writer `Deployment` (1 replica, Lease, small spool PVC); reader `Deployment` (N, no volume); one `Service` fronts writer and readers |
| Kubernetes (phase 3) | Readers scale with an HPA; TDengine scales by adding dnodes/vgroups; no object store on the read path |
| Air-gapped / CI | A throwaway TDengine container; `--role=writer --import-dir` seeds the store; readers run against the same DSN |

**Why extend rather than fork.** Three reasons, in order: the feed decoders, symbol config and
Binance payload builders already exist and are the hard part to get right; two processes consuming
the same feed would drift in their notion of "now", ordering and bar identity; and the operator
already renders, rolls and health-checks this workload, so history inherits that machinery instead
of adding a second one to operate.

### 4.2 One writer, many readers

Exactly one process may write bars at a time. The writer holds a Kubernetes `Lease`
(`historical-bars/<cell>`) and renews it every 5 s with a 15 s TTL. This is the same pattern the
control-plane operator already uses. The writer Deployment runs **one** replica, because its spool
PVC is `ReadWriteOnce`: a second pod cannot mount it while the first is alive. "Failover" therefore
means "the replacement pod that is created after the old one terminates", not a hot standby.

Failover safety comes from the bar row's own cursor, not from the Lease and not from the spool:

- Every bar row carries `last_log`/`last_ord` — the trade position it reflects — and `bar_state`
  carries the same cursor plus the watermark. Both are updated on every cadence, so the newest row
  for each interval is a self-describing checkpoint.
- A replacement writer reads the newest row for each interval (`ORDER BY ts DESC LIMIT 1`) and
  resumes at the next trade. It does not need the old pod's spool: a bar exists only after its
  trades were applied, so the row already contains them. At most one cadence of unsealed data can
  be re-applied, and re-applying a trade the row already covers is a no-op because the row's
  `last_log` is `>=` that trade.
- A **same-pod** restart (the PVC survived) additionally replays the spool from the cursor (§4.5),
  which is what makes window re-derivation and revision checks cheap. A replacement pod starts with
  an empty spool, so any revision it cannot re-derive from its own spool falls back to the archive
  or REST (§9, §10) and is marked `source=rest` until reconciled.
- Upserts are idempotent by key: re-writing identical values rewrites the same row.

The Lease is advisory, so treat a partitioned old writer as possible. It checks the Lease before
every flush and stops renewing on failure; a zombie that keeps writing for a few seconds can only
rewrite the **open** row with an older (but self-consistent) value, which the live writer's next
cadence overwrites with a newer one. It cannot corrupt a sealed row: its cursor is behind, so the
only window it can seal is one the new writer has already sealed with the same trade prefix and
therefore the same values. The result is a visible-but-transient flap in the open bar, not a lost
trade.

Readers are **stateless**. They hold no bar state, mount no volume, take no Lease and cache
nothing. A reader restart is a reconnect; a reader that joins mid-day queries TDengine and is
immediately complete (§4.8). This is the main operational simplification over the segment-store
design: there is no reader cache to warm, no manifest to poll and no checkpoint to hydrate.

#### 4.2.1 High availability contract — writer vs querier

The two roles fail differently and recover differently. TDengine (`REPLICA 3`, three dnodes and
mnodes, each vgroup a Raft group) is the shared HA substrate for both; the contract above it is:

| | **Writer** (one active) | **Querier / reader** (N stateless) |
|---|---|---|
| Detection | Lease renewal failure (5 s renew / 15 s TTL), `bar_state.writer_epoch` change, spool open failure, TDengine write timeout | `/ready` probes, TDengine query timeout, feed freshness (`--feed-timeout-secs`) |
| Recovery | Replacement pod resumes from the newest bar row's `last_log`/`last_ord`; a same-pod restart also replays the spool; upserts are idempotent | Reconnect, re-verify schema, query TDengine, join snapshot to buffered live frames (§4.8) |
| RTO | Lease TTL + pod scheduling; no hot standby (the spool PVC is `ReadWriteOnce`) | One pod replacement or an HPA scale-out; new pods stay `503` until the snapshot is in hand |
| Data safety | Every bar row is its own checkpoint; sealed rows are immutable; a revision is audited in `bar_revisions` | No local state to lose; no cache to invalidate; TDengine is the single source of truth |
| Degraded mode | DB down → keep spooling trades, retry with backoff, `/ready=false`; DB up but Lease lost → stop before the next flush | DB down → `503` with `Retry-After`, never a partial array; existing WS frames may finish the current window; new subscribes refused |
| Independence | A reader crash never affects the writer | A reader never needs the writer to be up to answer a query (§4.8.1) |

The deliberate non-goals matter as much as the guarantees: there is **no hot writer standby**, so
writer RTO is scheduling time rather than sub-second; there is **no reader-side cache**, so a reader
cannot serve stale history after the writer moves; and a network-partitioned writer is assumed
possible, so the Lease is checked before every flush and the worst overlap is a transient flap of
the open row (the live writer's next cadence overwrites it). See §13 for the full failure table.

### 4.3 Components

| Component | Responsibility | Restart behaviour |
|---|---|---|
| **Feed tap** | Decode `MarketMessage` frames from NATS, Aeron or a JSONL replay; filter to `Trade` bodies and the symbol's `Status` heartbeat; retain `market_seq` | Reconnects with backoff; freshness drives `/ready` |
| **Trade spool** | Append every consumed `TradeRecord` plus the envelope's `market_seq` (`ts_ms` is on the trade) to a rolling per-symbol file, fsync on a 1 s cadence | The restart and revision source; see §4.5 |
| **Hole checker** | Compare the spool's `last_trade` with the engine's `/events` cursor to detect a gap the spool cannot fill | Runs at startup and every 60 s; ids only |
| **Bar builder** | Per `(symbol, interval)`: fold trades into the open `1s`, `1m` and `5m` windows, maintain cumulative fields, emit on cadence | Rebuilt from `bar_state` + spool on start |
| **Cadence ticker** | Re-publishes the open bar every `min(interval, 2 s)` and evaluates the close watermark | Stateless |
| **Sealer** | Freezes a completed window, asserts invariants, re-derives `1m` from `1s` and `5m` from `1m`, writes `sealed=true` | Idempotent; re-seals produce identical rows |
| **TDengine writer** | Batch upserts per `(symbol, interval)` subtable; retries idempotently; records errors | Reconnects with backoff; a failed batch is retried |
| **Schema manager** | Creates databases/stables idempotently, verifies `schema_version`, checks `KEEP`/`REPLICA` | Refuses to start on a version mismatch |
| **State tables** | `bar_state` (watermark, cursor, revision), `bar_gaps` (known holes), `bar_revisions` (audit), `bar_imports` (file ledger) | Upserts; append-only for gaps/revisions/imports |
| **Query API** | `GET /klines`, `GET /bars`, `/info`, `/health`, `/ready`, `/metrics` | Stateless; runs in reader and writer roles |
| **WS hub** | Subscribe-time snapshot from TDengine, live fan-out, per-connection conflation, 1024-stream cap | Stateless |
| **Archive importer** | Enumerate, download, verify, decode, upsert `data.binance.vision` files | Resumable per file by content hash |
| **Reconciler** | Compare sealed bars against a second source (archive/REST), record revisions | Scheduled; read-only against the store except for revisions |

### 4.4 Task and channel topology

```text
tokio runtime
├── feed task            (1 per source)        ──mpsc(Trade, 8192)──►  spool writer
├── spool replay task    (on demand)           ──mpsc(Trade)───────►  builder task (1 per symbol)
├── builder task         (1 per symbol)        ──mpsc(BarUpdate)───►  ┌─────┴─────────┐
│                                                                     ▼               ▼
├── hub task             (bounded broadcast)  ◄──BarUpdate────  open bars       TDengine writer
├── cadence task         (1 per symbol)       ──tick───────────►   builder           │
├── TDengine writer      (1 task)             ──batched upserts─────────────────────► TDengine
└── HTTP/WS workers      (actix/tokio)        ◄──reads── TDengine (pooled connections)
```

Rules that mirror `docs/MARKET_DATA_WEBSOCKET.md` §4.12: nothing on the feed task blocks; the
builder never allocates per trade; the DB writer batches and retries on its own task; a full hub
queue drops the frame and increments `bar_frames_dropped` (the `seq` gap makes that observable and
repairable, which is the point). A slow TDengine never stalls the feed: the builder hands rows to a
bounded queue, and if the queue fills the writer keeps spooling trades and raises
`bar_db_queue_full_total`; the bars are re-derived from the spool once the DB recovers.

### 4.5 Restart and catch-up: a local spool, not `/events`

The engine's replicated event log looks like the obvious catch-up source, but it cannot place a
trade in time: `EventsResponse` returns `MatchEvent`s with `event_id`, price and volume but **no
timestamp** — event time lives in the applied entry, and the event API does not expose it
(`src/network/api.rs`). A builder that replayed `/events` alone could not answer "which window does
this fill belong to".

So the writer keeps its own **trade spool**: every consumed `TradeRecord` — which carries `ts_ms`,
`(log_index, ordinal)`, price, volume, quote volume and taker side — is appended to
`spool/<symbol>/<YYYY-MM-DD>.trades` together with the enclosing `MarketMessage.market_seq` (the
trade body itself does not carry it), fsynced on a short cadence and retained for `--spool-days`
(default 7). One file serves three needs:

1. **Restart.** Read the newest bar row's `last_log`/`last_ord` (or `bar_state`), replay the spool
   from there while it is available, then attach to the live feed. Exact by construction, because
   the spool *is* the trade stream; when it is not available (a replacement pod), the row's own
   cursor is still a correct checkpoint (§4.2).
2. **Revisions.** Re-deriving a sealed window (§6.4) reads the spool instead of asking the engine.
3. **Audit.** A disputed bar can be recomputed from the exact inputs that produced it.

`/events` stays useful for what it does expose: **hole detection**. If the spool's last trade id
does not line up with the event log's `last_event_id`, the writer knows it missed frames and raises
`bar_spool_gap_total`. When the gap is older than the spool, or the spool is lost, the window is
recovered from REST/archive with `source=rest`, reconciled later, and recorded in `bar_gaps` until
it is repaired — the timestamp always comes from a data source, never from the wall clock.

**Why the spool is not a TDengine table.** TDengine keys a row by its timestamp within a subtable.
The engine stamps a taker order in milliseconds, so many fills share the same microsecond; they
would overwrite each other in a `trades` table. A synthetic unique timestamp would break the
`KEEP`/partitioning semantics that make TDengine cheap. The spool therefore stays a local file on
the writer's small PVC, exactly as it was before. If the engine ever exposes the applied-entry
timestamp and `(log_index, ordinal)` through a `/trades` endpoint (open question 2), the spool can
be dropped and a cold writer can replay from any node.

This also settles a subtle boundary: bars are never stamped with ingest time. A trade's window
comes from the entry timestamp that travelled in the log and is preserved in `TradeRecord.ts_ms`,
which the market-data feed already derives from the order's intake stamp rather than
`SystemTime::now()` at apply.

### 4.6 Module layout and configuration

The code lands inside the existing crate; there is no new `Cargo.toml`.

```text
market_data_system/src/
├── main.rs, config.rs            # existing; new flags only
├── source/{mod,nats,aeron,jsonl}.rs   # existing feed decoders, reused as-is
├── feed.rs, session.rs, server.rs, protocol.rs   # existing; query routes + kline payload reused
├── bars/
│   ├── mod.rs                    # window aggregation for 1s, 1m and 5m
│   ├── ring.rs                   # open bars + live WS push buffer
│   └── seal.rs                   # watermark, invariants, sealed upsert
├── store/
│   ├── mod.rs                    # connect/pool, schema manager, retry loop
│   ├── schema.rs                 # DDL, schema_version, KEEP/REPLICA checks
│   ├── write.rs                  # batched canonical-decimal INSERT/upsert
│   ├── query.rs                  # SELECT … CAST … INTERVAL, densification
│   └── state.rs                  # bar_state, bar_gaps, bar_revisions, bar_imports
├── spool.rs                      # rolling timestamped trade spool
├── history/
│   ├── query.rs                  # range resolution + densification
│   ├── rest.rs                   # GET /klines, GET /bars
│   └── import/{archive,rest}.rs
└── reconcile.rs
```

| Flag | Default | Status | Purpose |
|---|---|---|---|
| `--symbol <name>:<id>:<base>:<quote>` | required, repeatable | existing | catalogue, unchanged |
| `--nats-url`, `--nats-subject-prefix`, `--nats-stream-base` | — | existing | live source (Profile N) |
| `--aeron-channel`, `--aeron-dir` | — | existing | live source (feature `aeron`) |
| `--replay-dir`, `--archive-dir` | — | existing | offline replay and recording |
| `--role` | `writer` | new | `writer` (seal + serve) or `reader` (serve only) |
| `--tdengine-url` | `ws://127.0.0.1:6041` | new | `taosAdapter` WebSocket endpoint |
| `--tdengine-user`, `--tdengine-password` | `root` / env | new | credentials; password never logged |
| `--tdengine-database` | `market_bars` | new | database name |
| `--tdengine-replica` | `1` | new | `REPLICA` on auto-create; `3` in production |
| `--tdengine-vgroups` | `1` | new | `VGROUPS` on auto-create; sized per host in production |
| `--tdengine-keep-days` | `3650` | new | `KEEP` on auto-create; must cover imports |
| `--tdengine-auto-create` | `true` in the lab | new | create database/stables if absent; `false` in production |
| `--tdengine-batch-rows` | `2000` | new | rows per subtable statement |
| `--tdengine-retry-ms` | `500` | new | reconnect/retry backoff, capped at 30 s |
| `--tdengine-query-timeout-ms` | `5000` | new | per-query deadline |
| `--seal-lag-ms` | 2×interval, capped at 60 s | new | watermark lag before a window closes |
| `--spool-days` | 7 | new | raw trade retention (restart + revisions) |
| `--spool-dir` | `/var/lib/market-data-spool` | new | writer's local spool volume |
| `--lease-name` | `historical-bars/<cell>` | new | single-writer coordination |
| `--replay-bars` | 500 | new | WS snapshot depth |
| `--repair-window-secs` | 3600 | new | how far back `/bars?since=` can repair |
| `--max-seal-lag-secs` | 120 | new | `/ready` seal-lag bound |
| `--bootstrap-buffer-bytes` | 64 MiB | new | trades buffered while a subscriber's snapshot query runs |
| `--feed-timeout-secs` | 30 | existing | feed freshness bound for `/ready` |
| `--derive-on-read` | off | new | aggregate from `bars_1m` instead of `bars_5m`; repair/backfill check only |
| `--rest-budget-weight-per-min` | 750 | new | tail backfill ceiling |
| `--import-dir`, `--import-symbol`, `--import-interval` | — | new | offline archive seed |

Removed from the earlier segment-store design: `--bars-dir` (bar data), `--segment-store`,
`--segment-origin`, `--manifest-poll-secs`, `--checkpoint-*`, `--cache-bytes`, `--warm-days`.
The only writer-local path left is `--spool-dir`.

### 4.7 TDengine as the shared store

#### 4.7.1 What it replaces

| Earlier component | Now |
|---|---|
| Day segment files (`bars/<symbol>/<interval>/<day>.seg`) | `bars_1s` / `bars_1m` / `bars_5m` rows, keyed by `(symbol, ts)` |
| Segment `.sha256` + CRC | TDengine block checksums + three replicas |
| SQLite `catalog.sqlite` | `bar_state`, `bar_gaps`, `bar_imports` |
| Shared manifest object | `bar_state` row, read with `LAST_ROW` |
| Open-day checkpoint objects | the open row itself, upserted each cadence |
| Reader content-addressed cache + mmap | no reader cache; TDengine page cache and `CACHEMODEL 'last_row'` |
| `SegmentStore` (`s3` / `origin-http` / `local`) | the TDengine client |
| Object-store retention / cold tier | `KEEP`; object storage only for backup/export |

The consequence is that the deployment loses an entire tier: no MinIO/S3, no manifest polling, no
open-day object fan-out, no reader disk, no `origin-http` fallback. The writer still needs a small
PVC for the spool; readers need nothing.

#### 4.7.2 Deployment and ownership

- **Schema ownership.** `market_data_system` owns the database and runs the DDL idempotently at
  startup when `--tdengine-auto-create` is set. In production the DDL is applied by the deployment
  job and `--tdengine-auto-create=false` makes the service verify rather than create.
- **Version gate.** `bar_meta.schema_version` is a single row. A reader or writer that finds a
  different version refuses to start, exactly as the engine refuses a mismatched snapshot version;
  there is no mixed-version bar deployment.
- **Lab.** One dnode, `REPLICA 1`, `VGROUPS 1`, a container from
  `tdengine/tdengine:3.3.6.13`. Its manifest will live next to `nats.yaml` and expose only
  6041 (`taosAdapter`) inside the cluster.
- **Production.** Three dnodes and three mnodes, `REPLICA 3`, `VGROUPS` sized to the host count.
  Each vgroup is a Raft group; the connector follows the vnode leader through `taosAdapter`, so a
  dnode failure is a brief retry, not a data loss. TDengine's Helm chart or operator owns the
  cluster lifecycle; `MarketDataService.history` only carries the DSN, database, replica and
  `KEEP`.
- **Network.** Only market-data pods may reach 6041 (NetworkPolicy). The engine, the order system
  and the terminal never see the DSN.

#### 4.7.3 Why TDengine, measured

| Requirement | Measured result on this host (§12) |
|---|---|
| One-day `1m` read (1,440 rows) | 6.45 ms p50, 14.9 ms p99; 3.36 ms warm |
| One-day `1s` read (51,793 rows) | 76-114 ms p50 |
| Query-time `1s` → `1m` rollup, one day | 10.6-11.1 ms p50, exact 1,440/1,440 |
| Eight concurrent one-day `1m` readers | 755 queries/s, zero errors |
| Bulk ingest, `1s` | 166,483 rows/s (7 symbols × 1 day) |
| Bulk ingest, `1m` | 259,508 rows/s (7 symbols × 365 days) |
| Revision upsert | 257,847 rows/s |
| `LAST_ROW` point lookup | ~3.9 ms client round-trip |
| Exact decimals | 38-digit `SUM` and `CAST` matched the `i128` value digit for digit |
| Restart durability | data and exact decimal survived a container restart |

The database is not the bottleneck at this scale; the feed and the writer's aggregation are. That
is the right place for the complexity to sit.

#### 4.7.4 Why not object storage, a shared PVC or NFS

| Option | Problem |
|---|---|
| Object store + reader cache (the earlier design) | Every reader misses into a network fetch, a SHA-256 verify and an mmap; a revision invalidates by content hash; a joiner needs a checkpoint protocol for the open day. The measured TDengine day read is already single-digit milliseconds, so the cache buys little and costs a tier. |
| RWX PVC (NFS/CephFS/EFS) for every reader | `rename`/`fsync` semantics are weaker over NFS, `mmap` over the network turns a page fault into an RPC, and one filesystem outage takes every reader down. |
| Each reader gets its own PVC | Requires copying sealed days N times anyway, with a volume per replica to provision and back up. |
| Serve history only from the writer | The writer becomes the read bottleneck and the failover target; scaling readers buys nothing. |
| TDengine (chosen) | Exact decimals, native time windows, three replicas, a `LAST_ROW` cache, no per-reader state, and one thing to operate instead of a store plus a cache plus a manifest. |

Object storage still has a role: **backup and export**, not the read path. A nightly logical dump
or a Parquet/CSV export can live in the existing object store; queries never touch it (§7.6).

#### 4.7.5 Consistency model

- **Upsert is the only write.** `INSERT` with an existing `(symbol, ts)` replaces the row. There is
  no `UPDATE` path, because a bar is always written as a complete value.
- **Per-subtable atomicity, not global.** One statement targeting one subtable is atomic. A batch
  that spans subtables is not; the writer therefore emits one statement per `(symbol, interval)`
  and retries the failed statement. Every retry is idempotent because the values are identical
  (a duplicate writer) or the revision is already recorded (a retry after a lost acknowledgement).
- **Last write wins.** Two writers with the same cursor produce identical rows, so the winner does
  not matter. A stale writer that lost the Lease cannot corrupt a newer value: it would have to
  write a *different* value for the same key, which means its trade prefix diverged, which the
  cursor and the spool prevent.
- **No cross-row transaction.** A revision writes the audit row first, then the `1s` row, then the
  `1m` row. A crash between them is repaired on startup by comparing each bar's `rev` with the
  newest `bar_revisions` row for `(open_ts, rev)`; a reader may briefly see the old `1m` with the new
  `1s`, which is why the revision completes before the bar is advertised as revised —
  `bar_state.revision` is bumped last and is the value in `X-Bar-Revision`.
- **`LAST_ROW` cache.** `CACHEMODEL 'last_row'` answers "latest bar" in ~3.9 ms. The cache is
  per-vnode and can be briefly stale after a leader change; the writer uses it only at startup to
  seed the cursor, and a normal ordered scan is the fallback. The open bar served to clients is the
  row the writer just wrote, not a cache guess.
- **No transaction around a day.** There is no "seal the day" atomic step; each bar is sealed
  individually when its window closes. A writer crash mid-day leaves the already-sealed rows and
  the open row in place; the morning is durable without a checkpoint.

### 4.8 Bootstrap: connect, verify, ready

A reader that starts at 14:00 queries TDengine and sees every bar the writer has sealed since
midnight, plus the open row. There is no unsealed tail to recover and therefore no checkpoint
state machine. Bootstrap is:

```text
starting ──connect──► verifying ──schema ok──► syncing ──feed attach──► ready
                          │
                          └── 503 + Retry-After ◄── (queries before ready)
```

1. **connect** — open the `taosAdapter` WebSocket, authenticate, select the database. Retry with
   exponential backoff; `/ready` stays false.
2. **verify** — read `bar_meta.schema_version`, compare with the binary's constant; check that the
   expected stables exist, `KEEP` covers the advertised retention, and `REPLICA` matches the
   deployment's expectation. A mismatch is fatal (the process exits with a clear error) rather
   than serving a partially understood schema.
3. **attach + buffer** — attach to the feed *before* the first snapshot query, into a bounded
   buffer (`--bootstrap-buffer-bytes`, 64 MiB). The query is milliseconds, so the buffer is
   normally a handful of trades; it exists to close the race between the snapshot's `seq` and the
   first live frame.
4. **snapshot** — read the last `--replay-bars` bars plus the open row from TDengine. This is the
   first WS snapshot and the denormalized answer for "today so far".
5. **replay** — apply buffered trades whose `market_seq` is greater than the snapshot's last
   `seq`, dropping the rest. The feed is ordered, so the join is exact and idempotent.
6. **ready** — flip `/ready` to true. Until then every query returns `503` with `Retry-After`; the
   gateway only routes to ready replicas, so no client ever sees a short answer.

The writer runs the same steps minus the feed buffering: it must have the feed before it can write,
and it seeds its cursor from the newest bar row plus `bar_state` (and from the spool when this pod
has one, §4.2).

#### 4.8.1 What a client gets while and after joining

| Query window | Served from |
|---|---|
| Current open bar | TDengine's open row, refreshed by the writer at the cadence (`x=false`) |
| Today's closed windows | TDengine rows written since midnight, with no bootstrap step |
| Yesterday and older | TDengine rows, subject to `KEEP` |
| Range spanning both | one query; `X-Bar-Sealed-Through` reports `bar_state.sealed_through` |

A reader never needs the writer to be up to answer a query, and a reader failure never affects the
writer. That separation is the point of putting the store in TDengine rather than in the writer's
process or volume.

#### 4.8.2 When TDengine is unreachable during bootstrap

- Retry with backoff and keep `/ready=false`. A reader that was already serving drops to `503`
  for queries that need the DB; live WS frames already forwarded can continue for the current
  window, but a new subscribe is refused rather than sent a short snapshot.
- If the failure is a single vnode leader election, the connector retries within milliseconds and
  the client sees at most one failed query. The service does not flap `/ready` on a single retry;
  it degrades after `--tdengine-query-timeout-ms` consecutive failures.
- A schema-version mismatch is not retried: it is a deployment error and exits.

### 4.9 Retention, backup and the meaning of `KEEP`

TDengine deletes data older than the database's `KEEP`. The bar database is created with
`KEEP 3650` (10 years) by default; an archive import that reaches further back must raise `KEEP`
**before** the first old row is written (`ALTER DATABASE … KEEP` is allowed upward). A write older
than `KEEP` is rejected by TDengine; the importer treats that as a hard configuration error, not a
row to skip.

`KEEP` is the only retention knob on the read path. There is no warm/cold tier, no segment
compaction and no cache eviction. The API advertises the retention window from `KEEP` and returns
`410` for a range older than it. Backup and long-term export are separate:

- **Backup**: a logical dump (`taosdump`) of `bars_1m` plus the `1s` series for the last N days, or
  a storage-level snapshot where the deployment supports one. The RPO/RTO is an open question
  (§19.5).
- **Export**: Parquet/CSV to object storage for analytics and cold archive. The query path never
  reads it back; a restore is an import, not a tier.

The spool keeps 7 days of raw trades on the writer's PVC. That is the revision and restart window;
older re-derivation uses the archive or REST, marked `source=rest` and reconciled.

## 5. Data model

### 5.1 The canonical bar

```rust
/// Scale-18 fixed-point, as everywhere else in the engine.
pub struct Bar {
    pub open: Fixed,
    pub high: Fixed,
    pub low: Fixed,
    pub close: Fixed,
    pub volume: Fixed,        // base asset
    pub quote_volume: Fixed,  // quote asset
    pub taker_base: Fixed,    // taker-buy base volume
    pub taker_quote: Fixed,   // taker-buy quote volume
    pub trades: u32,
    /// First/last trade ids. `None` for an imported bar: the reference archive
    /// publishes neither.
    pub first_trade: Option<TradeId>, // (log_index, ordinal)
    pub last_trade: Option<TradeId>,
    /// Timestamp of the window's last trade — the engine's taker stamp.
    /// `None` for an imported bar: the reference archive publishes no
    /// last-trade time, and it is never guessed from a serve-time clock.
    pub last_trade_ts_us: Option<u64>,
    pub sealed: bool,
    pub rev: u32,             // 0 = as first written; increments on correction
    pub source: BarSource,    // live | archive | rest
    /// The engine `market_seq` that last changed this bar. 0 for imported and
    /// synthetic bars, which never participate in `since` repair.
    pub seq: u64,
}

pub struct TradeId {
    pub log_index: u64,
    pub ordinal: u32,
}
```

`synthetic` is not a stored field: a bar with `trades == 0` exists only at read time (§7.4). A
stored row always has `trades > 0`. `sealed` replaces the earlier design's `closed` because the
database row, not a segment, is the unit of durability.

### 5.2 Identity and invariants

- Key: `(symbol, interval, open_time_us)`. In TDengine the tag is `symbol`, the timestamp column is
  the window open, and the subtable is `bars_<interval>_<symbol>`.
- `close_time_us = open_time_us + interval_us - 1`.
- Invariants asserted before an upsert (a failure is a bug, not a retry):
  - `low <= open, close <= high`, `high >= low`;
  - `volume >= 0`, `taker_base <= volume`, `taker_quote <= quote_volume`;
  - a stored row has `trades > 0`; a served `trades == 0` row is always `synthetic`;
  - `open_time_us` is a multiple of `interval_us` and
    `close_time_us = open_time_us + interval_us - 1`;
  - `first_trade <= last_trade`, and a traded bar's `last_trade` is strictly greater than the
    previous traded bar's `last_trade`;
  - `rev == 0` unless a `bar_revisions` row exists for `(symbol, interval, open_time_us, rev)`;
  - every value fits `DECIMAL(38,18)`: `|v| < 10^20` (equivalently, the 18-scale integer
    `< 10^38`), and has no more than 18 fractional digits.
- **Timestamp provenance, by source.** This is the rule the whole system depends on; §5.7 defines
  how it reaches the wire.
  - **live**: every timestamp is the taker's engine intake stamp carried in the Raft log —
    `open_time_us = floor(taker_ts_us / interval_us) * interval_us` and
    `last_trade_ts_us = taker_ts_us`. The builder reads no clock, so a replay, a spool restart and
    a second writer produce identical bars.
  - **archive / REST import**: `open_time_us` is the source row's open time;
    `last_trade_ts_us`, `first_trade` and `last_trade` are `None` because the source does not
    publish them. They are **never** filled in from an ingest or serve-time clock. `seq` is 0.
  - **synthetic** (densified empty window): `last_trade_ts_us` is `None`; the window's only
    timestamps are its boundaries; `seq` is 0.

### 5.3 Intervals and anchoring

The public interval set is exactly the Binance spot set: `1s`, `1m`, `3m`, `5m`, `15m`, `30m`,
`1h`, `2h`, `4h`, `6h`, `8h`, `12h`, `1d`, `3d`, `1w`, `1M`.

**Stored** intervals are exactly three: `1s` (the canonical series, formed from trades), `1m` (a
materialized rollup of the traded `1s` children) and `5m` (a rollup of `1m` children). Everything
coarser is computed at read time with TDengine `INTERVAL`. A target that is a whole number of
5-minute windows reads `bars_5m` when that table has data and falls back to `bars_1m` on older
deployments; `3m` always reads `bars_1m`:

| Request | Source | TDengine window |
|---|---|---|
| `1s` | `bars_1s` | none (raw rows) |
| `1m` | `bars_1m` | none (raw rows) |
| `5m` | `bars_5m` | none (raw rows) |
| `3m` | `bars_1m` | `INTERVAL(3m)` |
| `15m`, `30m`, `1h` … `12h` | `bars_5m` (fallback `bars_1m`) | `INTERVAL(width)` |
| `1d`, `3d` (UTC) | `bars_5m` (fallback `bars_1m`) | `INTERVAL(1d)` / `INTERVAL(3d)` |
| `1d`, `3d` (`timeZone=±HH:MM`) | `bars_5m` (fallback `bars_1m`) | `INTERVAL(width, (24h - tz) mod 24h)` |
| `1w` (UTC) | `bars_5m` (fallback `bars_1m`) | `INTERVAL(1w, 4d)` — Monday 00:00 |
| `1w` (`timeZone=±HH:MM`) | `bars_5m` (fallback `bars_1m`) | `INTERVAL(1w, (96h - tz) mod 168h)` |
| `1M` (UTC) | `bars_5m` (fallback `bars_1m`) | `INTERVAL(1n)` — natural month |
| `1M` (`timeZone=±HH:MM`) | `bars_1m` | edge-fold from `1m` at local midnight |

The offset forms are verified against the pinned version: `INTERVAL(1d, 16h)` starts a day at
16:00 UTC (= 00:00 `+08:00`), `INTERVAL(1w, 4d)` starts on Monday, `INTERVAL(1w, 88h)` starts on
Sunday 16:00 UTC (= Monday 00:00 `+08:00`), and `INTERVAL(1n)` starts on the 1st. A negative offset
literal is a syntax error, so the offset is always expressed as a positive hour count; for `1M`
with a positive time zone the boundary falls on the previous month's last day and is therefore
edge-grouped rather than expressed as a TDengine offset.

**Why only `1s`, `1m` and `5m` are stored.** Materializing every interval multiplies the write path
and makes a revision cascade through thirteen tables, while the measured cost of the read-time
rollup is already single-digit to low-tens of milliseconds for the windows the 1000-bar cap allows
(§12). `1m` is materialized because a day of `1m` (6.5 ms) is measurably cheaper than rolling a day
of `1s` up on every request (11 ms). `5m` is the additional 6-row source for `30m` and larger
multiples; `1m`→`5m`→`30m` is asserted field-by-field against a direct `1m`→`30m` rollup. `1h` and
`1d` are not materialized: a year of `1d` from `1m` measured 28 ms.

**Offset-anchored requests** are `1d`, `3d`, `1w` and `1M` under a non-zero `timeZone`. They are
computed at query time and never stored separately. Every supported offset is a multiple of 15
minutes and the source series is `1m`, so an edge window is always a whole number of `1m` children.
Grouping rules are exact (`open` = first child's `open`, `close` = last child's `close`,
`high` = max, `low` = min, volumes sum, trades sum), and only children that traded contribute — a
synthetic child cannot invent a trade. For `1M` with a positive time zone the local month starts on
the previous UTC day, so the first and last month windows are folded from their `1m` rows instead
of using `INTERVAL(1n)`.

The response echoes the anchor (`"tz":"+08:00"`) so the client can verify it. If offset-anchored
intervals ever need to be first-class, they get their own interval ids (`1M@+08:00`) rather than
overloading `1M`.

The `timeZone` parameter accepts `[-12:00, +14:00]` in the same three forms Binance accepts (`8`,
`05:45`, `+08:00`), and `startTime`/`endTime` are always UTC, matching the reference.

### 5.4 Time units

The store and the internal API use **microseconds** (`i64` for absolute times, `u64` for windows)
because the reference archive switched to microseconds on 2025-01-01 and mixing units is the single
most common loader bug in this domain. TDengine is created with `PRECISION 'us'`; the column is a
`TIMESTAMP` in microseconds. The Binance-shaped response converts to **milliseconds** because that
is what the contract publishes; the native shape returns both (`t`/`T` in ms, `t_us`/`T_us` in µs)
so a differential test can compare either.

Unit conversion never rounds. `ms = floor(us / 1000)` is applied only at the edge, and the stored µs
value is authoritative; a bar served in the reference shape and the same bar served in the native
shape denote the same instant.

### 5.5 Trade linkage

Our `f`/`L` are `(log_index, ordinal)` pairs, rendered in the Binance shape as the first trade's
`log_index` in `f` and the last trade's `log_index` in `L` (the ordinal is native-only); both are
`null` for an imported bar, whose source publishes no trade ids. A live bar is therefore traceable
to exact log positions and can be re-derived from the trade spool (§4.5); the engine's `/events` log
confirms that no position was skipped but cannot supply the entry timestamp, which is why the spool
exists. This is stronger than Binance's opaque incremented trade ids and is the basis for
reconciliation.

`seq` is the `market_seq` of the last message that changed the bar. It is not a per-frame counter:
one market message can carry a sweep of fills, and the bar records that one update. Every replica
sees the same `market_seq` in the feed envelope, so `seq` is replica-agreeable without a writer
counter or a shared sequence service. `market_seq` is monotonic within a symbol deployment; a
cross-shard migration may start a new base, which is a resubscribe boundary, not a repair boundary.

### 5.6 Lifecycle

| State | Where it lives | Visible as |
|---|---|---|
| **open** | the latest TDengine row, `sealed=false` | `x=false`, `rev=0` |
| **sealed** | the TDengine row, `sealed=true` | `x=true`, final |
| **revised** | the same row, `sealed=true`, `rev>0`, plus a `bar_revisions` record | `x=true`, `rev>0` |

There is no separate "provisional" state and no open segment. The open row is already durable in
TDengine, so a writer restart mid-day does not lose the morning: it reads the row back and
continues. A query takes the open row directly from the database; it does not reconstruct it from a
ring owned by another process.

### 5.7 Event time on the wire

The reference payload has exactly one event-time field, `E`. Its value is defined by the bar's
source, and it is never "now":

| Bar source | `E` (ms) | Why |
|---|---|---|
| live (open or closed) | `last_trade_ts_us` | the taker's engine stamp — the same instant as the trade's `T` and the bar window |
| imported closed | `close_time` (window end) | the source publishes no last-trade time; the window end is deterministic and inside the bar |
| synthetic, closed (empty window) | `close_time` | the bar's flat content became true when the window ended |
| synthetic, open (empty window) | `open_time` | nothing has happened yet, so the content became true at the window start — using the close would put `E` in the future for a `1d` bar |
| `klineSnapshot` replay | the bar's own value above | a replayed frame keeps the bar's time, never the socket's |

`E` therefore always satisfies `t <= E <= T`, never exceeds the newest engine time known to the
writer, and is never "now".

1. A live bar's `E` carries millisecond precision because it is a real trade instant, not a rounded
   window boundary.
2. An imported bar has `E == T`. The native shape's `null` `last_trade_ts_us` is how a client
   distinguishes "a trade at the close" from "the source published no trade time".
3. `GET /bars?since=` and the WS replay return the stored per-bar value, so re-serving a bar from
   TDengine never changes its `E`.

The native object carries `t_us`/`T_us` and `last_trade_ts_us`, so provenance is inspectable per bar
without guessing.

## 6. Generation

### 6.1 Source: trades, not `Bar` frames

The builder consumes `MessageBody::Trade` from the engine feed. The engine's `MessageBody::Bar`
frames are still consumed, but only as a **cross-check**: a mismatch increments
`bar_engine_mismatch_total` and is logged with both values. They are not stored and never served.

This is the decision that fixes the reference's biggest practical gap. In the engine,
`update_bars` suppresses the first emit of every window (`last_live_ms` is seeded to `ts_ms`) and
nothing closes a window on a timer, so a quiet market publishes no klines at all and the newest
candle always lags one trade. Deriving from trades makes the historical path correct regardless of
when that engine bug is fixed, and lets us emit the cadence unconditionally.

### 6.2 Builder algorithm

Per `(symbol, interval)`, on each trade `(ts_us, price, volume, quote_volume, taker_side,
market_seq)`:

```text
window = floor(ts_us / interval_us) * interval_us
if window != open.window:
    close(open)                              // emit x=true, seal candidate
    open = synthetic_bar(window, prev.close) // seed O=H=L=C=prev.close, v=0
open.high  = max(open.high, price)
open.low   = min(open.low, price)
open.close = price
open.volume += volume
open.quote_volume += quote_volume
if taker_side == Buy: open.taker_base += volume; open.taker_quote += quote_volume
open.trades += 1
open.last_trade = trade_id
open.last_ts = ts_us
open.seq = market_seq
open.sealed = false
```

Properties: one pass, no allocation after warm-up, deterministic given the trade order. The builder
runs one instance per symbol for **both** stored intervals at once: a trade is folded into the open
`1s` window and, through the `1s` close, into the open `1m` rollup. `1m` is therefore a rollup of
traded `1s` children, not a second independent aggregation of raw trades; the two agree by
construction, and the measured differential test (§6.6) is the guard.

`prev.close` is the close of the last window that traded (not the previous emitted flat bar), which
is what the archive samples show.

`ts_us` is never read from a clock. It is the taker's engine intake timestamp that travelled in the
Raft log and survives in `TradeRecord.ts_ms` — the same instant the live feed, the trade tape and
`GET /klines` expose, converted from milliseconds at ingestion. The builder contains no
`SystemTime` call at all, which is what makes a replay, a spool restart and a second writer produce
identical bars; an ingest-time stamp would silently move a fill across a window boundary on a slow
or replaying consumer. The importer is the one component that reads timestamps from outside, and it
only reads the source's own open/close times — it never invents a trade time (§5.7).

### 6.3 Cadence and closing

A per-symbol ticker fires every `min(interval, 2 s)` and re-publishes the open bar. For `1s` that is
every second, for everything else every 2 s, matching the reference. The ticker does not create bars
for windows with no trades beyond the open one; it re-sends the current open bar with the same (or a
newer) `seq` even when nothing changed, so a client can distinguish "quiet" from "disconnected".

The writer upserts the open row on each tick (a single row, `sealed=false`). That is one write per
second per symbol for `1s`, and one per two seconds for `1m`; the measured revision upsert path
sustains 257k rows/s, so the live open-bar write is noise.

Closing is watermark-driven, not ticker-driven:

```text
watermark_us = max(event_time_seen)      // advanced by trades and by the feed heartbeat
close all windows with window + interval_us + seal_lag <= watermark_us
```

`seal_lag` defaults to 2 × interval and is capped at 60 s. It absorbs feed reordering and the
catch-up path without delaying visibility of a closed bar by more than a couple of seconds. A
closed bar is upserted with `sealed=true` and the final values; the next tick creates the new open
row.

### 6.4 Late data and revisions

A trade whose window is already closed but not yet **sealed** is applied to the open row and the
affected windows are re-emitted with the same `open_time`.

A trade whose window is already sealed is a reconciliation event:

1. re-derive the window from trades (the spool keeps the trade-id range, so this is a bounded
   read);
2. append a `bar_revisions` record `(symbol, interval, open_ts, rev, old_json, new_json, source,
   at)`; the audit is written before the row so a crash leaves a record of the attempted change,
   which is retried idempotently on startup;
3. upsert the `1s` row with `rev+1`;
4. re-derive the containing `1m` row from its `1s` children (a 60-second range query, or the spool
   when the range is older than `KEEP`'s write window) and upsert it with `rev+1`;
5. bump `bar_state.revision` — this is the publish point and the value returned in
   `X-Bar-Revision`;
6. coarser intervals need no rewrite: they are computed at read time from `bars_1m`, so the next
   query sees the revision.

A replacement pod whose spool is empty re-derives from the `1s`/`1m` rows where possible; where it
cannot, it records the range in `bar_gaps` and marks the corrected bar `source=rest` until the
reconciler confirms it. The query path never returns a `rev>0` bar without its revision being
committed first. A `bar_revisions` row is append-only and is never deleted by `KEEP` before the bar
it describes.

### 6.5 Restart and replay determinism

The builder is a pure function of the trade prefix. On start:

1. read the newest bar row's `last_log`/`last_ord` and `last_ts` for the symbol (and `bar_state` for
   the watermark);
2. if this pod has a spool, replay it from that cursor (§4.5) — the spool carries `ts_ms` and the
   envelope's `market_seq`, so this is the exact stream, not a reconstruction;
3. if the spool is absent or does not reach back far enough (replacement pod, spool lost), trust the
   newest row's own cursor and record any unrebuildable range in `bar_gaps`; recover it from the
   archive or REST where possible, and mark those bars `source=rest` until the reconciler confirms
   them;
4. only then attach to the live feed, dropping live trades whose `(log_index, ordinal)` is
   `<=` the cursor.

Replaying the same trade prefix twice yields identical rows, which is what makes the "duplicate
writer" and "re-import" cases boring.

### 6.6 Rollups: one base series, every interval derived

Only the `1s` series is canonical. `1m` is a materialized rollup of the **traded (sparse) base
windows**; every coarser interval is a rollup of `1m` at read time:

| Output | Inputs | Rule |
|---|---|---|
| `1s` | trades | §6.2, the only place a bar is formed from fills |
| `1m` | traded `1s` children | `open` = first child's `open`, `close` = last child's `close`, `high` = max, `low` = min, volumes/trades sum, `first_trade` = first, `last_trade` = last, `last_ts` = last child's `last_ts`, `seq` = last child's `seq` |
| `5m` | traded `1m` children | the same rule; materialized so coarser multiples have a 6-row instead of 30-row source |
| `3m`, `15m` | `1m` rows | `INTERVAL` rollup (not a whole number of 5-minute windows for `3m`) |
| `30m` … `1M` | `5m` rows when present, else `1m` | the same rule, executed by TDengine `INTERVAL` / `FIRST` / `LAST` / `MAX` / `MIN` / `SUM` |
| any interval | — | a window with no traded child is **not** stored; densification at serve time supplies the flat carry-forward (§7.4) |

**The sparse rule is not cosmetic — it is measured.** Aggregating the 2025-01-02 BTCUSDT archive:

| Rollup | Exact match vs Binance | Difference |
|---|---|---|
| `1s` → `1m`, **traded-only** children | **1,440 / 1,440** | none |
| `1s` → `1m`, dense children (flat seconds included) | 1,411 / 1,440 | 29 `open` values off by one tick: the flat first second carries the **previous** close, not the first trade of the minute |
| `1m` → `1h`, traded-only (and dense) | 24 / 24 | that day had no empty leading minute, so both agree; the `1s` case is the discriminator |
| `1m` → `5m` → `30m` vs `1m` → `30m` | exact, field by field | unit test in `market_data_system/src/history/query.rs`; sparse minutes never become flat children |
| live `5m` → `30m` via `/klines` | 17/17 traded windows exact | `scripts/check-30m-rollup.py` against the OrbStack terminal; OHLC, both volumes and trade count |

The same equality was reproduced through TDengine: a query-time `INTERVAL(1m)` rollup of the stored
`1s` day returned 1,440 windows and matched the materialized `bars_1m` rows field for field, while
the base series had 51,793 sparse traded seconds. That makes the read-time path a verified
implementation of the same rule, not a second interpretation.

Consequences:

- **Storage**: `bars_1m` and `bars_5m` are materialized rollups — read accelerators, not independent
  sources of truth. A revision to a `1s` window re-derives the containing `1m` and `5m` rows (§6.4);
  coarser intervals are recomputed on the next query.
- **Writes**: the writer maintains `1s`, `1m` and `5m`. There are no per-interval checkpoints and no
  fan-out beyond those three series.
- **Queries** never aggregate raw `1s` for far history: `5m` is the fast path for targets that are
  whole multiples of 5 minutes (including `30m`), `1m` for the rest, with an automatic fallback to
  `1m` when `bars_5m` is empty; `--derive-on-read` (optional) exists only for a repair or a backfill
  check.
- **Equality with the reference is testable**: the golden test in §16 asserts the 1,440/1,440
  result, and a dense-vs-sparse differential test would fail if the densification rule ever leaked
  into a rollup.

## 7. TDengine schema and storage

### 7.1 Databases and DDL

One database for bars and their state, created with microsecond precision, a `KEEP` that covers the
advertised retention, and the last-row cache that makes "latest bar" cheap:

```sql
CREATE DATABASE IF NOT EXISTS market_bars
  PRECISION 'us'
  KEEP 3650
  DURATION 10d
  VGROUPS 1              -- lab; size to the host/vgroup plan in production
  REPLICA 1              -- lab; 3 in production
  CACHEMODEL 'last_row';

CREATE STABLE IF NOT EXISTS market_bars.bars_1s (
  ts            TIMESTAMP,          -- window open, µs
  open          DECIMAL(38,18),
  high          DECIMAL(38,18),
  low           DECIMAL(38,18),
  close         DECIMAL(38,18),
  volume        DECIMAL(38,18),
  quote_volume  DECIMAL(38,18),
  taker_base    DECIMAL(38,18),
  taker_quote   DECIMAL(38,18),
  trades        BIGINT UNSIGNED,
  last_ts       TIMESTAMP,          -- last trade time; NULL for imported
  first_log     BIGINT, first_ord INT,
  last_log      BIGINT, last_ord  INT,
  rev           INT,
  source        TINYINT,            -- 0 live, 1 archive, 2 rest
  sealed        BOOL,
  seq           BIGINT              -- last market_seq; 0 for imported
) TAGS (symbol VARCHAR(32));

CREATE STABLE IF NOT EXISTS market_bars.bars_1m (
  ts TIMESTAMP,
  open DECIMAL(38,18), high DECIMAL(38,18), low DECIMAL(38,18), close DECIMAL(38,18),
  volume DECIMAL(38,18), quote_volume DECIMAL(38,18),
  taker_base DECIMAL(38,18), taker_quote DECIMAL(38,18),
  trades BIGINT UNSIGNED,
  last_ts TIMESTAMP,
  first_log BIGINT, first_ord INT,
  last_log BIGINT, last_ord INT,
  rev INT,
  source TINYINT,
  sealed BOOL,
  seq BIGINT
) TAGS (symbol VARCHAR(32));

-- Identical columns; 5m is the intermediate source for 30m and larger rollups.
CREATE STABLE IF NOT EXISTS market_bars.bars_5m (
  ts TIMESTAMP,
  open DECIMAL(38,18), high DECIMAL(38,18), low DECIMAL(38,18), close DECIMAL(38,18),
  volume DECIMAL(38,18), quote_volume DECIMAL(38,18),
  taker_base DECIMAL(38,18), taker_quote DECIMAL(38,18),
  trades BIGINT UNSIGNED,
  last_ts TIMESTAMP,
  first_log BIGINT, first_ord INT,
  last_log BIGINT, last_ord INT,
  rev INT,
  source TINYINT,
  sealed BOOL,
  seq BIGINT
) TAGS (symbol VARCHAR(32));
```

`bars_1m` repeats the column list on purpose: each stable is an explicit schema contract, and a
test asserts that the two column lists are identical so they cannot drift. The duplication is the
cost of making a column addition a visible, reviewable migration rather than an implicit one.

State and audit tables:

```sql
CREATE STABLE IF NOT EXISTS market_bars.bar_state (
  ts              TIMESTAMP,        -- state update time
  sealed_through  TIMESTAMP,        -- event-time watermark
  first_ts        TIMESTAMP,        -- first stored bar for the symbol
  revision        BIGINT,           -- global revision counter
  repair_floor_seq BIGINT,          -- max seq outside the `since` repair slice (§8.2)
  last_log        BIGINT,
  last_ord        INT,
  writer_epoch    VARCHAR(64)       -- writer instance that owns the cursor
) TAGS (symbol VARCHAR(32));

CREATE STABLE IF NOT EXISTS market_bars.bar_gaps (
  ts         TIMESTAMP,             -- when the gap was noticed or closed (never a bar time:
                                    --   a gap row must outlive KEEP, not age out with it)
  gap_start  TIMESTAMP,             -- first missing window, inclusive
  gap_end    TIMESTAMP,             -- exclusive
  open       BOOL,                  -- true = hole exists; a repair appends open=false
  reason     TINYINT,               -- 1 spool lost, 2 feed gap, 3 import missing
  detail     VARCHAR(256)
) TAGS (symbol VARCHAR(32), interval VARCHAR(4));

CREATE STABLE IF NOT EXISTS market_bars.bar_revisions (
  at        TIMESTAMP,              -- revision time (writer clock), audit only
  open_ts   TIMESTAMP,              -- the revised window
  rev       INT,
  old_json  VARCHAR(2048),
  new_json  VARCHAR(2048),
  source    TINYINT,
  detail    VARCHAR(256)
) TAGS (symbol VARCHAR(32), interval VARCHAR(4));

CREATE STABLE IF NOT EXISTS market_bars.bar_imports (
  at        TIMESTAMP,              -- import attempt time
  file      VARCHAR(256),
  sha256    VARCHAR(64),
  rows      BIGINT,
  status    TINYINT                 -- 0 started, 1 verified, 2 failed
) TAGS (symbol VARCHAR(32), interval VARCHAR(4));

CREATE TABLE IF NOT EXISTS market_bars.bar_meta (
  ts              TIMESTAMP,        -- last schema verification, refreshed on every start
  schema_version  INT,
  created_at      TIMESTAMP,
  note            VARCHAR(256)
);
```

`bar_state` is read with `LAST_ROW` per symbol and is the source of the `X-Bar-Sealed-Through`,
`X-Bar-Revision` and cursor headers. `bar_gaps` makes an ingestion hole explicit instead of a flat
stretch; a query whose range overlaps a gap returns `409` with the gap bounds (§8.1). `bar_revisions`
is the audit trail; `bar_imports` is the importer's file ledger. `bar_meta.schema_version` is the
deployment gate.

The subtable name is derived from the validated symbol (`[a-z0-9_]{1,32}`) and the interval; user
input never reaches an identifier. A symbol that fails the pattern is rejected at configuration
time, not at query time.

### 7.2 The exact-decimal contract

This is the part that must not be hand-waved. A time-series database is often the place where an
18-decimal engine value quietly becomes an `f64`. TDengine does not do that, but the client can.
The contract is:

1. **The unit is the engine's scale-18 `i128`.** Every bar value is an integer number of
   `10^-18` units. There is no `f64` and no implicit decimal type in the bar path.
2. **TDengine `DECIMAL(38,18)` is exact.** It holds 20 integer digits and 18 fractional digits.
   A decimal literal is parsed exactly; `SUM` is exact; an aggregate that would exceed 38 digits
   fails with `Decimal value overflow` (code 317) instead of truncating. Measured on 3.3.6.13:
   - `0.100000000000000001`, `0.200000000000000002`, `123456789.123456789123456789` and
     `99999999999999999999.999999999999999999` round-tripped digit for digit;
   - `SUM` of those four returned `100000000000123456789.423456789123456791`, the exact value;
   - `CAST(… AS VARCHAR)` preserved every digit, including trailing zeros where present;
   - a 21-integer-digit value was rejected with code 317; a 19-fractional-digit value was rounded,
     so **the ingest guard rejects >18 fractional digits before the database sees them**.
3. **Write path: canonical text.** A `Fixed` is formatted as `[-]<integer>.<18 digits>` and
   embedded in the `INSERT` statement. The literal is parsed by TDengine exactly. The writer never
   binds a value through `f64`, `rust_decimal` or any type with fewer than 38 digits.
4. **Read path: `CAST … AS VARCHAR`.** TDengine's JSON and binary protocols can return `DECIMAL` as
   a client decimal. The `taos` Rust connector maps it to `rust_decimal::Decimal`, which is a
   96-bit, 28-29-digit type. Measured: parsing
   `99999999999999999999.999999999999999999` through `rust_decimal` produced
   `100000000000000000000.00000000` — the database was exact, the client silently dropped 18
   fractional digits. The serving query therefore casts every decimal column (and every aggregate
   result) to `VARCHAR` and parses the text with the same strict 18-scale parser used by the engine
   API. `VARCHAR` is lossless in the connector.
5. **The parser is strict.** It accepts `[+-]?digits[.digits]`, scales to exactly 18 fractional
   digits, rejects more than 18 fractional digits, rejects exponent notation and rejects values
   outside `|v| < 10^20`. It accepts `0` and `1.000000000000000000` as the same zero/one; TDengine
   may omit trailing zeros for an integer-valued decimal, which is a representation difference, not
   a value difference.
6. **The guard is explicit.** Before a row is written, every value is checked against the
   `DECIMAL(38,18)` bound and the 18-fractional-digit rule. A violation is a bug or a malformed
   import: the row is rejected, `bar_decimal_rejected_total` increments, and the error is logged with
   the symbol and window. The writer never rounds a value to make it fit.
7. **Aggregates are cast, not trusted.** `CAST(FIRST(open) AS VARCHAR)`,
   `CAST(MAX(high) AS VARCHAR)`, `CAST(SUM(volume) AS VARCHAR)` and the rest are the only way an
   aggregate leaves the database. A `SUM` overflow is surfaced as a `500` with the offending range,
   never as a wrapped or truncated number.
8. **The wire is text.** The Binance shape already publishes strings; the native shape publishes
   the same canonical strings. The terminal and any other client parse them without an intermediate
   `f64`. This is the same rule as `docs/DECIMAL.md`: 18-scale integers end to end.

**Why not store a raw `i128` in two `BIGINT`s or a `BINARY(16)`?** It would make point reads
allocation-free, but it doubles the column count, defeats TDengine's decimal compression and makes
every aggregate a client-side loop. The measured cost of the `CAST` path is small: a one-day `1m`
read went from 5.74 ms to 8.40 ms (+2.7 ms for five columns) and 9.97 ms for seven; a one-year
`1d` rollup was unchanged at ~28 ms because it is aggregation-bound. Exactness is worth 2-3 ms on a
query that already takes single-digit milliseconds.

**Why not `DOUBLE`?** It cannot represent 18-decimal values exactly, and it would reintroduce the
rounding the engine's `Fixed` migration removed. `FLOAT`/`DOUBLE` columns are never used for prices,
volumes or fees in this schema.

**Range.** `DECIMAL(38,18)` covers any realistic market value: the integer part allows 20 digits,
while BTC at 100,000 is 6 digits and a symbol-year of volume is well under 20. The engine's `i128`
allows about 1.7e38, slightly more than `10^38 - 1`; the database is the tighter bound and the
ingest guard enforces it. A value that exceeds it is rejected, not wrapped.

### 7.3 Write path

```text
builder ──(symbol, interval, Bar)──► bounded queue ──► writer task
                                                        │
                          group by (symbol, interval)   │
                          format canonical decimals    │
                          one INSERT per group          │
                          retry on transient error ─────┘
```

- **One statement per `(symbol, interval)` group.** A group is a single subtable, so the statement
  is atomic. A batch never mixes subtables, which is what makes the partial-success caveat
  tractable: a failed group is retried, a succeeded group is not repeated.
- **Batch size `--tdengine-batch-rows` (2000).** The measured bulk path sustained 166k-259k rows/s
  at this size; the live path writes one to a few rows per second per symbol, so the batch is
  usually one row.
- **Upsert, not append.** `INSERT INTO bars_1s_<symbol> USING bars_1s TAGS ('<symbol>') VALUES
  (...)` twice for the same `ts` replaces the row. The open bar is re-upserted every cadence; the
  sealed bar is the final upsert of that key.
- **Retry is safe.** A timeout can mean "committed but the acknowledgement was lost". The writer
  retries the same statement; the values are identical, so the second write is a no-op in value
  terms. A revision retry first reads `bar_revisions` for `(open_ts, rev)` and skips the append if
  the row is already there (§7.5).
- **Ordering.** When a spool is present, the writer appends the trade to it before writing the bar
  row, so a bar committed by that pod always has its input inside the spool's retention window. A
  replacement pod without a spool relies on the row's own `last_log` cursor (§4.2) and records any
  unrebuildable range as a gap instead of pretending it can reconstruct it.
- **Backpressure.** The queue is bounded. A full queue does not block the feed; it increments
  `bar_db_queue_full_total` and leaves the trades in the spool. The builder's open bar is still
  correct in memory; the DB catches up from the spool. A prolonged outage is a `/ready=false` and
  an alert, not a lost trade.

### 7.4 Read path

```text
GET /klines?symbol&interval&startTime&endTime&limit
  ├─ 1s / 1m / 5m : SELECT ts, CAST(col AS VARCHAR) ... WHERE symbol=… AND ts>=… AND ts<…
  ├─ 3m … 1M      : SELECT _wstart, CAST(FIRST(open) AS VARCHAR), CAST(MAX(high) AS VARCHAR),
  │                   CAST(MIN(low) AS VARCHAR), CAST(LAST(close) AS VARCHAR),
  │                   CAST(SUM(volume) AS VARCHAR), … FROM bars_1m
  │                   WHERE symbol=… AND ts>=… AND ts<… INTERVAL(width[, offset])
  ├─ previous close: SELECT close ... WHERE symbol=… AND ts<start ORDER BY ts DESC LIMIT 1
  ├─ seal watermark: SELECT LAST_ROW(sealed_through), LAST_ROW(revision) FROM bar_state
  └─ densify       : walk [start, end); missing window → O=H=L=C=previous close, v=0, n=0,
                     synthetic=true, x=true (or x=false for the open window)
```

- The query fetches the sparse rows in `[start, end)`; the response walks the dense window grid and
  fills the holes. The previous close comes from a reverse `LIMIT 1` before `start`, which is a
  short reverse scan of the sparse table, not a full aggregate.
- `limit` is applied **after** densification, because the cap counts returned bars. To return the
  most recent N bars, the handler reads `LAST_ROW(ts)` (or the last row of the interval query),
  computes the start as `last_open - (limit-1)*width`, and fetches that range. The extra point
  lookup is ~3.9 ms; it is the price of a correct "last N" with sparse storage.
- The open window is included by default (`x=false`). `includeOpen=false` adds `sealed=true` and
  excludes it.
- `ORDER BY ts` is explicit. A descending query is used only for the "last N" anchor and is reversed
  before densification; the client never sees a descending array.
- Empty ranges return `[]` (the reference behaviour). A range that overlaps a `bar_gaps` row, or
  extends beyond `bar_state.sealed_through` into the past, returns `409` (§8.1). A range before
  `bar_state.first_ts` is legitimately empty (pre-listing) and returns `[]`.
- The connection pool is per process. Readers use read-only queries; the writer uses the same pool
  for upserts. A reader never writes.

The `CAST` columns are the exactness boundary (§7.2); no other read path decodes a decimal.

### 7.5 State, gaps and audit

- **`bar_state`** — one current row per symbol, read with `LAST_ROW`. It carries the event-time
  watermark, the first stored bar, the global revision counter, the repair floor, the last applied
  trade id and the writer epoch. The writer updates it after each seal and on startup.
- **`bar_gaps`** — append-only and keyed by the time the gap was **noticed**, not by the gap's own
  window: TDengine deletes rows older than `KEEP`, and an unrepaired 2025 hole must not disappear
  from the table in 2035 merely because its bars aged out. `gap_start`/`gap_end` carry the missing
  range. A gap is closed by a successful repair, which writes a `bar_revisions` record for every
  repaired bar and then appends a second row with the same `gap_start`, a newer `ts` and
  `open=false` — the record of the hole survives the repair. Gaps are rare, so the reader loads the
  symbol's rows and reduces them by `gap_start` (newest `ts` wins); there is no online aggregate to
  get wrong.
- **`bar_revisions`** — append-only audit. `old_json`/`new_json` are canonical serializations of the
  full bar, so a disputed value can be reconstructed exactly without a second table. TDengine has
  no unique constraint beyond the timestamp, so retry idempotence is explicit: the writer first
  reads whether `(open_ts, rev)` already exists and skips the append if it does; a rare duplicate
  from a crash between the check and the insert is harmless because readers resolve a revision by
  `(open_ts, rev)` and take the newest `at`.
- **`bar_imports`** — the importer's ledger: file name, SHA-256, row count, status. A re-import of
  the same hash is skipped after a ledger lookup; a changed hash triggers a diff and revisions
  rather than a duplicate row.
- **`bar_meta`** — a single logical row, upserted on every startup so its timestamp keeps moving and
  `KEEP` cannot silently delete the schema marker. `schema_version` is read with
  `ORDER BY ts DESC LIMIT 1`.

### 7.6 Compression, retention and backup

Measured on the benchmark schema (7 symbols, `1s` for one day, `1m` for one year; `5m` derived from `1m`):

| Series | Rows | Compressed size | Bytes/row | Ratio |
|---|---|---|---|---|
| `1m` | 3,679,200 | 62.4 MB | 17 | 14.7% |
| `1s` | 362,433 | 14.4 MB | 40 | 32.0% |

`du` on the vnode directory reported 606 MB, but that is TDengine's preallocated vnode file, not
the data size; `SHOW TABLE DISTRIBUTED` is the number to use for capacity. `DECIMAL` columns are
stored with zstd and no delta encoding in 3.3.6, which is why a `1s` row is 40 B rather than the
~14 B an integer-delta codec would reach. The trade-off is deliberate: the database is exact and
operational, and 40 B per traded second is ~1.26 GB per symbol-year at a full 86,400 traded
seconds, or ~0.76 GB at the measured 60% density.

`KEEP` is the retention boundary. `bars_1s`, `bars_1m` and `bars_5m` share the database `KEEP`; a deployment
that wants a shorter `1s` retention and a longer `1m` retention uses two databases or accepts the
same window. The default 10 years covers the archive and the live path; a longer import raises
`KEEP` first. There is no compaction job, no segment merge and no cache eviction.

Backup is logical (`taosdump`) or storage-level, and export is Parquet/CSV to object storage. Both
are outside the query path; a restore is an import with `source=archive`, not a tier promotion.

### 7.7 Schema evolution

`bar_meta.schema_version` is a single integer. Adding a column is a new stable definition plus a
migration that backfills the new column (or leaves it NULL with a documented meaning); a reader
that does not understand the version refuses to start. The schema is intentionally additive: new
optional columns (`taker_base`, `seq`, `rev`) are the pattern; removing or retyping a column is a
new database and a re-import, exactly as the engine treats snapshot versions.

## 8. Query API

### 8.1 `GET /klines` — Binance-compatible

Served by the existing market-data service on its current listener, next to `/ws`, `/info` and
`/metrics`; no extra port or Service.

```text
GET /klines?symbol=gbpusd&interval=1m&startTime=1735689600000&endTime=1735693200000&limit=1000
```

| Parameter | Required | Default | Notes |
|---|---|---|---|
| `symbol` | yes | — | lowercase, as advertised by `/info` |
| `interval` | yes | — | the 16 reference intervals, case-sensitive |
| `startTime` | no | — | ms, UTC, inclusive of bar **open** time |
| `endTime` | no | — | ms, UTC, inclusive of bar **open** time |
| `limit` | no | 500 | max 1000 (compatibility cap) |
| `timeZone` | no | `0` | re-anchors `1d`/`3d`/`1w`/`1M` only (see §5.3) |
| `includeOpen` | no | `true` | extension: `false` returns only sealed bars |
| `format` | no | `array` | extension: `object` returns the native shape |

Response: a bare JSON array of twelve-element arrays, exactly as the reference:

```json
[[1735689600000,"1.25010","1.25040","1.24990","1.25030","0.82000",
  1735689659999,"1.02510",17,"0.41000","0.51250","0"]]
```

The strings are canonical 18-decimal values; the example is trimmed for readability. Semantics,
stated explicitly because the reference leaves them fuzzy:

- A bar is returned when its **open** time lies in `[startTime, endTime]` (both inclusive). If only
  `endTime` is given, the range ends there and starts `limit` bars earlier; if neither is given, the
  most recent `limit` bars are returned, including the open one.
- When more than `limit` bars match, `startTime` wins: the response starts at `startTime` and
  returns `limit` bars, and `X-Bar-Next` carries the next `openTime` (ms) to continue. This is
  deterministic, unlike an implicit "whichever 1000 the server chose".
- Every response carries: `X-Bar-Schema: 1`, `X-Bar-Sealed-Through: <ms>`,
  `X-Bar-Revision: <bar_state.revision>`, `ETag: "<revision>-<sealed_through>"`, and `X-Bar-Next`
  when the range is truncated.
- Errors: `400` for an unknown symbol (`-1121`) or interval (`-1120`), an unparseable time zone or
  `limit > 1000`; an empty but valid range returns `[]` (the reference behaviour), not an error;
  `409` when the range overlaps a known ingestion gap or extends past `sealed_through` into the
  past (a hole is surfaced, never smoothed over); `410` with `Retry-After` and
  `X-Bar-Retention-Days` when the range is older than `KEEP` (or, for the `since` form below,
  when the cursor is below the repair floor).
- A `5xx` from TDengine is a `503` with `Retry-After`, never a partial array. A `SUM` overflow is a
  `500` naming the range; it is a data/query bug, not a transient error.

### 8.2 `GET /bars` — native shape and gap repair

```text
GET /bars?symbol=gbpusd&interval=1s&since=884422&limit=1000
```

Returns objects with the full record: `t`, `T`, `t_us`, `T_us`, `o`, `h`, `l`, `c`, `v`, `q`, `n`,
`V`, `Q`, `f`, `L`, `E`, `last_ts_us`, `x`, `rev`, `synthetic`, `seq`. `f`, `L` and `last_ts_us` are
`null` for an imported bar (§5.7); `seq` is `null`/0 for an imported or synthetic bar. `limit` is
10 000 here because internal clients are not bound by the reference cap.

`since=<seq>` is the repair primitive. It returns the **current state** of every bar whose
`seq >= since` and whose window lies within the repair slice, deduplicated by `open_time` and
ordered by `seq` — a client that missed frames gets the authoritative end state of exactly the
windows it missed, not a replay of superseded refreshes (a bar is a value; the intermediate values
were never true for longer than one cadence). Because `seq` is the engine `market_seq`, a reader and
a writer agree on it without coordination.

The repair slice is `[sealed_through - --repair-window-secs, sealed_through]` (default one hour),
because TDengine has no secondary index on `seq` and an unbounded `seq` scan would read the whole
retention window. Correctness across the slice boundary is the writer's job: it periodically
publishes `bar_state.repair_floor_seq` = `MAX(seq)` over every bar whose window is **older** than
the slice. A cursor above that floor therefore proves that every bar changed since the cursor lives
inside the slice; a cursor at or below it returns `410` with `X-Bar-Oldest-Seq: repair_floor_seq+1`,
and the client takes a `klineSnapshot` instead. One aggregate over the slice boundary per minute is
the only extra writer work, and it is measured in milliseconds.

Imported bars never change after import, so they carry `seq=0` and are not returned by `since`. A
client that needs them asks for a time range.

### 8.3 WebSocket

```text
wss://<host>/ws/<symbol>@kline_<interval>
wss://<host>/stream?streams=<s1>@kline_1s/<s2>@kline_1m
```

Protocol is the reference control protocol (`SUBSCRIBE`/`UNSUBSCRIBE`/`LIST_SUBSCRIPTIONS`), with
one addition: on subscribe the server sends

```json
{"e":"klineSnapshot","s":"gbpusd","i":"1m","seq":884422,"bars":[...]}
```

before the live frames. `bars` holds the last `--replay-bars` windows (default 500) ending with the
open bar, read from TDengine. That is the "cold page load has candles" behaviour the market-data
service already implements, but the source is now the shared store rather than a per-process ring;
a reader that joined at 14:00 answers it immediately.

The snapshot is a single TDengine query plus densification. A reader that has not finished
bootstrap refuses the subscribe with a `503`-equivalent close rather than sending a short snapshot.

Every live frame is the reference payload plus `"seq"` and `"rev"` inside `k`:

```json
{"e":"kline","E":...,"s":"gbpusd","k":{...,"x":false,"seq":884431,"rev":0}}
```

`E` follows §5.7: the bar's last trade time for a live bar, the window end for an imported or
synthetic one — never the time the socket happened to write the frame.

CLI rule: a client that sees `seq` jump by more than one has missed updates and should call
`GET /bars?since=<last_seq>`; a jump larger than the retained repair window is a full resubscribe.
Because one market message can sweep several fills into one bar, a `seq` jump by more than one is
normal under bursty flow; the repair call returns the current state either way.

Connection policy follows the reference: 24 h lifetime, ping every 20 s, 5 inbound messages/s, 1024
streams per connection. A `serverShutdown` control frame precedes a planned restart.

### 8.4 Consistency guarantees

| Guarantee | Statement |
|---|---|
| Sealed immutability | Once `X-Bar-Sealed-Through >= T`, no bar with `T' <= T` changes except through a revision, which bumps `rev` and `bar_state.revision` |
| Revision visibility | A revision row and the revised bar are committed before any response can observe the new `rev`; `X-Bar-Revision` lets a cache validate |
| Read-your-writes (WS → REST) | After a client receives a frame with `(seq, rev)`, a REST read of that `openTime` returns a bar with `rev` at least that value |
| Monotonic `seq` | Per `(symbol, interval)`, the stored `seq` is the last `market_seq` that changed the bar and never decreases within a deployment |
| Timestamp provenance | Every `E` is derived from the bar's source — the taker stamp for a live bar, the window end (or the window start for an open empty window) for an import or a synthetic bar — never from the serve clock; re-serving a bar from TDengine preserves its `E` |
| No silent holes | A query for a range that overlaps `bar_gaps`, or extends past `sealed_through` into the past, returns `409` with the bounds, never a silently shorter or flat array |
| Eventual density | Every requested window inside `[listing, sealed_through]` is returned, as a traded or synthetic bar |

## 9. Ingest: archive import

### 9.1 Pipeline

```text
enumerate (S3 ListBucketResult, prefix=data/spot/{daily,monthly}/klines/<SYMBOL>/<interval>/)
   → fetch .zip and .zip.CHECKSUM
   → verify sha256
   → stream-decompress the single CSV (no header; 12 columns)
   → detect the timestamp unit per row (13 digits = ms, 16 = µs)
   → drop the "Ignore" column
   → drop rows with trades == 0 (they re-synthesize on read)
   → set source timestamps only: open_time from the row; last_trade_ts_us and
     first/last trade ids stay None (the archive publishes none of them)
   → clamp to [first_trade, now) — listing-month files start at the listing second
   → validate invariants and monotonicity
   → batch upsert into bars_1s (derive bars_1m and bars_5m), bars_1m or bars_5m
   → record the file in bar_imports (sha256, rows, status)
```

The importer writes only the stored intervals: `1s` files produce `bars_1s` rows and a derived
`bars_1m` rollup; `1m` files produce `bars_1m` rows. `3m` and coarser files are skipped because they
are derived at read time; importing them would create a second source that could disagree with the
`1m` rollup. The file inventory is recorded before the rows, so a crash mid-file is visible and a
re-run is idempotent by hash.

### 9.2 Reference quirks handled

Each of these is a test case in §16, and each comes from a measurement in
[`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII §3:

| Quirk | Handling |
|---|---|
| Milliseconds before 2025, microseconds after | Unit chosen per row from the digit count, never from the file date |
| `1mo` filename suffix versus the API's `1M` | Offset-anchored intervals are derived, so `1w`/`1M` archive files are skipped; the mapping is defined for the export/tests where the suffix appears |
| Twelfth column occasionally non-zero | Parsed and discarded |
| No header row | Header detection is defensive: a non-numeric first field is skipped with a warning |
| Listing-month files start mid-month | Range is clamped; days before the first trade produce no row |
| Files can be replaced | Per-file content hash; a changed hash triggers a merge and a revision record, not a duplicate row |
| Daily files for `1mo` do not exist | Only `monthly/.../1mo/` is requested |
| Empty intervals are present as flat rows | Dropped on import; densification regenerates them identically, which is asserted by the round-trip test |
| Rows carry no last-trade time and no trade ids | Imported bars store `None` for `last_trade_ts_us`, `first_trade` and `last_trade`; the wire `E` falls back to the window end and is never filled from the import clock (§5.7) |
| Decimal text has 8 decimals | Parsed exactly into 18-scale `Fixed`; no `f64` and no rounding |

### 9.3 Scheduling and budget

Monthly files for a validated symbol/interval pair are imported once, oldest first, with a
per-symbol rate limit and a disk-free guard on the staging volume (import stops at 85% usage). Daily
files are imported the day after publication. A symbol-year of `1s` is 365 files, ~2.5 MB each,
~0.9 GB downloaded and ~0.4 GB stored after re-encoding; the measured TDengine bulk ingest
(166k rows/s) makes a year a tens-of-seconds job once the files are local.

`KEEP` must be large enough before the first old file is written. A rejected old row is a hard
error (`bar_import_rejected_total`), not a skipped file.

## 10. Ingest: REST tail

REST is used only for the window between the last archive file and the live feed — typically one
day, and only when the archive is unavailable or the live feed missed a window.

- Cursor: `startTime = last_sealed_open + interval`, `limit = 1000`, repeat until the response is
  short or reaches `now - seal_lag`.
- Budget: `klines` has weight 2 and the published ceiling is 6000 `REQUEST_WEIGHT`/min per IP, so a
  dedicated backfill identity is allocated at most 25% of that (750/min) and stops on the first
  `429`, honouring `Retry-After`. `418` disables the source for the ban duration and raises an
  alert.
- The tail path writes through the same merge rule, marked `source=rest`, so a bar that later
  arrives live with a different value produces a revision rather than a conflict.

## 11. Reconciliation

### 11.1 Three sources, one rule

| Source | Authority |
|---|---|
| Live (trades from the feed) | Authoritative for the current day and for any bar derived from a complete trade prefix |
| Archive | Authoritative for history; can be wrong and can be corrected |
| REST | Cross-check and tail; never authoritative over a sealed live bar without a revision |

Rule: when two sources disagree on a sealed bar, keep the live/derived value until the archive or
REST agrees with itself across a full window and the trade id range is contiguous; then write a
revision with both values. `bar_revisions` is the only place an overwrite is allowed, and it is
append-only. Comparison is over OHLCV, volume and trade count only: an imported bar carries no
`last_trade_ts_us` or trade ids (§5.7), and their absence is not a disagreement — a revision may
fill them in from the live/spool side but never invent them from the source.

A revision re-derives the containing `1s`, `1m` and `5m` rows; coarser intervals follow on read. That is the
main operational win over the segment design, where a base revision had to rewrite every stored
interval that contained the window.

### 11.2 Invariant checks at seal

Every window close re-checks the §5.2 invariants over the window being sealed, plus:

- every stored bar's `last_trade` is greater than the previous stored bar's `last_trade`;
- the re-derived `1m` row equals the rollup of its `1s` children, and `5m` of `1m` (cheap, in memory);
- for a deterministic 1% of windows still inside spool retention, the re-derived bar from trades
  equals the stored bar (hash-selected, so coverage is uniform rather than clustered at recent
  data).

A failed check refuses the seal, leaves `sealed=false`, and raises `bar_seal_failed_total` with the
key — the system prefers "stale but consistent" over "fresh but wrong". A failed `1m` check also
blocks the `1m` row from being marked sealed, even if the `1s` rows are individually correct.

## 12. Capacity and performance

### 12.1 Measured on this host

Environment: TDengine **3.3.6.13**, arm64 container, one dnode, one vgroup, `REPLICA 1`,
`PRECISION 'us'`, `CACHEMODEL 'last_row'`, schema mirroring §7.1 (`bars_1s`, `bars_1m`, `bars_5m`), seven
symbols (`gbpusd`, `eurusd`, `audusd`, `usdjpy`, `eurgbp`, `nzdusd`, `btcusdt`), synthetic integer
random-walk data for 2025-01-02 through 2026-01-01, REST over `taosAdapter:6041`, client round-trip
timing (the server `timing` field read 0 in this version). The data is synthetic, so absolute
throughput is indicative, not a market forecast; the ratios and the exactness results are the
transferable part.

| Operation | Workload | Result |
|---|---|---|
| Bulk ingest `1s` | 7 symbols × 1 day, 362,433 rows | 166,483 rows/s (2.18 s) |
| Bulk ingest `1m` | 7 symbols × 365 days, 3,679,200 rows | 259,508 rows/s (14.18 s) |
| Revision upsert | 5,256 rows | 257,847 rows/s |
| `1m` read, day | 1,440 rows | 6.45 ms p50, 14.9 ms p99; second pass 3.36 ms p50 |
| `1m` read, month | 43,200 rows | 63 ms p50 |
| `1m` read, 3 months | 131,040 rows | 208 ms p50 |
| `1m` read, year | 525,600 rows | 885 ms p50 |
| `1s` read, hour | 2,170 rows | 5.1 ms p50 |
| `1s` read, day | 51,793 rows | 76-114 ms p50 |
| `1m` read, day, 7 symbols | 10,080 rows | 16.7 ms p50 |
| Query-time rollup `1s` → `1m`, day | 51,793 children | 10.6-11.1 ms p50, exact 1,440/1,440 |
| Query-time rollup `1m` → `1h`, day | 1,440 children | 5.4 ms p50 |
| Query-time rollup `1m` → `1d`, year | 525,600 children | 28.4 ms p50 |
| Last 500 `1m` | descending anchor | 4.17 ms p50, 8.64 ms p99 |
| Last 2 days `1m` | 2,880 rows | 7.59 ms p50 |
| Last 300 `1s` | 300 rows | 2.51 ms p50 |
| Latest of 7 symbols | `PARTITION BY symbol` + `LAST_ROW` | 3.17 ms p50 |
| `LAST_ROW` point lookup | one symbol | ~3.9 ms |
| 8 concurrent readers | 1-day `1m` each | 755 queries/s, zero errors |
| `CAST` overhead, 5 decimals | 1-day `1m` | 5.74 → 8.40 ms p50 (+2.7 ms) |
| `CAST` overhead, 7 decimals | 1-day `1m` | 9.97 ms p50 |
| Restart durability | container restart | 525,600 rows and exact `close` survived |

**Disk.** `SHOW TABLE DISTRIBUTED` reports 62.4 MB for 3.68M `1m` rows (17 B/row, 14.7%
compression) and 14.4 MB for 362k `1s` rows (40 B/row, 32.0%). `du` on `/var/lib/taos/vnode`
reported 606 MB, which is preallocated vnode space, not data. At a full 86,400 traded seconds per
day, `1s` is ~1.26 GB per symbol-year; at the measured 60% density it is ~0.76 GB. `1m` is
~8.9 MB per symbol-year. The earlier custom-segment estimate was 0.3-0.5 GB per symbol-year at
`1s`; TDengine is 1.5-2.5× larger on disk but removes the segment codec, catalog, manifest,
checkpoint and reader cache.

**Precision** (the contract in §7.2):

| Probe | Result |
|---|---|
| 38-digit round-trip (`99999999999999999999.999999999999999999`) | exact |
| `SUM` over 51,793 `1s` rows vs the `i128` sum | exact, digit for digit |
| `CAST(… AS VARCHAR)` on a 38-digit value | exact |
| 21 integer digits | rejected, code 317 (`Decimal value overflow`) |
| 19 fractional digits | rounded by TDengine; the ingest guard rejects it first |
| `rust_decimal` parse of a 38-digit value | **lossy**: `100000000000000000000.00000000` |
| Revision round-trip | `close` changed to `9.999999999999999999`, `rev=7`, `source=2` |

### 12.2 Derived query budgets

The 1000-bar compatibility cap bounds every public request:

| Request | Dense bars | Sparse source | Expected p50 |
|---|---|---|---|
| `1s`, 1000 bars | 16.7 min | 1,000 rows | ~2.5 ms |
| `1m`, 1000 bars | 16.7 h | 1,000 rows | ~6.5 ms |
| `1h`, 1000 bars | 41.7 days | 60,000 `1m` rows | ~20-30 ms (estimate) |
| `1d`, 1000 bars | 2.7 years | 1.4M `1m` rows | ~75 ms (estimate from 28 ms/year) |
| `1w`, 1000 bars | 19.2 years | 10.1M `1m` rows | ~0.5 s (estimate; the worst legal case) |

The native `limit` is 10,000, so an internal client can ask for ten times these ranges; the same
linear scaling applies. The `410` repair window is one hour by default, which is 3,600 `1s` rows
or 60 `1m` rows — a single-digit-millisecond query.

### 12.3 What scales and what does not

- **Readers** scale horizontally with no shared state: add a pod, add a connection pool. The
  measured 755 qps on one vgroup is a floor; production spreads vgroups across dnodes.
- **Writers** do not scale horizontally by design (one Lease, one builder). The write load is one
  `1s` row per traded second and one `1m` row per traded minute, plus the open-bar refresh; a
  single writer is far from the measured 166k rows/s ceiling.
- **TDengine** scales by dnodes and vgroups; a vgroup is a Raft group with `REPLICA` copies. The
  lab's single vgroup is the smallest configuration that exercises the schema, not a capacity
  recommendation.
- **The 1000-bar cap** is the real latency bound: it prevents a client from asking for a year of
  `1m` in one request (885 ms) and makes the worst public case a few years of daily bars.

## 13. Failure modes

| Failure | Detection | Behaviour |
|---|---|---|
| TDengine unreachable | connect/query timeout | The writer keeps spooling trades, retries with backoff, `/ready=false`; readers return `503` with `Retry-After`; new WS subscribes are refused, existing live frames continue for the current window |
| Write timeout after commit | Client timeout, unknown outcome | Retry the same idempotent upsert; the second write is a no-op in value terms |
| Partial multi-subtable insert | Statement error on one group | One statement per subtable, so only the failed group is retried; the succeeded groups are not repeated |
| Vgroup leader election | `Redirect`/`NotReady` from the connector | Connector retries; at most one failed query; no data loss (Raft) |
| Duplicate writer | Lease renewal failure | Second writer stops within 15 s (it checks the Lease before each flush); a brief overlap can only flap the open row, which the live writer's next cadence repairs |
| Writer crash mid-window | Lease expiry, `bar_state.writer_epoch` changes | Replacement reads the newest row's cursor and resumes; the open row was already durable; a same-pod restart also replays the spool |
| Trade spool lost or corrupt | Open fails, CRC mismatch, or cursor behind `bar_state` | Record the range in `bar_gaps`, recover from archive/REST where possible, mark `source=rest`, raise `bar_spool_gap_total`; serve `409` for the unrepaired range |
| TDengine data loss on one dnode | Replica down, checksum | `REPLICA 3` serves from another copy; a lost vgroup is a restore from backup (open question) |
| `KEEP` too small | TDengine rejects an old write | Import fails loudly; raise `KEEP` before retrying; never silently skip |
| Decimal overflow | Ingest guard or DB code 317 | Reject the row, increment `bar_decimal_rejected_total`, alert; never write a truncated value |
| 19+ fractional digits | Ingest guard | Reject the source row; the database would round it |
| Clock jump | Wall clock vs event-time watermark | Windows are anchored on event time only; a jump affects metrics and lease timing, not bars |
| Late trade into a sealed window | Trade id below the bar's `last_trade` | Revision path (§6.4): audit, `1s` upsert, `1m` re-derive, `rev+1` |
| Archive file replaced | Content hash change | Re-import that day, diff, revision per changed bar |
| Query for a range older than `KEEP` | `bar_state.first_ts` / retention check | `410` with `Retry-After` and the retention window |
| Query overlapping a known gap | `bar_gaps` overlap | `409` with the gap bounds; never a flat stretch |
| Query beyond `sealed_through` into the past | Watermark comparison | `409`; the writer is behind or the feed is stale |
| `LAST_ROW` cache stale after a leader change | Writer startup only | Fall back to an ordered `ORDER BY ts DESC LIMIT 1`; the live open bar is the writer's row, not a cache guess |
| Schema version mismatch | `bar_meta.schema_version` | Refuse to start; no mixed-version deployment |
| Reader bootstrap race | Buffer overflow or snapshot/seq mismatch | Re-query the snapshot and replay again; never join across a hole |

## 14. Observability

`GET /metrics` exposes, per symbol and interval: `bar_frames_emitted_total`,
`bar_frames_dropped_total`, `bar_engine_mismatch_total`, `bar_seal_total`, `bar_seal_failed_total`,
`bar_seal_lag_us`, `bar_revision_total`, `bar_synthetic_ratio`, `bar_import_bytes_total`,
`bar_catchup_trades_total`, plus the TDengine client counters:

- `bar_db_up` (gauge), `bar_db_reconnects_total`, `bar_db_write_errors_total`,
  `bar_db_write_retries_total`, `bar_db_write_latency_seconds` (histogram),
  `bar_db_query_latency_seconds` (histogram), `bar_db_queue_depth`, `bar_db_queue_full_total`,
  `bar_decimal_rejected_total`, `bar_gap_windows` (gauge), `bar_upsert_rows_total`.

`GET /health` is liveness plus the TDengine connection and schema check. `GET /ready` requires:
feed freshness within `--feed-timeout-secs` (writer and WS-serving readers), seal lag under
`--max-seal-lag-secs`, TDengine reachable, schema version matching, and no gap overlapping now.
`GET /info` advertises the symbol catalogue, interval set, retention window, the `since` repair
floor, `sealed_through`, and `schema: 1` — the client's contract probe.

A single failed TDengine query does not flap `/ready`; the service degrades after
`--tdengine-query-timeout-ms` consecutive failures. The distinction matters because vgroup leader
elections are expected and short.

## 15. Security and operations

- The service is the existing market-data tier: internal, plaintext on a trusted network, with a
  gateway owning TLS and authentication. No order-entry routes exist here, so there is no
  destructive surface.
- TDengine credentials come from a Kubernetes Secret (`--tdengine-password` / `TDENGINE_PASSWORD`);
  the DSN is logged without the password. Production uses `wss://` to `taosAdapter` and a
  NetworkPolicy that admits only market-data pods to 6041. The database port is never exposed
  outside the cluster.
- Per-symbol read authorization is a token claim when a gateway is present; in the lab it is off.
- Query limits: `limit ≤ 1000` (compat) / `10 000` (native), a 1 MiB response cap, 1 s request
  timeout, per-IP token bucket at the gateway.
- The `symbol` parameter is validated against the catalogue before any identifier is built.
  Subtable names are derived from a validated `[a-z0-9_]{1,32}` symbol, never from raw user input.
- Import jobs are read-only against the network and write only through the store API; they run as a
  separate container/identity with no Lease.
- The writer's spool volume is the only bar-related PVC. It holds raw trades for 7 days; it is
  encrypted at rest by the storage class like the engine's own volumes.

## 16. Testing strategy

| Layer | Test |
|---|---|
| Aggregation | Unit: window math across UTC boundaries, month/year boundaries, a trade exactly on a boundary, first trade of a symbol, a trade after a long halt |
| Timestamp provenance | Live: a bar's `E` equals the taker stamp and `t <= E <= T`; imported: `last_trade_ts_us` is `null` and `E == T`; a bar read from TDengine has the same `E` as when it was live |
| Timestamp isolation | Inject a frame whose frame time differs wildly from its trade time and assert the served `E` still equals the bar's trade time (guards the `state.last_ts_ms` regression) |
| Determinism | Property: shuffling trades inside one window yields the same bar; replaying a prefix twice yields identical rows |
| Idempotence | Property: inserting the same row twice leaves one row with identical values; re-importing the same file leaves the table hash unchanged |
| Decimal round-trip | Integration: write `99999999999999999999.999999999999999999`, read through the serving `CAST` path, assert equality with the `i128`; write a value with 19 fractional digits and assert the ingest guard rejects it; assert a `SUM` overflow is an error, not a truncated value |
| No lossy client decimal | Integration: assert the served value of a 38-digit stored value is exact; this fails if the read path is changed to decode `DECIMAL` through `rust_decimal` |
| Rollup exactness | Golden: aggregate the archived `1s` day into `1m` with **traded-only** children and assert 1,440/1,440 rows equal to the reference CSV, field by field; the dense variant must fail on the 29 leading-flat minutes, proving densification never enters a rollup |
| TDengine rollup | Integration: compare the Rust-materialized `bars_1m` against `SELECT … INTERVAL(1m) FROM bars_1s` for a day; both must be 1,440 rows and field-identical |
| Schema | Integration: `CREATE` is idempotent; `bar_meta.schema_version` mismatch refuses to start; `KEEP`/`REPLICA` are verified; the two stable column lists match |
| Upsert | Integration: the open bar's repeated upserts leave one row; a revision changes `rev` and the audit row; a duplicate writer's second write is a value no-op |
| Partial failure | Integration: inject one failing group in a multi-group batch; retry; assert the succeeded group was not duplicated and the failed group is present |
| Gap | Integration: write a `bar_gaps` row and assert a query overlapping it returns `409` with the bounds; repair it and assert the query succeeds |
| Crash | SIGKILL the writer during a flush; restart; reopen must serve either the old or the new row, never a torn value |
| Failover | Kill the writer mid-window; a replacement (same PVC) must replay the spool and produce a bar identical to an uninterrupted run; a replacement with a fresh PVC must resume from the row cursor and produce the same sealed bars |
| Sequence | Drop every Nth frame in the hub; a client using `since` must reconstruct the exact end state |
| API conformance | `GET /klines` output parsed by a Binance client library; defaults, caps, error codes, headers asserted |
| Differential | Live vs REST for one full day per symbol; every difference must be explainable as a revision |
| Load | 1 M bars/day ingest, 1000-bar queries at 200 rps, 8 concurrent readers, WS 5 000 subscribers on one instance |

**How the integration tests run.** A `TDENGINE_URL`-gated suite (default `ws://127.0.0.1:6041`) that
is skipped when the variable is unset, plus a CI service container:

```yaml
services:
  tdengine:
    image: tdengine/tdengine:3.3.6.13
    ports: ["6041:6041"]
    env:
      TAOS_FQDN: localhost
```

Each run creates a unique database (`mdtest_<pid>`) and drops it; tests never share a database and
never point at the lab's `market_bars`. The engine's `cargo test` stays DB-free; only the
`market_data_system` integration target needs the container.

## 17. Rollout plan

| Phase | Scope | Exit criterion |
|---|---|---|
| **P0** | TDengine schema + builder + `1s`/`1m` upserts + `GET /klines` from TDengine + WS snapshot/live | A client (the terminal) reads 500 bars without any `kline_*` frame, and a cold reader that starts at 14:00 answers today's morning |
| **P1** | Revisions, `bar_gaps`, `bar_state`, restart/catch-up, `GET /bars`, exactness tests | Kill -9 the writer; restart; bars and cursor are identical; a 38-digit value survives the serving path |
| **P2** | Archive importer, REST tail, reconciler | A month of BTCUSDT `1s` imported, verified against the archive checksums and the CSV |
| **P3** | TDengine 3-replica cluster + reader `Deployment` + HPA + operator rendering | A reader started mid-day answers today's range; two readers serve the same sealed range after a writer restart; no reader touches the writer's spool |
| **P4** | `KEEP` sizing, backup/restore, export, `MarketDataService.history` GUI status | Restore a dump into a fresh database and serve the same ranges; scale readers 1→4 under load with no writer involvement |
| **P5** | Offset-anchored intervals (`1d`, `3d`, `1w`, `1M`) with time-zone grouping, CSV/Parquet export | A `+08:00` `1d` request matches a hand-computed fixture, and `+05:45` groups from `15m` |

P0 is deliberately shippable on its own: it fixes the terminal's empty-chart case and the
market-data page's blank chart without touching the engine.

The operator change is smaller than the segment design: `MarketDataService.history` gains
`enabled`, `tdengine.url` (Secret reference), `database`, `replica`, `keepDays`, `vgroups`,
`readers` and `autoscaling`; the renderer emits the reader Deployment/HPA and the writer Deployment
with its spool PVC, and points both at the DSN. TDengine itself is installed once per cell
(Helm/operator or `deploy/tdengine.yaml` for the lab), not per `MarketDataService`.

## 18. Decisions

1. **Bars derive from trades, not from the engine's `Bar` messages.** The engine's bar emission is
   incomplete by construction (first emit suppressed, no timer); trades are the durable fact and
   make every interval exact. The engine's bars stay as a diagnostic cross-check.
2. **Sparse storage, dense reads.** Storage keeps only traded windows; one densification
   implementation serves REST, WS and export. This matches measured reference output while keeping
   the store small and the "no trade" fact intact.
3. **Microseconds internally, milliseconds on the reference-shaped wire.** Eliminates the archive's
   mid-history unit switch as an ingest hazard; TDengine is created with `PRECISION 'us'`.
4. **`seq` is the engine `market_seq` that last changed the bar, and `rev` is the bar revision.**
   The two things the reference lacks that make a public kline feed operationally safe; `seq` is
   replica-agreeable without a writer counter.
5. **Sealed means final; corrections are audited revisions.** Never a silent overwrite, never a
   rewritten row without a `bar_revisions` record.
6. **TDengine is the system of record.** No custom segment format, no SQLite catalog, no manifest,
   no open-day checkpoint, no reader cache. The measured read latency, exact decimals and
   three-replica HA make the database the simpler and safer store.
7. **Three materialized series (`1s`, `1m`, `5m`); every coarser interval is a read-time rollup.**
   Multiples of 5 minutes read `bars_5m` when available and fall back to `bars_1m`; `30m` therefore
   rolls up as `1m` → `5m` → `30m`. This is measured, not assumed: `1s` → `1m` traded-only is
   1,440/1,440 against the reference and the 5m/30m chain is asserted field-by-field in unit tests.
8. **Exact decimals end to end.** `DECIMAL(38,18)` on the server, canonical text on write,
   `CAST … AS VARCHAR` on read, a strict 18-scale parser at the edge. `f64` and `rust_decimal` are
   not allowed to decode a bar value; the measured 38-digit loss through `rust_decimal` is the
   reason.
9. **Upsert is the only write; retries are idempotent.** One statement per subtable, last write
   wins, revisions bump `rev`. Partial multi-subtable success is contained by never mixing
   subtables in one statement.
10. **One writer, many stateless readers.** Readers mount no volume, cache nothing and take no
    Lease; a reader restart is a reconnect. This is what makes query scale-out trivial.
11. **The open bar lives in TDengine.** The writer upserts it each cadence, so a reader that joins
    mid-day sees the morning without a checkpoint and a writer restart does not lose the open row.
12. **The trade spool stays local.** TDengine cannot key multiple fills at the same microsecond, so
    the spool remains a timestamped local file. An engine `/trades` endpoint would remove it.
13. **`KEEP` is retention; object storage is backup/export only.** No warm/cold tier on the read
    path, no cache eviction, no manifest.
14. **Extend the market-data service; do not fork it.** The feed decoders, symbol config, Binance
    payloads, sessions, conflation and `/info`/`/ready`/`/metrics` are already correct and already
    deployed. History adds modules to that process, so there is one view of the market and one
    workload to operate.
15. **Compatibility cap 1000, native cap 10 000.** Existing clients see the reference limits; our
    own consumers are not forced to paginate as hard. The cap is also the latency bound (§12.3).
16. **Timestamp provenance is part of the data model, not a formatting detail.** A live bar carries
    the taker's engine intake stamp; an imported bar carries only what its source published, with
    `None` where the source is silent; `E` is derived from the bar and never from the serve clock.
17. **The TDengine version is pinned and integration-tested.** 3.3.6.13 is the measured version;
    the image is `tdengine/tdengine:3.3.6.13` (renamed to `tdengine/tsdb` from 3.3.7.0). A schema
    or decimal-behaviour change is a new pin with a re-run of §12 and §16, not an automatic upgrade.

## 19. Open questions

1. **Calendar intervals**: is a `1M` bar anchored to UTC or to the symbol's exchange-local day? The
   reference supports both via `timeZone`; we support UTC on disk and the offset at query time.
   TDengine `INTERVAL(1n, offset)` handles a negative time zone directly; a positive time zone is
   edge-grouped from `1h`/`15m` because the local month starts on the previous UTC day.
2. **Engine ask: event timestamps.** `EventsResponse` exposes `MatchEvent` without `ts_ms`, so
   remote catch-up from `/events` cannot place a fill in a window and the writer must keep its own
   spool (§4.5). Adding the applied-entry `ts_ms` to the event page — or a `/trades` endpoint that
   returns timestamped fills — would make the spool optional and a cold restart possible from any
   node. This is the one engine change the design would benefit from; it is not required for
   P0-P2.
3. **TDengine version pin.** 3.3.6.13 is what was measured; 3.3.7.0 renamed the image to
   `tdengine/tsdb` and may change decimal/interval details. Decide the upgrade cadence and whether
   the lab tracks the production pin.
4. **Vgroup and replica sizing.** The measured run used one vgroup and one replica. Production
   needs three dnodes/three mnodes and `REPLICA 3`; the vgroup count should be sized to the host
   count and the `1s` ingest rate, and re-checked with the same benchmark on the real cluster.
5. **Backup and disaster recovery.** `taosdump` is logical and can be slow for a `1s` year; a
   storage-level snapshot depends on the deployment. Define RPO/RTO, whether `1s` is backed up in
   full or reconstructible from `1m` + archive, and how a restore is validated.
6. **`KEEP` sizing.** 10 years is a default, not a decision. A full archive import from the
   listing date and a 20-year retention target change it; `ALTER DATABASE … KEEP` is upward-only
   and must happen before the first old write.
7. **Connector choice and pooling.** The `taos` Rust connector supports pure WebSocket
   (`features = ["ws"]`) and avoids a C client. Confirm the production pool size, reconnect
   behaviour, and whether the connector's `Value::Decimal` is ever reached (the serving path casts
   to `VARCHAR` so it should not be).
8. **`1m` materialization vs pure read-time rollup.** Materializing `1m` costs a second write path
   but halves the day-query latency (6.5 ms vs 11 ms) and gives every coarser query a cheap source.
   Confirm the trade-off on the real symbol set; dropping `bars_1m` is a schema change, not a
   query flag.
9. **`seq` across cross-shard migration.** `market_seq` is monotonic within a deployment; a
   migration or restore may start a new base. The engine's `epoch` field could make the boundary
   explicit. Decide whether the WS should reset `seq` on an epoch change or keep a monotonic
   composite.
10. **Trade retention for re-derivation.** Revisions need the trades of a sealed window. The spool
    keeps 7 days; older revisions use the archive/REST and are marked `source=rest`. Confirm
    whether 7 days is enough for the operational correction window.
11. **Futures-style intervals** (`markPrice`, `indexPrice`, continuous contracts): out of scope
    until a derivative product exists.
12. **Should the engine keep `Bar` frames at all** once the historical path is authoritative?
    Current answer: keep them as a heartbeat/diagnostic and mark them explicitly non-durable in the
    wire docs.
13. **TDengine on the OrbStack lab.** A three-dnode/three-mnode cluster may be too heavy for the
    single-node lab; the lab can run one dnode with `REPLICA 1` while production runs three. The
    operator should make that a documented profile, not an accident.

## 20. References

- TDengine data model, decimal, interval, `LAST_ROW`, `KEEP`, `CACHEMODEL` and the `taos` Rust
  connector:
  <https://docs.tdengine.com/tdengine-reference/sql-manual/data-types/>,
  <https://docs.tdengine.com/tdengine-reference/sql-manual/interval/>,
  <https://docs.tdengine.com/tdengine-reference/sql-manual/functions/>,
  <https://docs.tdengine.com/tdengine-reference/sql-manual/database/>,
  <https://docs.tdengine.com/tdengine-reference/client-libraries/rust/>.
- TDengine container images: `tdengine/tdengine:3.3.6.13` (measured); `tdengine/tsdb` from
  3.3.7.0.
- Binance kline stream and payload:
  <https://github.com/binance/binance-spot-api-docs/blob/a049a2b37d/web-socket-streams.md>
- Binance `GET /api/v3/klines`, `uiKlines`, `aggTrades`, `historicalTrades`:
  <https://github.com/binance/binance-spot-api-docs/blob/adfd443f85/rest-api.md>
- Binance WebSocket API `klines`:
  <https://github.com/binance/binance-spot-api-docs/blob/907657c4d0/web-socket-api.md>
- Public data archive and its formats:
  <https://github.com/binance/binance-public-data/tree/d571690eb6>
- This project's measurements and gap analysis:
  [`THIRD_PARTY_INVESTIGATION.md`](./THIRD_PARTY_INVESTIGATION.md) Part VII
- Engine feed: [`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md)
- Hub and subscribe-time replay: [`MARKET_DATA_WEBSOCKET.md`](./MARKET_DATA_WEBSOCKET.md) §4.6
- Replicated event log used for catch-up: [`MATCH_RESULTS.md`](./MATCH_RESULTS.md)
- Scale-18 `Fixed`: [`DECIMAL.md`](./DECIMAL.md)
- Engine storage precedent (no longer used for bars): [`STORAGE.md`](./STORAGE.md)

