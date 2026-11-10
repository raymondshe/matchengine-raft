# API Reference

## HTTP and gRPC surface

The HTTP application and management endpoints speak JSON. Every handler returns a `Result`
wrapper, so a successful call looks like `{"Ok": ...}` and a forwarded or failed call like
`{"Err": ...}`. Raft peer RPCs use gRPC, not the HTTP JSON API.

### Raft gRPC API

Raft peer-to-peer RPCs are served only by gRPC on `--http-addr` port + 1000. No Raft RPC is
mounted on the HTTP JSON listener:

- `AppendEntries` and `Vote` (unary)
- `InstallSnapshot` (client-streaming chunks)
- `AppendStream` (bidirectional pipelined appends)
- `TransferLeader` (graceful leadership handoff used by the console)

See `proto/raft.proto`. Unary append/vote use `RPCOption::soft_ttl`, snapshots stream in
OpenRaft-configured chunks (3 MiB default, 8 MiB cap), and both message directions are capped at
64 MiB. `Unavailable` maps to
`RPCError::Unreachable`, timeouts to `RPCError::Timeout`, and other statuses to
`RPCError::Network`.

### Admin APIs

- `POST /init` - Initialize a cluster; body is `[]` for the receiving node alone, or
  `[[1, "127.0.0.1:21001"], ...]` for an explicit membership
- `POST /add-learner` - Add a learner node; body is `[id, "host:port"]`
- `POST /change-membership` - Change the voter set; body is `[1, 2, 3]`
- When `RAFT_WAL_FSYNC_MS` is unset, the binary requires at least four voters: `/init` and
  `/change-membership` reject smaller sets, and `/write`/`/write_batch` return `503` until four
  voters exist.
- `GET /metrics` - Raft metrics plus OpenRaft `data_metrics`, `server_metrics` and process-local
  `transport_metrics` (heartbeat/replication `AppendStream` streams and requests, snapshot
  send/receive count, bytes, average and max microseconds). With `--features profiling`,
  `runtime_stats` carries the compact runtime histogram summary; without it the field is `null`.
  The existing `RaftMetrics` fields stay flattened at the top level inside the usual `{"Ok": ...}`
  envelope.
- `POST /get_linearizer` - Fetch a read linearizer from the leader (used by `/follower_read`)

### Operational APIs

- `GET /health` - Liveness: reports the gRPC listener, Raft role/leader, WAL and state-machine
  frontier. Returns `200` while the listener, Raft core and storage are usable, `503` otherwise.
- `GET /ready` - Readiness: returns `200` only when the node is healthy and a cluster leader is
  known; `503` during startup, elections or a partition. A follower with a known leader is ready
  for reads; writes still go to the leader.
- `GET /pretrade/status` - Pre-trade diagnostic: always returns `200` with the applied
  `symbol_config`, `symbol_locked`, open order/stop counts, `sequence`, health/leader/recovery
  flags and the `reasons` pre-trade readiness is false. It is the probe behind the client
  provisioning helper; `/ready` remains the operational gate.
- `GET /read-policy/status` - Configured leader-read policy and per-endpoint lease/ReadIndex counters
- `GET /admission/status` - Per-node write admission limit and current/peak/admitted/rejected counters
- `GET /console/` - Embedded operational console (HTML/CSS/JS); no build step or external assets
- `GET /console/api/node` - Stable node snapshot: health, Raft role, storage frontier, events and feed
- `GET /console/api/cluster` - Local node plus peer snapshots from the current membership
- `POST /console/api/actions/transfer-leader` - Graceful leadership transfer (leader-only)
- `POST /console/api/actions/elect` - Best-effort local Pre-Vote election request
- `POST /console/api/actions/snapshot` - Trigger a local snapshot
- `POST /console/api/actions/purge-log` - Purge this node's log up to a snapshot-covered index
- `POST /console/api/actions/runtime-config` - Toggle node-local OpenRaft runtime switches
- `GET /console/api/actions` - Recent in-memory console actions

Console actions require an `X-Console-Action: 1` header and are serialized. The console inherits
the internal-only HTTP contract; TLS and operator authentication stay at the gateway.

### Application APIs

- `POST /write` - Submit a `Place`, `Cancel`, `Amend`, `PurgeExpired` or `ConfigureSymbol` request
  through Raft. Order/mutation requests return the assigned `market_seq` (for a `Place`, also the
  order sequence) plus the entry's first events (capped at 512). `ConfigureSymbol` returns a status
  string and uses the `error` field on refusal; it does not consume a market sequence. `Place`
  accepts an optional `client_order_id` for idempotent retries. A `Batch { requests }` envelope is
  also accepted; `/write_batch` takes the same requests as a bare JSON array.
- `POST /write_batch` - Submit 1–128 non-nested requests in one Raft entry. The response contains a
  `batch` array with one response per request; event ordinals are continuous across the entry.
- `/write` and `/write_batch` share a per-node in-flight admission limit (default **1,024**
  requests). At the limit the edge returns `429 Too Many Requests`; clients should retry with
  backoff. The limit counts requests, not orders, so one 128-order batch consumes one slot.
  Lower it to `512` for a tighter tail at roughly 1,024 concurrent clients; `--max-inflight-writes
  0` disables rejection. `GET /admission/status` reports the limit, current in-flight count, peak,
  admitted and rejected totals.
- `POST /read` - Read from the local state machine (may be stale)
- `POST /linearizable_read` - Linearizable read; the request must reach the leader
- `POST /follower_read` - Linearizable read served locally after fetching a linearizer from the leader
- `POST /events` - Read a page of the replicated match-event log (`after`, `limit`, `wait_ms`); linearizable and leader-only, with optional long-poll
- `GET /events/status` - Retained event window, eviction counters and retention limit
- gRPC `EventService.StreamEvents` - Stream JSON event pages with the same cursor, `wait_ms` and `max_pages` on the gRPC listener

#### Read consistency and lease reads

`POST /read` is a local, possibly stale read and is not affected by the read policy. The
leader-consistent paths default to `local_lease`: `/linearizable_read`, `/events`, the gRPC
`StreamEvents` anchor and `/get_linearizer` (used by `/follower_read`) use OpenRaft's caller-side
lease cache, then the core `ReadPolicy::LeaseRead` path. A lease read reuses a recent quorum
acknowledgement and avoids the fresh round while the leader lease is valid. If the lease is stale,
the node falls back to `ReadIndex`; if quorum is available the read succeeds at fallback latency,
otherwise it returns the Raft read error.

Start the node with `--read-policy read_index` (env `MATCHENGINE_READ_POLICY=read_index`) to force a
fresh quorum round on every read, or `--read-policy lease` to keep the core lease path without the
caller-side cache. The local path measured 11-36% higher read throughput and ~40% lower 64-reader
p50/p99 on the 4-node benchmark.

Lease and local-lease reads assume **bounded clock drift** between nodes. Use NTP or PTP; set
`--read-policy read_index` unless the deployment guarantees that the leader's lease cannot outlive
its actual leadership. A local-lease cache hit can overtake an unfinished write; await the write
response before a read that must observe it. Client retries are **at-least-once**: set
`client_order_id` on writes that must not duplicate, and the replicated submission map returns the
original sequence and events. Downstream event consumers are expected to be idempotent: resume from
`(log_index, ordinal)` and dedupe on `event_id`; local-lease reads do not change either contract.
The status endpoint reports the mode and, per path, `lease_attempts`, `lease_hits`,
`lease_fallbacks`, `lease_failures`, `read_index_attempts` and `read_index_failures`.

The Rust request/response types behind `/write` are:

```rust
pub enum MatchEngineRequest {
    Place { order: Order, client_order_id: Option<String> },
    Cancel { order: Order },
    Amend { order: Order, price: Option<Fixed>, volume: Option<Fixed> },
    PurgeExpired { now_ms: u64, day_close_ms: Option<u64> },
    ConfigureSymbol { config: SymbolConfig },
    Batch { requests: Vec<MatchEngineRequest> },
}

pub struct MatchEngineResponse {
    pub value: Option<String>,        // sequence string for compatibility
    pub sequence: Option<u64>,        // assigned market_seq
    pub events: SmallVec<[MatchEvent; 1]>, // first 512 events; one inline, serializes as an array
    pub events_truncated: bool,       // true when more events exist
    pub error: Option<String>,        // non-fatal application error
    pub batch: Vec<MatchEngineResponse>, // per-request responses for /write_batch
}
```

Every order/mutation request consumes one `market_seq`; `Place` also uses it as the order
sequence, while `ConfigureSymbol` does not consume one. The response carries at most 512 events;
the full batch is available from `/events`. Fill events carry
`quote_volume`, `volume_unit`, `taker_fee`/`taker_fee_asset` and `maker_fee`/`maker_fee_asset`;
the same fields are on market-data `TRADE` and fill `LIFECYCLE` records (wire v7).
`client_order_id` is optional and only affects `Place`: a retry returns the original sequence and
events instead of placing a second order. It must be 1–128 bytes from
`[A-Za-z0-9._:-]`; an empty value is rejected, so omit the field to disable idempotency.

`/write_batch` executes sub-requests in order in one `apply` call, so later items see the effects of
earlier ones. Each item keeps its own sequence and events; all events share the entry's log index
with continuous ordinals. A rejected sub-request does not abort the rest of the batch. Empty
batches, nested batches, oversized batches and duplicate `client_order_id`s within one batch are
rejected before Raft.

`/read`, `/linearizable_read` and `/follower_read` accept a JSON string key naming a view of the
order book:

- `orderbook_orders` - all orders resting in the book as a JSON array
- `orderbook_stops` - the stop orders parked off the book, in admission order
- `orderbook_last_trade_price` - the last trade price (`null` before the first trade), which elects stops
- `orderbook_depth` - aggregated visible depth for the top 10 price levels per side
  (`orderbook_depth:N` selects another count, capped at 200)
- `orderbook_spread` - best ask minus best bid (`null` while either side is empty)
- `marketdata_status` - market-data feed counters and gate state (`{}` when the feed is off)
- `orderbook_sequence` - the order sequence counter (`orderbook_sequance` is accepted as a legacy
  alias)
- `symbol_config` - this process/cluster's replicated symbol and precision configuration
- `symbol_id` - the one symbol this process/cluster trades

Prices and quantities are stored internally as exact 18-scale fixed-point integers. JSON accepts
either decimal strings or legacy numbers; match events, `/read` and the market-data feed emit
decimal strings so 18-decimal values round-trip exactly. A process/cluster trades exactly one
symbol: `SymbolConfig` is replicated and snapshotted with base/quote `AssetId`/`AssetClass`
metadata, price tick, base lot and fee-asset policy. Prices/quantities with excess precision or
wrong increments are rejected. On the leader, `/write` and `/write_batch` reject an order whose
`symbol_id` does not match the configured symbol with `400` before the Raft proposal; the state
machine still re-checks it as defense in depth. Scaling to several symbols means running one
process/cluster per symbol, so resources and failures stay isolated. Values are normalized to the
common 18-decimal scale.

Example order payload accepted by `/write` (note the field name `sequence`):

```json
{"Place": {"order": {"live": "Limit", "side": "Buy", "volume_unit": "base", "price": "4000.25", "volume": "4.4", "maker_fee_rate": "0.001", "taker_fee_rate": "0.002", "id": 1, "sequence": 1}, "client_order_id": "order-123"}}
```

`client_order_id` is optional; omit it for the old non-idempotent behaviour. `volume_unit` is
`base` by default; `quote` makes `volume` a gross quote amount (limit orders normalize at the limit
price, market orders use a quote budget/proceeds sweep).

Set this process's symbol, assets and accepted precision before trading:

```json
{"ConfigureSymbol": {"config": {
  "symbol_id": 0,
  "base_asset": {"code": "QQQ", "class": "etf", "decimals": 4},
  "quote_asset": {"code": "USD", "class": "fiat", "decimals": 2},
  "price_decimals": 2,
  "price_tick": "0.01",
  "base_lot": "0.0001",
  "fee_asset_policy": "quote",
  "stp": "off"
}}}
```

The configuration is replicated and locked once the first order is accepted. Changing it while
orders are open, or after trading has started, is refused. A successful change on a publishing
node also queues a market-data reset snapshot for the new symbol. The response uses the optional
`error` field for this non-fatal case; the write itself is still committed. `stp` defaults to
`"off"`; when enabled, the gateway must populate `order.owner_id`.

### Provisioning helper and pre-trade readiness

`MatchEngineClient` provides the normal provisioning flow:

```rust
let config = SymbolConfig::new(7, 2, 2)?;
let status = client.provision_symbol(config.clone()).await??;
assert_eq!(config, status.symbol_config);

// Wait until the node is healthy, recovered, has a leader, is unlocked and
// has an empty book.
let ready = client
    .wait_for_pretrade_ready(&config, Duration::from_secs(15))
    .await??;
assert!(ready.ready);
```

`provision_symbol` validates the configuration locally, refuses a locked symbol or non-empty
book, writes `ConfigureSymbol` only when the applied configuration differs, and verifies the
applied config afterwards. `wait_for_pretrade_ready` polls `GET /pretrade/status`; a configuration
mismatch fails immediately, while transport errors and `ready == false` are retried until the
timeout. Both return an outer transport error and an inner application error, matching
`write`/`write_batch`. `GET /pretrade/status` is local to the serving node; point the client at
the leader (or let the write path forward) before enabling order entry.

Fee rates (`maker_fee_rate`, `taker_fee_rate`) are signed `Fixed` fractions supplied by the
gateway from the user level. `"0.001"` means 10 bps and negative values are rebates. They are
optional and default to zero. The engine computes each fill's notional from the maker price and
matched quantity, then charges the taker order's taker rate and the maker order's maker rate.
Fees are truncated toward zero at 18 places; a fill carries an ordered `error` set
(`notional_overflow`, `taker_fee_overflow`, `maker_fee_overflow`) when a dependent value exceeds
the supported fixed-point range. The affected numeric field is `0` when its code is present, so a
zero is authoritative only when the code is absent; Rust consumers can use the
`MatchResult`/`MatchEvent` `exact_*` accessors. Nullable fields were rejected to keep the snapshot
v11/wire v7 contract. The engine records fees but does not move balances: the order
system freezes the trading asset and clearing deducts fees from that freeze.

Base/quote volume semantics and asset-aware fee deduction for fiat, stablecoin, crypto, ETF and
equity pairs are implemented. ETF/equity bases always settle fees in the quote asset; crypto/spot
and fiat-to-fiat pairs default to the received asset, and crypto can explicitly select quote or
base. Quote volume is limited to plain limit and market orders; quote icebergs and FOK are rejected
at admission. The full model is in [`docs/FEE_DEDUCTION.md`](./FEE_DEDUCTION.md); third-asset fee
tokens and derivatives remain outside the engine, and sessions, settlement, corporate actions and
fixed/regulatory/minimum/tax fees are owned by the external order system.

Every other order-type field (`tif`, `post_only`, `stop_price`, `display_volume`, `hidden_volume`,
`timestamp`) is optional and defaults to "plain GTC limit order":

```json
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": 4000.0, "volume": 100.0, "tif": "IOC", "display_volume": 10.0}}}
```

The matching engine overwrites `sequence` when it applies the order, and the receiving leader
stamps `timestamp` at intake, so both values clients send are ignored (`0` is the conventional
placeholder). `sequence` is optional and defaults to `0`.

Order identity has three separate fields:

- `id` is client-supplied metadata. It is not a book key, is not checked for uniqueness, and
  duplicate `id`s are allowed.
- `sequence` is assigned by the engine on apply; use the returned sequence to key a later
  cancel/amend. A book order is identified by `(side, current price, sequence)`; a parked stop by
  `(side, sequence)`. A client-sent `sequence` on `Place` is ignored.
- `client_order_id` is a `Place`-only idempotency key, retained for the last 262,144 submissions.
  A retry inside that window returns the original sequence and events; after eviction it is a new
  order. It never identifies a resting order for cancel/amend.

A `Cancel` or `Amend` whose key does not match a resting order still consumes one `market_seq` and
publishes a `Rejected` event with reason `order_not_found`. `/write` returns `200` because the
request is committed; the event, not the HTTP status, is the authoritative result.

### Schema-v11 order types

Snapshot v11/wire v7 add four optional order features. They default off, so an order that omits the
new fields behaves exactly like v10. The full semantics are in
[`docs/ORDER_TYPES_V11.md`](./ORDER_TYPES_V11.md); the API fields are:

- `market_to_limit` (`bool`): a `Market` order that sweeps and then rests any remainder at the last
  fill price. `price` is ignored on entry; the rested remainder becomes an ordinary `Limit` and the
  flag is cleared. No fill cancels the whole order. IOC/FOK, post-only, stops, icebergs and quote
  volume are rejected with this flag.
- `trail_offset` (`Fixed`): a positive trailing distance for a stop. `stop_price` must be `0` on
  entry; `trail_reference` is engine-managed (client values are overwritten) and persisted for
  restart. The current derived trigger is reported in `stop_price`.
- `peg` (`"primary" | "midpoint" | "last_trade"`) and `peg_offset` (signed `Fixed`): a resting
  limit whose price is derived from non-pegged liquidity. `price` must be sent as `0`. A change in
  the reference re-prices the order with a new sequence and an `amended` event; a target that would
  cross is cancelled with `peg_would_cross`; a missing reference is `peg_reference_unavailable`.
- `owner_id` (`u64`, optional): the trader identity used by self-trade prevention. The symbol's
  `stp` policy is `off` (default), `cancel_resting`, `cancel_aggressor` or `cancel_both`. STP only
  matches two non-null owner ids; anonymous orders never self-trade-prevent.

Examples:

```json
{"Place": {"order": {"live": "Market", "side": "Buy", "volume": "5", "market_to_limit": true, "sequence": 0}}}
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": "99", "volume": "1", "trail_offset": "1", "sequence": 0}}}
{"Place": {"order": {"live": "Limit", "side": "Buy", "price": "0", "volume": "2", "peg": "primary", "peg_offset": "0", "sequence": 0}}}
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": "100", "volume": "1", "owner_id": 42, "sequence": 0}}}
```

For a pegged `Amend`, `price` is refused because it is derived. A non-zero `order.peg_offset` in the
amend key changes the offset; `0` keeps the current offset (cancel and place to reset it to zero).
A trailing-stop amend uses `order.trail_offset` for the new offset; echoing the current derived
`order.stop_price` is accepted, while a different non-zero trigger is refused because it is derived. Both still consume one market sequence and publish
`rejected` on refusal.
