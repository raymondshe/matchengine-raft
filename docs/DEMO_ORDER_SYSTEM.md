# Demo order system (WebUI) design

Status: **proposed** — design only; no demo routes or assets are implemented yet.

Last updated: 2026-11-03.

Related documents: [`OPERATIONAL_CONSOLE.md`](./OPERATIONAL_CONSOLE.md),
[`MATCH_RESULTS.md`](./MATCH_RESULTS.md), [`FEE_DEDUCTION.md`](./FEE_DEDUCTION.md),
[`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md), [`AGENTS.md`](../AGENTS.md).

## 1. Summary

Add a small **demo order desk** at `/demo/` that runs as a **separate process** from the
matchengine-raft node and demonstrates the complete trading workflow against it:

1. configure one symbol before trading starts,
2. place, cancel and amend orders,
3. watch the visible order book and the order's lifecycle,
4. read the resulting match events from the replicated `POST /events` log with a cursor.

It is a **demo and developer tool**, not a production trading terminal. It is a separate surface
from the operational console: the console remains read-mostly, while the demo intentionally writes
orders. The engine process is not modified; the demo connects to it over the public HTTP API.

The design reuses the operational console's architecture:

- static, dependency-free HTML/CSS/JavaScript embedded in the **demo binary**,
- a separate Actix listener serving the demo assets and a thin BFF,
- stable server-side schemas instead of exposing raw OpenRaft internals,
- the console's theme, shell, panels, tables, chips, buttons, forms and event viewer,
- the leader-aware `MatchEngineClient` for writes and event reads.

## 2. Goals and non-goals

### Goals

- Demonstrate the **order-to-event** workflow in one browser page:
  - submit a valid order,
  - see the write response, sequence and immediate events,
  - see the book/order tables update,
  - see the authoritative events arrive from `/events`.
- Make the event log workflow tangible: cursor pagination, `next_after`, `has_more`, retention
  window, deduplication and gap handling.
- Show the **complete** resting book on demand for debugging: every resting order, every price
  level and every parked stop, while keeping the normal polling view small.
- Reuse the console's CSS and visual language rather than building a second design system.
- Stay usable on a single node and on a cluster: writes and event pages follow the leader; reads
  show the serving node's applied state with an explicit follower banner.
- Keep the demo self-contained: no npm, no CDN, no external fonts, no chart library.
- Keep the matching path untouched: the demo only calls existing APIs and reads replicated state.

### Non-goals

- Public internet exposure, TLS, authentication or per-user authorization.
- Balances, settlement, accounts, positions or risk checks. The engine reports fees; it does not
  hold funds. The demo must not pretend otherwise.
- Multi-symbol trading. One process/cluster = one symbol, per the engine's contract.
- Historical charts, market-data Aeron integration, long-term storage or reporting.
- Replacing the operational console. The console answers "is the cluster healthy?"; the demo
  answers "what does an order do?".
- A reset/truncate button. Resetting a demo means restarting with a fresh `--data-dir`.

## 3. Constraints and current state

- The node already serves the HTTP API and the embedded `/console/` UI on `--http-addr`.
- The listener is plaintext and unauthenticated by design; TLS and operator auth belong to the
  gateway. The supported deployment is loopback or a trusted private network.
- One process/cluster trades exactly one symbol. `SymbolConfig` is replicated and locked after the
  first accepted order. The demo must expose this lock clearly.
- `POST /write` accepts a `MatchEngineRequest` and returns the OpenRaft `ClientWriteResponse`.
  `POST /write_batch` accepts 1–128 non-nested requests in one entry.
- `POST /read` takes a JSON string key and returns a JSON **string** (structured values are
  double-encoded). Relevant keys: `symbol_config`, `orderbook_orders`, `orderbook_stops`,
  `orderbook_depth[:N]`, `orderbook_sequence`, `orderbook_spread`,
  `orderbook_last_trade_price`, `marketdata_status`.
- `POST /read orderbook_orders` returns every resting order, but `orderbook_depth[:N]` is capped at
  200 levels. The demo process computes full depth from the full order list, so the debug view is
  not limited to that top-N depth.
- The demo runs separately: it does not read the engine's state machine directly. It calls
  `POST /read` and `POST /events` over HTTP. `symbol_locked` is not a `/read` key, so the demo
  treats `orderbook_sequence > 0` as "trading has started" and disables configuration.
- `POST /events` returns `EventsResponse { events, next_after, oldest_event_id, last_event_id,
  has_more }`; the page size is 1–1000 and the read is leader-gated. `GET /events/status` returns
  retention counters.
- `MatchEngineClient` already implements leader discovery, forwarding, retries and typed errors for
  writes, events and linearizable reads.
- The operational console is implemented in `src/network/console.rs`, with assets in
  `web/console/{index.html,app.css,app.js}`. Its CSS is a 333-line dark theme with reusable
  variables and classes.
- The console's stated non-goal "no order entry" still holds: the demo is a separate, opt-in
  process with its own listener and URL namespace.

## 4. Users and scenarios

| User | Scenario |
|---|---|
| Developer | Exercise a new order type or fee configuration and watch the resulting events. |
| QA / support | Reproduce a reported order and inspect the exact sequence, log index and event fields. |
| Sales / demo | Show a complete place → match → event workflow on a laptop. |
| Operator | Verify that the replicated event feed matches the book after a restart or failover. |

Primary flow:

1. Open `http://<demo-host>/demo/` (the demo process, pointed at a node).
2. The desk loads the node/leader state, symbol configuration and current book.
3. If trading has not started, the operator reviews or configures the symbol.
4. Place a resting order; it appears in the book and the order table.
5. Place a crossing order; the fill, fees and lifecycle events appear in the event tape.
6. Watch the cursor advance, pause/follow, filter by kind/side/order, and verify `next_after`
   stays stable across refresh and failover.

## 5. Architecture

### 5.1 Components

```text
browser
   │  demo process: static HTML/CSS/JS
   ▼
demo Actix listener (demo/)
   ├── /demo/                  embedded assets
   ├── /demo/api/state         /read snapshot + Raft metrics
   ├── /demo/api/book          full /read order list, depth computed in the demo
   ├── /demo/api/events        leader-aware event pages
   ├── /demo/api/orders        leader-aware Place
   ├── /demo/api/orders/cancel leader-aware Cancel
   ├── /demo/api/orders/amend  leader-aware Amend
   └── /demo/api/symbol        leader-aware ConfigureSymbol (before lock)
          │
          ▼
MatchEngineClient (leader tracking, forwarding, retries)
          │  HTTP/JSON
          ▼
matchengine-raft node (separate process): Raft / state machine / replicated event log
```

The demo is a thin **backend-for-frontend (BFF)** in its own binary. It does not implement matching,
ordering or event storage; it validates a small demo DTO, builds the existing
`MatchEngineRequest`, and calls the existing public API through `MatchEngineClient`. State is
assembled from `POST /read` and `GET /metrics`; the engine process is untouched.

### 5.2 Why a BFF instead of browser → `/write` directly?

- Writes and `/events` must reach the leader. `MatchEngineClient` already forwards and retries;
  duplicating that logic in JavaScript would be fragile and expose leader details to the browser.
- The browser would need to double-parse `/read` JSON strings and make 6–8 requests per refresh.
- A stable `/demo/api/state` schema decouples the UI from raw `/read` key names and OpenRaft
  response envelopes, matching the console's `NodeSnapshot` approach.
- The BFF can enforce demo-only caps (depth, open-order count), validate the demo ticket and add
  the write-action header check in one place.

The BFF is **not** a new consensus or storage path; it is a small facade over the existing API and
the leader-aware client.

### 5.3 Reuse of the operational console

| Console piece | Reuse in the demo |
|---|---|
| Embedded `include_str!` assets | Same pattern; no external files at runtime |
| `/console/api/node` stable schema | `/demo/api/state` follows the same "do not expose raw Raft types" rule |
| Cluster-aware leader/readiness chips | Topbar shows node, leader, term, ready, follower warning |
| `web/console/app.css` theme | Demo serves `include_str!("web/console/app.css")` + its own layer at `/demo/app.css` |
| Panels, tables, chips, badges, buttons, filter bar, pager, banners | Reused directly |
| Event viewer markup and filters | Reused layout; the demo upgrades it to cursor-follow + dedupe |
| CSP, DOM `textContent` rendering, no-CDN policy | Same |
| Same-origin, gateway-protected trust model | Same |

### 5.4 Process and enablement

The demo is a standalone crate at `demo/` with its own `Cargo.toml`, binary and embedded assets.
It is never compiled into the matchengine-raft node. Start the engine first, then the demo:

```shell
# engine process (initialized once)
cargo run --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:24301 \
  --data-dir /tmp/matchengine-raft-demo --instance-prefix demo

# demo process
cargo run --manifest-path demo/Cargo.toml -- \
  --addr 127.0.0.1:24300 --engine 127.0.0.1:24301 --engine-id 1
```

- `--addr`: demo WebUI/BFF address.
- `--engine`: HTTP address of the running node.
- `--engine-id`: that node's Raft id, used for role display and client forwarding.

The engine needs initialization (`POST /init` with `[]`) and a leader before orders are accepted;
the demo can start earlier and shows a "no leader" banner until then. The demo process is
loopback/private-network-only, same as the engine; TLS and authentication remain gateway concerns.

## 6. UI design

### 6.1 Layout

A single page would be too dense, so the desk reuses the operational console's radio-tab pattern.
The topbar, page head and demo banner stay shared; four tabs split the detail:

| Tab | Content |
|---|---|
| **Trade** | Instrument, depth ladder, open orders and parked stops (left) + order ticket and result (right), with a compact recent-events strip below. |
| **Full book** | The complete order-level bid/ask book, all aggregated levels and parked stops (debug view). |
| **Events** | Full-width cursor tape with filters and pager, plus a selected-event detail card. |
| **Symbol** | Symbol configuration/lock, asset metadata, tick/lot, fee policy, node and event-window status. |

```text
┌─ topbar: brand · node · leader · term · ready · symbol · updated · pause ─┐
├─ page head: "Order desk" · follower/leader banner · refresh ──────────────┤
│ [ Trade ] [ Full book ] [ Events ] [ Symbol ]                           │
├──────────────────────────────────────────────────────────────────────────┤
│ Trade:  ┌───────────────────────┐  ┌───────────────────────────────┐     │
│         │ Instrument / book     │  │ Order ticket                  │     │
│         │ BBO · depth           │  │ side / type / TIF · price     │     │
│         │ open orders / stops   │  │ volume · submit · result      │     │
│         └───────────────────────┘  └───────────────────────────────┘     │
│         ┌────────────────────────────────────────────────────────────┐   │
│         │ Recent events · link to Events tab                         │   │
│         └────────────────────────────────────────────────────────────┘   │
└─ footer status: book sequence · last trade · event window · last update ──┘
```

- **Trade** is two columns: `minmax(0, 1.1fr) | minmax(360px, .9fr)`. At < 900 px it becomes one
  column. The recent-events strip spans the tab.
- **Full book**, **Events** and **Symbol** use the full page width. Events uses
  `minmax(0, 1fr) | 330px` for the tape and detail card; at < 1100 px the detail moves below.
- Tabs are plain radio inputs + labels, identical to the console layout preview: no JavaScript and
  no framework. The active tab uses the console's tab styles.
- The topbar and page head reuse the console classes (`.topbar`, `.brand`, `.chip`, `.page`,
  `.page-head`, `.eyebrow`, `.muted`, `.mono`).
- The event tape is visually the console Events tab, densified; it is the primary proof of the
  workflow, while the Trade tab keeps a small recent slice for immediate feedback.

A static, script-free layout preview with illustrative data and the full-book debug view is at
[`DEMO_ORDER_LAYOUT.html`](./DEMO_ORDER_LAYOUT.html). It reuses the operational console layout
preview's CSS and is a visual reference, not production code.

### 6.2 Instrument panel

- Symbol code, base/quote asset classes, price decimals, tick, lot, fee policy.
- Best bid/ask, spread, last trade, book sequence.
- Depth ladder: price, visible volume, order count; bid/ask side label and colour; click a level to
  prefill the ticket price.
- Open orders table: side, type, price, remaining volume, sequence, TIF, flags (post-only,
  iceberg, stop). Buttons: **Cancel**, **Amend**.
- Parked stops table: side, stop price, remaining volume, sequence; cancel only.
- **Full book (debug)**: an explicit toggle/button that loads the complete book from
  `/demo/api/book`. It shows separate Bid and Ask tables with every resting order — price, visible
  volume, hidden iceberg reserve, display volume, order sequence, type, TIF, post-only/stop flags,
  fee rates and timestamp — plus all aggregated price levels and every parked stop. The data is
  complete; the table pages client-side (for example 100 rows per page) so a large book remains
  usable. This view is manual-refresh only and carries a "debug view; large books may be slow"
  badge. It is never part of the normal 1 s state poll.
- Empty state: "No resting orders" plus a "Use best price" shortcut.

### 6.3 Order ticket

Fields:

| Field | Notes |
|---|---|
| Side | Buy / Sell segmented control |
| Type | Limit / Market |
| TIF | GTC / IOC / FOK / GTD / Day; GTD reveals an expiry datetime |
| Price | Required for limit and stop-limit; disabled for market |
| Volume | Required; unit selector Base / Quote |
| Post-only | Limit only; crossing orders are rejected by the engine |
| Display volume | Iceberg limit only; must be less than volume |
| Stop price | Stop-market / stop-limit; also selects the stop type |
| Maker/taker fee rate | Advanced; decimal fraction of notional (`0.001` = 10 bps), default 0 |
| Client order id | Auto-generated UUID; reused for a retry of the same submit |

Notes:

- `Quote` volume is available for plain limit and market orders only; disable it for icebergs and
  stops, where the engine requires a base quantity.
- `GTD` maps `expiry_ms` to `TimeInForce::Gtd(expiry_ms)`. Choose a timestamp safely after the
  node's intake time (for example +5 minutes); a past deadline returns `gtd_already_expired`.
- `Day` orders rest until an explicit `PurgeExpired` with `day_close_ms`. The demo has no
  end-of-day action yet, so Day behaves like GTC unless the API is driven directly.
- Fee inputs are signed decimal rates (`-1..=1`, for example `0.001` = 10 bps) parsed into exact
  `Fixed` values; the browser never performs floating-point fee arithmetic.

Validation is convenience only; the engine remains authoritative and may return a `rejected`
event (`fok_unfillable`, `post_only_would_cross`, `stop_would_trigger`, `gtd_already_expired`,
`invalid_order`). The result panel shows the write response and the returned events, including
sequence and `log_id.index`.

The submit button is disabled while a write is in flight. A successful write immediately triggers:
state refresh, event-feed poll with the current cursor, and a flash on the affected order/event.

### 6.4 Event tape

Columns: event id (`log_index:ordinal`), kind, side, order sequence, price, volume, quote volume,
taker/maker fee + asset, reason. Reuse the console's `.badge` colours: fill = good, cancelled /
expired / rejected = warn/bad, other = neutral.

Controls:

- Kind and side filters (reuse the console's event filters).
- Page size 100 / 500 / 1000.
- **Follow** (default) / **Pause**.
- **Reset cursor** clears the client cursor and reloads from the oldest retained event.
- **Order filter**: clicking an order sequence in the book or an event filters the tape and
  highlights related rows.
- Status line: `oldest`, `last`, `retained`, `next_after`, `has_more`, and a warning when the
  cursor fell behind the retained window.

A row can expand to show the raw event JSON (read-only) for debugging.

### 6.5 Status and feedback

- Topbar: node id, leader id, term, ready/health chips, symbol, last update age.
- Follower banner: "This node is a follower. Book reads may lag; writes and events follow the
  leader. [Open leader demo]" (leader address from the state payload).
- Stale banner: last successful state/event age, with the error text.
- Write result banner: sequence, log index, rejected reason, or a typed error with leader guidance.
- An `aria-live="polite"` region announces submitted orders and rejected orders.

## 7. Backend design

### 7.1 Route map

These routes are served by the **demo process**, not the engine. A gateway can expose or block the
demo listener independently of the engine HTTP API.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/demo` | Redirect to `/demo/` |
| `GET` | `/demo/` | Embedded HTML shell |
| `GET` | `/demo/app.js` | Embedded JavaScript |
| `GET` | `/demo/app.css` | Shared theme + demo CSS |
| `GET` | `/demo/api/state` | Symbol, book, orders, stops, event window, node/leader |
| `GET` | `/demo/api/book` | Complete resting book and full depth (debug, manual) |
| `GET` | `/demo/api/events` | Leader-aware event page (`after`, `limit`) |
| `POST` | `/demo/api/orders` | Place one order |
| `POST` | `/demo/api/orders/cancel` | Cancel by side + price + sequence |
| `POST` | `/demo/api/orders/amend` | Amend by side + price + sequence |
| `POST` | `/demo/api/symbol` | Configure the symbol before the lock |
| `POST` | `/demo/api/write` | Optional advanced pass-through for `Batch` / `PurgeExpired` |

`POST /demo/api/write` is deliberately not part of the first UI. If added, it must accept the same
validated/typed requests and remain behind the write header; it is useful for demos of
`ConfigureSymbol`, `Batch` and `PurgeExpired`.

### 7.2 State endpoint

`GET /demo/api/state?depth=20` assembles the view from the engine's HTTP API
(`/read`, `/events/status`, `/metrics`) and returns:

```json
{
  "schema_version": 1,
  "node": {
    "id": 1,
    "http_addr": "127.0.0.1:21001",
    "is_leader": true,
    "leader_id": 1,
    "leader_addr": "127.0.0.1:21001",
    "term": 7,
    "healthy": true,
    "ready": true
  },
  "symbol": {
    "config": {
      "symbol_id": 0,
      "base_asset": {"code": "BTC", "class": "crypto", "decimals": 8},
      "quote_asset": {"code": "USDT", "class": "stablecoin", "decimals": 2},
      "price_decimals": 2,
      "price_tick": "0.01",
      "base_lot": "0.00000001",
      "fee_asset_policy": "received"
    },
    "locked": false
  },
  "book": {
    "sequence": 12,
    "spread": "1",
    "last_trade_price": "100.5",
    "depth": {
      "bids": [{"price": "99.5", "volume": "3", "orders": 2}],
      "asks": [{"price": "100.5", "volume": "4", "orders": 1}]
    }
  },
  "orders": {
    "total": 3,
    "truncated": false,
    "items": [
      {
        "live": "Limit", "side": "Buy", "symbol_id": 0,
        "price": "99.5", "volume": "3", "volume_unit": "base",
        "tif": "GTC", "post_only": false,
        "stop_price": "0", "display_volume": "0", "hidden_volume": "0",
        "id": 0, "sequence": 4, "timestamp": 1762000000000
      }
    ]
  },
  "stops": {"total": 0, "truncated": false, "items": []},
  "events": {
    "oldest_event_id": {"log_index": 10, "ordinal": 0},
    "last_event_id": {"log_index": 40, "ordinal": 2},
    "retained_events": 100,
    "evictions_total": 0,
    "retention_limit": 262144
  },
  "market_data": {"enabled": true, "last_market_seq": 12}
}
```

- `depth` defaults to 20 and is capped at 200.
- `orders.items` is capped (for example 200, sorted by sequence) with `total` and `truncated`. The
  demo fetches the full `/read orderbook_orders` list over HTTP and caps it after parsing; `stops`
  is likewise capped. The full-book endpoint returns the uncapped list.
- `symbol.locked` is inferred from `orderbook_sequence > 0`: the engine has no lock read key, and
  the first accepted order is the point after which configuration is refused.
- `node` fields come from the engine's `/metrics` and the demo's own connection settings.
- `leader_addr` comes from the membership in `/metrics`, never from a user-supplied URL.
- Each `/read` call is a separate HTTP request; the demo assembles an owned DTO and does not hold
  any engine lock.

### 7.2.1 Full-book debug endpoint

`GET /demo/api/book` returns the complete resting book for the debug view:

```json
{
  "schema_version": 1,
  "market_seq": 44,
  "counts": {"bids": 642, "asks": 642, "stops": 2},
  "bids": ["...Order objects, best first..."],
  "asks": ["...Order objects, best first..."],
  "stops": ["...parked stop Order objects..."],
  "depth": {
    "bids": [{"price": "60000.01", "volume": "0.20000000", "orders": 1}],
    "asks": [{"price": "60000.02", "volume": "0.65000000", "orders": 2}]
  },
  "truncated": false
}
```

- Full by default: every resting order, every aggregated price level and every parked stop. The
  order objects include `hidden_volume` and `display_volume` so iceberg reserves are inspectable.
- Fetches the complete `/read orderbook_orders` and `/read orderbook_stops` lists over HTTP and
  computes aggregated depth in the demo process. It is a debug view, not the engine's capped
  `orderbook_depth` read.
- This endpoint is **manual-refresh only** and is never part of the 1 s state poll. The UI warns
  that large books can be slow and pages the table client-side.
- An optional `limit` query parameter exists for constrained clients; the response then sets
  `truncated: true`. The demo UI requests the full set.

### 7.3 Events endpoint

`GET /demo/api/events?after_log_index=10&after_ordinal=2&limit=100` wraps
`MatchEngineClient::events`:

```json
{
  "schema_version": 1,
  "events": [
    {
      "event_id": {"log_index": 40, "ordinal": 0},
      "kind": "fill",
      "order_sequence": 9,
      "side": "Buy",
      "counterparty_sequence": 4,
      "price": "100.5",
      "volume": "3",
      "quote_volume": "301.5",
      "volume_unit": "base",
      "taker_fee": "0.1206",
      "taker_fee_asset": "base",
      "maker_fee": "0.0603",
      "maker_fee_asset": "quote",
      "fee_overflow": false
    }
  ],
  "next_after": {"log_index": 40, "ordinal": 0},
  "oldest_event_id": {"log_index": 10, "ordinal": 0},
  "last_event_id": {"log_index": 40, "ordinal": 2},
  "has_more": false
}
```

`after` is omitted to start at the oldest retained event. The endpoint forwards to the leader, so
the tape works from a follower. It maps `ClientError`/`LinearizableReadError` to the stable error
envelope below; it does not expose raw OpenRaft types.

### 7.4 Order DTOs

`POST /demo/api/orders` accepts a demo ticket and builds a `Place` request server-side:

```json
{
  "side": "Buy",
  "type": "Limit",
  "tif": "GTC",
  "price": "100.5",
  "volume": "2",
  "volume_unit": "base",
  "post_only": false,
  "stop_price": null,
  "display_volume": null,
  "expiry_ms": null,
  "maker_fee_rate": "0.001",
  "taker_fee_rate": "0.002",
  "client_order_id": "demo-7c1b6f5e-..."
}
```

Server rules:

- `symbol_id` comes from the current `SymbolConfig`; the browser never supplies it.
- `id` and `sequence` are `0`; `apply` assigns the real sequence.
- `timestamp` is left for the `/write` intake stamp; the browser never supplies a clock.
- `tif` maps to `TimeInForce`; `GTD` sets `TimeInForce::Gtd(expiry_ms)`.
- `maker_fee_rate` / `taker_fee_rate` are parsed as exact signed `Fixed` fractions and kept in
  `-1..=1`; `0.001` is 10 bps. They are sent to the engine unchanged, with no floating-point
  arithmetic.
- `type`, `side`, `volume_unit`, tick/lot and option combinations are validated for a useful `400`,
  but the engine still runs its own checks; a `rejected` event is returned in the normal response.
- If the symbol is locked and a `ConfigureSymbol` or incompatible symbol request arrives, return
  `409` before the write.

Response:

```json
{
  "schema_version": 1,
  "sequence": 12,
  "log_index": 34,
  "events": ["...MatchEvent objects..."],
  "events_truncated": false,
  "error": null
}
```

`Cancel` and `Amend` accept `{side, price, sequence, new_price?, new_volume?}`. The server rebuilds
the key order with `symbol_id`, side, current price and sequence. Cancel and amend do not need the
original volume; the engine keys the book by side + price + sequence.

### 7.5 Error envelope

All demo API errors use one shape so the UI has one renderer:

```json
{
  "error": {
    "kind": "not_leader",
    "message": "this node is not the leader",
    "leader_id": 2,
    "leader_addr": "127.0.0.1:21002"
  }
}
```

Kinds: `bad_request`, `not_leader`, `no_leader`, `timeout`, `transport`, `unavailable`,
`symbol_locked`, `event_page_invalid`. HTTP status maps to 400/409/503/504.

### 7.6 Module layout

The demo is a separate crate, outside the engine binary:

- `demo/Cargo.toml` — standalone package; depends on `matchengine-raft` via a path.
- `demo/src/main.rs` — CLI, Actix listener and route registration.
- `demo/src/api.rs` — demo routes, assets and the write-action header check.
- `demo/src/state.rs` — engine HTTP reads, DTOs, order construction, depth aggregation and errors.
- `demo/web/index.html`, `demo/web/app.js`, `demo/web/app.css` — demo UI.
- `docs/DEMO_ORDER_LAYOUT.html` — static layout preview.

No `src/` file, engine route or `StorageConfig` field is changed by the demo.

## 8. CSS and frontend reuse

### 8.1 CSS reuse

The demo does not fork the console theme. Its stylesheet is assembled at compile time from the
console's existing file plus a small demo-only layer:

```rust
const APP_CSS: &str = concat!(
    include_str!("../../web/console/app.css"),
    "\n",
    include_str!("../web/app.css"),
);
```

- `web/console/app.css` remains the single source of truth for colours, typography, topbar,
  buttons, tabs, panels, tables, filters, forms and banners.
- `demo/web/app.css` adds only the trading-desk-specific layout: instrument grid, depth ladder,
  ticket fields, event tape/detail, symbol cards and full-book tables.
- The console's HTML/CSS are untouched; `/demo/app.css` is a separate URL served by the demo
  process.

Reused classes include `.topbar`, `.brand`, `.chip`, `.badge`, `.dot`, `.btn`, `.page`,
`.page-head`, `.eyebrow`, `.panel`, `.panel-head`, `.stat-grid`, `.kv-grid`, `.grid-2`,
`.table-wrap`, `table`, `.filter-bar`, `.pager`, `.action-form`, `.check`, `.banner`, `.muted`,
`.mono`, `.good`, `.warn`, `.bad`.

New demo-only classes live in `demo/web/app.css` and cover:

- layout: `.desk-grid`, `.recent-events`, `.events-layout`, `.full-book-grid`, `.symbol-grid`
- instrument/book: `.instrument-grid`, `.ladder`, `.spread-row`, `.orders-table`, `.full-table`,
  `.full-levels`, `.full-book-toolbar`
- ticket/symbol forms: `.segmented`, `.ticket-form`, `.ticket-row`, `.field`, `.input-unit`,
  `.advanced`, `.result-banner`, `.symbol-form`, `.symbol-banner`, `.reset-note`
- events: `.tape`, `.events-table`, `.event-detail`, `.event-json`, `.event-id`, `.fee-note`,
  `.filter`, `.reason`
- shell: `.desk-footer`, `.debug-note`, `.grow`

### 8.2 JavaScript

- Vanilla ES module or classic script; no framework or build step.
- One `fetchJSON` helper with `AbortController` timeout (5 s), JSON content type, and a single
  in-flight guard per poll.
- Render with `textContent` and DOM creation; never `innerHTML` from server values.
- Keep the script small and auditable; it is currently ~33 KB, with a 40 KiB target.
- Poll state every 1 s and events every 700 ms while visible; pause on `document.hidden`; refresh
  immediately on `visibilitychange`; on errors keep the last data, show a warning banner and retry
  on the next tick.
- Reuse the console's event row pattern and filter bar; do not import the console's entire script.

## 9. Workflows

### 9.1 Load and bootstrap

1. `GET /demo/api/state` → render node/leader/symbol/book/orders/event window.
2. If `symbol.locked` is false, the **Symbol** tab's form is enabled (defaults: BTC/USDT, 2 price
   decimals, 8 base decimals, 0.01 tick, 1e-8 lot, received fees) with a "configuration locks after
   the first order" warning.
3. If locked, the symbol card stays readable and the form is disabled.
4. If `node.is_leader` is false, show the follower banner with `leader_addr`; writes and event
   pages still go through the leader-aware client.

### 9.2 Place an order

1. Submit the ticket. The browser generates `client_order_id` once per submit.
2. Validate locally (required fields, decimal shape, tick/lot convenience); do not block on the
   engine's rules.
3. `POST /demo/api/orders` with `X-Demo-Action: submit`.
4. On success, render the sequence/log index and the returned events, then immediately refresh
   state and poll events. On `rejected`, show the reason from the event.
5. On network failure, offer **Retry** using the same `client_order_id`; the engine resolves the
   duplicate instead of double-placing.
6. On `not_leader`, show the leader address/link; the BFF normally forwards automatically.

### 9.3 Cancel and amend

- Cancel uses the order row's side, current price and sequence; confirm in a small dialog.
- Amend prompts for a new price and/or volume (blank keeps the current value). The engine may return
  a new sequence when priority is lost; show "new sequence N" in the result.
- Both refresh state and the tape; the response events are merged with the cursor feed by
  `(log_index, ordinal)`.

### 9.4 Follow the event log

This is the core demo of the `/events` workflow.

1. On first load, if no saved cursor exists, request `GET /demo/api/events` with no `after`; this
   returns the oldest retained page.
2. Merge events into a client-side `Map` keyed by `${log_index}:${ordinal}`; update
   `cursor = next_after`.
3. If `has_more` is true, fetch the next page immediately with `after=cursor`; otherwise poll the
   same endpoint with `after=cursor` on the next tick.
4. Keep at most 500 rendered rows; keep the dedupe map bounded (for example 5,000 ids) while
   preserving the cursor.
5. Persist the cursor in `sessionStorage` under a key that includes node id and symbol id. On reload,
   resume from it.
6. If `oldest_event_id > cursor`, the retained window moved past the client: show an amber
   "history was trimmed" banner, clear the tape, and restart from the oldest retained event. Do not
   silently skip events.
7. On leader failover, the BFF forwards the next page; if it cannot, show a stale banner and retry
   with backoff. The cursor remains valid because ids are `(log_index, ordinal)`.
8. Pause stops polling but keeps the cursor; Resume fetches everything after the cursor, handling
   `has_more` until caught up.
9. Clicking an event filters by `order_sequence`; clicking an open order filters the tape and
   highlights its lifecycle.

### 9.5 Restart / snapshot / retention

- After a node restart, the event window is restored from the snapshot plus WAL replay; the cursor
  keeps working if the ids are still retained.
- If the demo is restarted with a fresh data dir, `oldest_event_id` is empty and the cursor is
  reset; the UI starts from the new window.
- The UI shows the retention limit and eviction count; the demo is not a durable archive.

## 10. Client state model

```js
const state = {
  node: null,               // /metrics + connection settings
  symbol: null,             // SymbolConfig + inferred lock
  book: null,               // sequence, spread, last trade, top-20 depth
  orders: [],               // capped open orders
  stops: [],                // capped parked stops
  events: new Map(),        // "log:ordinal" -> event, capped at 5,000
  cursor: null,             // {log_index, ordinal}, persisted in sessionStorage
  paused: false,            // event polling paused by the user
  side: "Buy",              // ticket side
  fullBook: null,           // manual debug snapshot
  fullPage: 0,              // client-side page of the full book
  selectedEvent: null,      // event detail card
};
```

- One in-flight state poll and one in-flight event poll per tab.
- A submit disables the ticket and uses the same `client_order_id` for its retry.
- The event map is the dedupe source; the DOM is a projection of it.
- No client-side calculation of balances or fees; display the engine's fields verbatim.

## 11. Failure handling

| Failure | UI behavior |
|---|---|
| No leader | Warning banner, last state retained; a write shows the typed no-leader error and the next poll retries. |
| This node is a follower | Follower banner; writes/events still forward via the BFF, local reads may lag. |
| Symbol locked | Disable the configuration form and show the lock badge. |
| Invalid precision/tick/lot | Show the engine's `rejected` reason; keep the ticket values for correction. |
| FOK unfillable / post-only crossing / stop crossed | Show the rejection reason and the current book. |
| Duplicate `client_order_id` retry | Show the returned original sequence/events; do not append duplicates. |
| `/events` retention advanced past cursor | Amber gap banner; reset to oldest retained; never silently drop. |
| Event page `has_more` after a pause | Loop pages until caught up, then resume polling. |
| Transport error / timeout | Warning banner, last data retained, retry on the next poll tick. |
| Leader failover | Cursor survives; BFF forwards; banner until the new leader is observed. |

## 12. Security and safety

- Same trust boundary as the console: loopback or trusted private network, gateway-owned TLS and
  authentication. The demo must not be exposed to the public internet.
- The demo is a separate opt-in process; it adds no route, flag or crate dependency to the engine
  binary. Run it on loopback or a trusted private network only.
- Writes require `X-Demo-Action` and `Content-Type: application/json`; origin/host checks mirror
  the console actions. No cookies, no CORS.
- Same CSP as the console: no inline scripts, no external assets; DOM text rendering.
- The BFF never accepts a URL; the leader address comes from membership.
- One submit in flight per tab; no destructive reset endpoint.
- The demo is an order-entry path. The gateway must protect `/demo` and `/demo/api/*` exactly like
  `/write`; this is documented in `demo/README.md`.

## 13. Testing

### Unit test coverage target

The current demo crate covers the conversion helpers. These are the remaining unit-level cases
worth adding as the DTO layer grows:

- DTO → `MatchEngineRequest` conversion for every order type, TIF, volume unit, stop and iceberg.
- Exact fee-rate parsing, including `0.001` → 10 bps, negative rebates and bounds. *(implemented)*
- State aggregation with empty book, open orders, truncated order cap and locked symbol.
- Full-book endpoint returns every bid/ask/stop, more than the 200-level `/read` cap, hidden iceberg
  fields, and the optional `limit`/`truncated` behavior.
- Event cursor parsing/rejection: missing/partial ids, limit 0/1001.
- Error mapping for no-leader, follower/forward, timeout and engine rejection.

### Demo crate tests

- `cargo test --manifest-path demo/Cargo.toml` covers ticket/TIF conversion, exact fee rates and
  best-first depth aggregation in `demo/src/state.rs`.
- `cargo clippy --manifest-path demo/Cargo.toml --all-targets -- -D warnings` and the demo fmt
  check keep the separate crate clean.
- Manual/scripted smoke: run an engine, initialize it, start the demo process, then configure a
  symbol, place/cancel/amend orders, page `/events` with `limit=1`, reload with a saved cursor and
  load the full book. The engine's own integration suite remains the source of truth for matching
  and Raft behavior.

### Manual checks

- Desktop and laptop widths; keyboard-only ticket and cancel/amend; screen-reader labels.
- Place each order type, pause/follow the tape, reload with a saved cursor.
- Kill/restart the node and the leader; confirm the cursor and banner behavior.
- No external network requests (DevTools) and no console errors.

## 14. Performance budgets

- Assets: shared console CSS + demo CSS + HTML + JS < 150 KiB uncompressed; no external requests.
- State endpoint: p95 < 50 ms on a local single node with a small book; it issues a handful of
  `/read` calls plus `/metrics` and `/events/status`, and caps orders/depth in the demo process.
- Full-book endpoint: on-demand only, never polled. A large book produces a large response and a
  large `/read` payload; the UI pages the result and warns about the cost.
- Events endpoint: one `/events` page per request; no server-side buffering beyond the page.
- Browser: one state poll and one event poll in flight; pause when hidden; tape DOM capped at 500
  rows.
- The demo adds no work to `apply`, matching, WAL, snapshots or Raft.

## 15. Implementation plan

Implemented: **M0** and **M1** ship in `demo/`. **M2** remains optional future work.

### M0 — Standalone demo foundation

1. Create the `demo/` crate; depend on `matchengine-raft` by path and leave the engine binary
   unchanged.
2. Add HTTP state aggregation over `/read` and `/metrics`, event paging over `/events`, and order
   DTOs over `/write`.
3. Serve the demo assets and combine `web/console/app.css` with `demo/web/app.css`.
4. Add unit tests for ticket conversion, fee rates and depth aggregation.
5. Smoke-test against a separately running engine: configure, place, cancel/amend, page events and
   load the full book.

### M1 — Usable desk

1. Page shell, topbar, follower/stale banners.
2. Instrument panel, depth ladder, open orders and parked stops.
3. Limit/market ticket, base/quote volume, post-only, iceberg and stop fields.
4. Event tape with cursor-follow, dedupe, pause/resume, filters and retention gap handling.
5. Cancel/amend actions and symbol configuration panel.

### M2 — Demo polish

1. Canned scenarios (rest, cross, IOC/FOK, iceberg, stop, fee presets).
2. Order/event detail drawer and JSON copy.
3. Keyboard shortcuts and accessibility pass.
4. Documentation, screenshots and a one-command demo script (fresh data dir + two nodes optional).

## 16. Alternatives considered

| Alternative | Why not |
|---|---|
| Browser calls `/write`, `/read`, `/events` directly | Writes/events need leader forwarding; `/read` double-encoding; many round trips; no stable schema. |
| Demo routes embedded in the engine binary | Modifies the engine for a dev tool, widens its public surface and couples demo release cadence to the engine; chosen against in the final design. |
| Reuse the console page with a hidden trading tab | Mixes an SRE tool with an order-entry surface and violates the console's non-goals. |
| A React/Vite app | Adds a build pipeline and dependencies; the console proves a dependency-free UI is enough. |
| A mock order system in JavaScript | Does not demonstrate real Raft/state-machine/event behavior. |

## 17. Risks and open questions

- **Opt-in process:** the demo is a separate binary; the engine remains on its existing surface.
  Run the demo on loopback/private networks and point it at the leader for the freshest view.
- **Follower state:** local reads may lag; the banner and leader link disclose it. A later version
  can proxy `/demo/api/state` to the leader for a fully leader-coherent view.
- **Fees:** expose decimal rate inputs (`0.001` = 10 bps) in an advanced panel with a zero default,
  because fee events are part of the demo and the engine uses rate fractions.
- **Accounts:** do not add mock balances in the first version; show fees and state that the engine
  does not hold funds.
- **Reset:** no destructive endpoint; a fresh `--data-dir` is the reset mechanism.
- **CSS coupling:** the demo concatenates `web/console/app.css` with its own layer at build time.
  Console class renames can break the desk; the check is a visual smoke test of `/demo/` after
  console CSS changes.

## 18. Acceptance criteria

1. The `demo/` binary starts separately with `--addr`, `--engine` and `--engine-id`; the engine
   binary and its routes are unchanged. The page uses the shared console theme.
2. The page shows node/leader/term/ready, the symbol (or the configuration form), BBO, depth,
   open orders, parked stops and the event window.
3. A limit order can be placed, cancelled and amended from the page; each action returns a sequence
   and refresh the book.
4. A crossing order produces fill events in the write response and in the event tape.
5. The event tape follows `next_after` without duplicates, handles multi-page catch-up, persists
   the cursor across reload, and reports a retention gap instead of skipping events.
6. A follower demo forwards writes and event pages to the leader and shows a clear follower banner.
7. Every demo write requires the demo action header; no external assets or inline scripts are used.
8. The full-book debug view shows every resting order, every price level and every parked stop
   (not the top-N state cap), is manually loaded, and warns about large books.
9. `cargo test --manifest-path demo/Cargo.toml`, the demo fmt check and the demo Clippy run are
   clean; the engine's `cargo fmt`, Clippy and MSRV checks are unaffected.
