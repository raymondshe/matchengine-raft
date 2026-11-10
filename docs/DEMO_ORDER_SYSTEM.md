# Demo order system (WebUI) design

Status: **implemented (M0/M1)** — the standalone `demos/order_system/` crate serves the order desk from its own
listener; the engine binary and its routes are unchanged. M2 polish remains optional future work.

Last updated: 2026-11-04.

Related documents: [`OPERATIONAL_CONSOLE.md`](./OPERATIONAL_CONSOLE.md),
[`MATCH_RESULTS.md`](./MATCH_RESULTS.md), [`FEE_DEDUCTION.md`](./FEE_DEDUCTION.md),
[`MARKET_DATA_PLAN.md`](./MARKET_DATA_PLAN.md), [`AGENTS.md`](../AGENTS.md).

## 1. Summary

The **demo order desk** at `/demo/` runs as a **separate process** from the matchengine-raft nodes
and demonstrates the complete trading workflow against them:

1. configure one symbol per product before trading starts,
2. place, cancel and amend orders,
3. watch the selected product's visible order book and the order's lifecycle,
4. read the resulting match events from that backend's replicated `POST /events` log with a cursor,
5. inspect the order-system freeze and the maker/taker settlement legs each order implies,
6. place a **linear quote-margined perpetual** order with a selected leverage on a **second engine
   and order book**, and inspect its demo-side margin, notional and fee.

The desk has two **products with independent backends**:

| Product | Backend | Pair-script instrument | Fee asset policy |
|---|---|---|---|
| **Spot** | the `--engine` node | EUR/USD | `received` |
| **Perpetual** | the optional `--perp-engine` node | BTC/USDT | `quote` |

One engine process trades exactly one symbol, and a linear quote-margined perpetual requires
`fee_asset_policy = quote` while the Spot EUR/USD example uses `received`. The products therefore
cannot share an order book; each product has its own `MatchEngineClient`, symbol configuration,
book, event cursor and order sequence. Starting the demo without `--perp-engine` leaves a Spot-only
desk.

It is a **demo and developer tool**, not a production trading terminal. It is a separate surface
from the operational console: the console remains read-mostly, while the demo intentionally writes
orders. The engine processes are not modified; the demo connects to them over the public HTTP API.

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
- Estimate the order-system freeze and the maker/taker settlement legs per order from the
  replicated symbol configuration, so the fee policy in [`FEE_DEDUCTION.md`](./FEE_DEDUCTION.md)
  §6.6 becomes visible. Every figure is labelled a demo estimate.
- Offer a **perpetual product toggle with leverage selection** backed by a **separate engine and
  order book**. This is a demo-only linear quote-margined overlay: orders are ordinary engine orders
  for the second engine's configured symbol; the demo labels the position side, leverage, notional,
  initial margin and fee it implies. The engine does not hold margin.
- Reuse the console's CSS and visual language rather than building a second design system.
- Stay usable on a single node and on a cluster: writes and event pages follow the leader; reads
  show the serving node's applied state with an explicit follower banner.
- Keep the demo self-contained: no npm, no CDN, no external fonts, no chart library.
- Keep the matching path untouched: the demo only calls existing APIs and reads replicated state.

### Non-goals

- Public internet exposure, TLS, authentication or per-user authorization.
- Balance enforcement, accounts, live positions or risk checks. The perpetual toggle only selects a
  leverage for the freeze/margin estimate; the demo tracks no position, PnL or liquidation and the
  engine reports fills and fees only. The demo may **estimate** the freeze, settlement legs and
  perpetual margin an order system would apply, but it must label them as demo estimates and must
  never present them as engine balances.
- Contract multipliers, inverse/coin-margined contracts, third-asset margin, funding payments,
  maintenance margin, liquidation and cross/isolated position management. Only a linear
  quote-margined perpetual with a zero maintenance buffer is modelled.
- Multi-symbol trading inside one engine. One engine process/cluster = one symbol, per the engine's
  contract; the desk reaches a second symbol by running a second engine and backend.
- Historical charts, market-data Aeron integration, long-term storage or reporting.
- Replacing the operational console. The console answers "is the cluster healthy?"; the demo
  answers "what does an order do?".
- A reset/truncate button. Resetting a demo means restarting with a fresh `--data-dir`.

## 3. Constraints and current state

- The node already serves the HTTP API and the embedded `/console/` UI on `--http-addr`.
- The listener is plaintext and unauthenticated by design; TLS and operator auth belong to the
  gateway. The supported deployment is loopback or a trusted private network.
- One process/cluster trades exactly one symbol. `SymbolConfig` is replicated and locked after the
  first accepted order. The demo must expose this lock clearly. The same config carries the asset
  codes/decimals, tick/lot and `fee_asset_policy` the freeze estimate needs.
- The Spot and Perpetual products are served by different nodes, but each node is an ordinary single
  node or cluster with its own symbol, book, sequence and event log. The demo has no notion of a
  shared cross-product sequence.
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

1. Open `http://<demo-host>/demo/` (the demo process, pointed at one or two nodes).
2. The desk loads both backend descriptions, then the active product's node/leader state, symbol
   configuration and current book.
3. If trading has not started on that backend, the operator reviews or configures its symbol.
4. Place a resting order; it appears in that product's book and order table. Switching products
   switches to the other backend's independent book, orders and event cursor.
5. Place a crossing order; the fill, fees and lifecycle events appear in that product's event tape.
6. Watch the cursor advance, pause/follow, filter by kind/side/order, and verify `next_after`
   stays stable across refresh and failover.

## 5. Architecture

### 5.1 Components

```text
browser
   │  demo process: static HTML/CSS/JS
   ▼
demo Actix listener (demos/order_system/)
   ├── /demo/                  embedded assets
   ├── /demo/api/backends      both product backends (or perpetual: null)
   ├── /demo/api/state         active product's /read snapshot + Raft metrics
   ├── /demo/api/book          active product's full /read order list, depth in the demo
   ├── /demo/api/events        active product's leader-aware event pages
   ├── /demo/api/orders        active product's leader-aware Place
   ├── /demo/api/orders/cancel active product's leader-aware Cancel
   ├── /demo/api/orders/amend  active product's leader-aware Amend
   ├── /demo/api/freeze        local freeze/settlement estimate for a ticket
   └── /demo/api/symbol        active product's leader-aware ConfigureSymbol (before lock)
          │
          ├── Spot backend:      MatchEngineClient (leader tracking, forwarding, retries)
          │     │  HTTP/JSON
          │     ▼
          │  matchengine-raft Spot node: EUR/USD book, own sequence and event log
          │
          └── Perpetual backend: MatchEngineClient (same client, different address)
                │  HTTP/JSON
                ▼
             matchengine-raft Perpetual node: BTC/USDT book, own sequence and event log
```

The demo is a thin **backend-for-frontend (BFF)** in its own binary. It does not implement matching,
ordering or event storage; it validates a small demo DTO, selects the product's backend, builds the
existing `MatchEngineRequest`, and calls the existing public API through that backend's
`MatchEngineClient`. State is assembled from the active backend's `POST /read` and `GET /metrics`;
the engine processes are untouched.

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

The demo is a standalone crate at `demos/order_system/` with its own `Cargo.toml`, binary and embedded assets.
It is never compiled into the matchengine-raft node. `scripts/demo-pair.sh` builds both binaries,
starts the Spot and Perpetual engines on separate data dirs, configures EUR/USD and BTC/USDT, and
runs the demo:

```shell
./scripts/demo-pair.sh
```

Pair defaults: demo `127.0.0.1:24400`, Spot engine `127.0.0.1:24401` (gRPC `25401`), Perpetual
engine `127.0.0.1:24402` (gRPC `25402`), and data in `/tmp/matchengine-raft-demo-pair`. Override
with `DEMO_ADDR`, `SPOT_ADDR`, `PERP_ADDR` and `DEMO_PAIR_DATA_DIR`. The script stops only its own
children; an explicit `DEMO_PAIR_DATA_DIR` is reused instead of wiped.

The manual equivalent is:

```shell
# Spot engine (initialized once, EUR/USD, received fee policy)
cargo run --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:24401 \
  --data-dir /tmp/matchengine-raft-demo/spot --instance-prefix demo-spot

# Perpetual engine (initialized once, BTC/USDT, quote fee policy)
cargo run --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:24402 \
  --data-dir /tmp/matchengine-raft-demo/perp --instance-prefix demo-perp

# demo process
cargo run --manifest-path demos/order_system/Cargo.toml -- \
  --addr 127.0.0.1:24400 \
  --engine 127.0.0.1:24401 --engine-id 1 \
  --perp-engine 127.0.0.1:24402 --perp-engine-id 1
```

- `--addr`: demo WebUI/BFF address.
- `--engine`: HTTP address of the Spot engine.
- `--engine-id`: the Spot node's Raft id, used for role display and client forwarding.
- `--perp-engine`: optional HTTP address of a **different** engine for the Perpetual product; omit it
  for a Spot-only desk. `--perp-engine-id` (default 1) is that node's Raft id.

Each engine needs initialization (`POST /init` with `[]`) and a leader before its product accepts
orders; the demo can start earlier and shows a "no leader" banner per active backend. The demo
process is loopback/private-network-only, same as the engines; TLS and authentication remain gateway
concerns.

## 6. UI design

### 6.1 Layout

A single page would be too dense, so the desk reuses the operational console's radio-tab pattern.
The topbar, page head and demo banner stay shared; four tabs split the detail. The page head carries
a **Trading pair** dropdown built from the configured backends (`EUR/USD · Spot`, `BTC/USDT ·
Perpetual`); choosing an entry switches engine, symbol, book, orders and event cursor.

| Tab | Content |
|---|---|
| **Trade** | Instrument, depth ladder, open orders and parked stops (left) + order ticket and result (right), with a compact recent-events strip below. |
| **Full book** | The complete order-level bid/ask book, all aggregated levels and parked stops (debug view). |
| **Events** | Full-width cursor tape with filters and pager, plus a selected-event detail card. |
| **Symbol** | Symbol configuration/lock, asset metadata, tick/lot, fee policy, node and event-window status. |

```text
┌─ topbar: brand · node · leader · term · ready · pair · backend · updated ─┐
├─ page head: [ Trading pair ▼ ] · follower/leader banner · refresh ────────┤
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

- The active pair's symbol code, base/quote asset classes, price decimals, tick, lot, fee policy.
- Best bid/ask, spread, last trade, book sequence.
- Depth ladder: price, visible volume, order count; bid/ask side label and colour; click a level to
  prefill the ticket price.
- Estimated-freeze strip above the ladder: one chip per asset with the summed estimate for every
  priced resting order and parked stop, plus counts of priced and unpriced orders and the
  "demo estimate · engine holds no balances" note.
- Open orders table: side, type, price, remaining volume, estimated freeze, sequence, TIF, flags
  (post-only, iceberg, stop). Buttons: **Cancel**, **Amend**. Clicking a row opens the order
  economics panel.
- Parked stops table: side, stop price, remaining volume, estimated freeze, sequence; cancel only.
  Clicking a row opens the same panel.
- **Full book (debug)**: an explicit toggle/button that loads the complete book from
  `/demo/api/book`. It shows separate Bid and Ask tables with every resting order — price, visible
  volume, hidden iceberg reserve, display volume, estimated freeze, order sequence, type, TIF,
  post-only/stop flags, fee rates and timestamp — plus all aggregated price levels and every parked
  stop. A badge repeats the aggregate freeze. The data is complete; the table pages client-side
  (for example 100 rows per page) so a large book remains usable. This view is manual-refresh only
  and carries a "debug view; large books may be slow" badge. It is never part of the normal 1 s
  state poll.
- Empty state: "No resting orders" plus a "Use best price" shortcut.

### 6.3 Order ticket

Fields:

| Field | Notes |
|---|---|
| Trading pair | Page-head dropdown, not part of the ticket: `EUR/USD · Spot` or `BTC/USDT · Perpetual`; the Perpetual entry is disabled when no `--perp-engine` backend is configured |
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
`order_not_found`, `invalid_order`). The result panel shows the write response and the returned
events, including sequence and `log_id.index`.

The submit button is disabled while a write is in flight. A successful write immediately triggers:
state refresh, event-feed poll with the current cursor, and a flash on the affected order/event.

### 6.3.1 Freeze preview and order economics

The engine is balance-agnostic and holds no freeze, so every figure in this section is a
**demo estimate** derived from the replicated `SymbolConfig`, the order fields and the entered fee
rates. The UI labels it as such wherever it appears; the engine remains authoritative for fills and
fees only. The policy being estimated is [`FEE_DEDUCTION.md`](./FEE_DEDUCTION.md) §6.6.

**Ticket preview.** While the ticket has a volume, the demo debounces a `POST /demo/api/freeze` and
shows the estimated freeze before submit. For a restable limit order the preview uses the **worse**
of the maker and taker rates, because it may rest or cross; market, IOC/FOK and stop tickets use the
taker rate only. The badge says "worst-case rate" for a restable limit and "taker rate" for a
taker-only ticket, rather than claiming a role the order may not take. A market order without a
limit/stop price uses `orderbook_last_trade_price` as its reference; if there is no last
trade either, the preview shows a reason instead of a number. Changing side, type, unit, price,
volume, stop, iceberg, expiry or the fee inputs refreshes the preview.

**Rows and panel.** Open orders, parked stops and full-book rows carry an estimated freeze; clicking
one selects it and opens the **Order economics** panel:

| Field | Meaning |
|---|---|
| Freeze | Estimated asset and amount the order system reserves, with the role used to size it |
| Basis | Why that asset is frozen: quote notional, base principal, quote notional + fee, or quote collateral + base-fee reserve |
| Fee rate | Non-negative rate selected for the estimate (it may come from a settlement leg rather than enlarge the freeze) |
| Reference price | Limit price, stop price, or last trade fallback (market tickets only) |
| Maker / Taker rows | Assets delivered and received, fee asset and fee amount, signed rate |
| Note | How clearing funds the fee under the policy, plus the rebate and estimate disclaimers |

A restable limit order (GTC/GTD/Day, not a stop) has both maker (resting) and taker (immediate)
legs; a market order, an IOC/FOK limit and a stop child have only the taker leg because they never
rest. Fees are signed, so a negative rate shows as a rebate. Resting freezes use remaining open
volume (including an iceberg's hidden reserve).

**Estimation rules implemented in `demos/order_system/src/freeze.rs`:**

- Reference price: order price, else stop price, else last trade for a market ticket preview.
- Quote-volume limit orders normalize to floor base at the limit price and re-derive the notional
  (`base * price`), matching the engine's sweep. Quote market buys floor and sells ceil the derived
  base.
- `Received` buy: freeze ceil quote notional `Q*P`; fee in base, withheld from the base delivery.
  `Received` sell: freeze ceil base principal; fee in quote, withheld from the proceeds.
- `Quote` buy: freeze ceil quote `Q*P*(1+rate)`; fee deducted from the frozen quote.
  `Quote` sell: freeze base principal; fee withheld from the quote proceeds.
- `Base` buy: freeze ceil quote `max(requested quote, base*P) + base*rate*P`; the base fee is
  withheld from the base delivery and the matching quote reserve is released.
- `Base` sell: quote collateral, at least `max(requested quote, base*P)`, plus the base-fee reserve;
  the fee is withheld or acquired against that collateral.
- Rounding is exact `Fixed` arithmetic: quote freezes round up to the quote asset's settlement
  precision, base freezes round up to the base lot, and a zero lot does not disable precision
  rounding. The browser never performs fee or freeze arithmetic.

**Perpetual overlay rules implemented in `demos/order_system/src/freeze.rs`:**

- The product is a **linear quote-margined perpetual**. Buy opens a long and sell opens a short;
  the order is sent to the **Perpetual backend's** engine as a normal cash order for that engine's
  configured symbol. It never touches the Spot book.
- Perpetual orders require the Perpetual backend's symbol to use `fee_asset_policy = quote`. A symbol
  configured with `received` or `base` cannot fund a margin fee, so the preview returns a reason and
  the write route rejects the ticket with `400` before Raft. The pair script therefore configures
  Spot EUR/USD as `received` and Perpetual BTC/USDT as `quote`, on separate engines.
- Leverage is an integer `1..=100`; the UI offers 1/2/3/5/10/20/25/50/100 and defaults to 10.
- `notional = max(quote_amount, base_amount * reference_price)`, so a quote-volume market sell
  collateralises the base quantity it may derive, not just the requested quote budget.
- `initial_margin = notional / leverage`, `fee = notional * selected_rate`, `buffer = 0`, and
  `freeze = ceil_quote(initial_margin + fee + buffer)`.
- A resting limit preview uses the worse of its maker/taker rates; a taker-only ticket uses the
  taker rate. A resting perp order shown in the book uses its maker rate, consistent with the spot
  estimate.
- The order's economics has `maker: null`, `taker: null` and a `perpetual` object with the
  leverage, position side, notional, initial margin, fee and buffer. The fee is funded by
  `deduct_from_margin`; there are no spot delivery legs.
- The demo records `{product, leverage}` in process memory keyed by that backend's order sequence.
  It survives a page reload but not a demo restart: the engine `Order` has no product or leverage
  field and the metadata is not replicated. After a demo restart, still-resting orders on the
  Perpetual backend still render as Perpetual (the backend determines the product) but show unknown
  leverage and no margin estimate until they leave the book.
- No contract multiplier, funding, PnL, liquidation, maintenance margin, inverse contract or
  third-asset settlement is modelled. A negative fee rate is a rebate, but the freeze estimate
  selects the clamped non-negative rate, so a rebate yields a freeze of exactly the initial
  margin — never less (pinned by
  `perpetual_rebate_rates_never_reduce_the_freeze_below_the_margin`).

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

A row can expand to show the raw event JSON (read-only) for debugging. The tape always belongs to
the pair selected in the page-head dropdown; switching pairs swaps in that backend's event window
and its saved cursor.

### 6.5 Status and feedback

- Topbar: node id, leader id, term, ready/health chips, selected pair, backend address, last update
  age.
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
| `GET` | `/demo/api/backends` | Both product backends, with `perpetual: null` for Spot-only |
| `GET` | `/demo/api/state?product=` | Active product's symbol, book, orders, stops, event window, node/leader |
| `GET` | `/demo/api/book?product=` | Active product's complete resting book and full depth (debug, manual) |
| `GET` | `/demo/api/events?product=` | Active product's leader-aware event page (`after_log_index`, `after_ordinal`, `limit`) |
| `POST` | `/demo/api/orders` | Place one order; body `product` selects the backend |
| `POST` | `/demo/api/freeze` | Estimate freeze/settlement for an unsubmitted ticket |
| `POST` | `/demo/api/orders/cancel` | Cancel by product + side + price + sequence |
| `POST` | `/demo/api/orders/amend` | Amend by product + side + price + sequence |
| `POST` | `/demo/api/symbol?product=` | Configure the selected product's symbol before the lock |
| `POST` | `/demo/api/write` | Optional advanced pass-through for `Batch` / `PurgeExpired` |

`POST /demo/api/write` is deliberately not part of the first UI. If added, it must accept the same
validated/typed requests and remain behind the write header; it is useful for demos of
`ConfigureSymbol`, `Batch` and `PurgeExpired`.

### 7.2 State endpoint

`GET /demo/api/state?depth=20&product=spot` assembles the view from the selected backend's HTTP API
(`/read`, `/events/status`, `/metrics`) and returns:

```json
{
  "schema_version": 2,
  "product": "spot",
  "backend": {
    "product": "spot",
    "engine_id": 1,
    "http_addr": "127.0.0.1:21001"
  },
  "backends": {
    "spot": {"product": "spot", "engine_id": 1, "http_addr": "127.0.0.1:21001"},
    "perpetual": {"product": "perpetual", "engine_id": 1, "http_addr": "127.0.0.1:21002"}
  },
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
        "order": {
          "live": "Limit", "side": "Buy", "symbol_id": 0,
          "price": "99.5", "volume": "3", "volume_unit": "base",
          "tif": "GTC", "post_only": false,
          "stop_price": "0", "display_volume": "0", "hidden_volume": "0",
          "id": 0, "sequence": 4, "timestamp": 1762000000000
        },
        "product": "spot",
        "economics": {
          "product": "spot",
          "freeze": {
            "asset": "USDT", "amount": "298.5", "basis": "quote_notional",
            "role": "maker", "fee_rate": "0.001", "reference_price": "99.5"
          },
          "maker": {
            "role": "maker", "fee_rate": "0.001",
            "delivers": {"asset": "USDT", "amount": "298.5"},
            "receives": {"asset": "BTC", "amount": "3"},
            "fee": {"asset": "BTC", "amount": "0.003"}
          },
          "taker": {
            "role": "taker", "fee_rate": "0.002",
            "delivers": {"asset": "USDT", "amount": "298.5"},
            "receives": {"asset": "BTC", "amount": "3"},
            "fee": {"asset": "BTC", "amount": "0.006"}
          },
          "fee_funding": "withhold_from_base_delivery",
          "perpetual": null
        }
      }
    ]
  },
  "stops": {"total": 0, "truncated": false, "items": []},
  "freeze": {
    "orders": [{"asset": "USDT", "amount": "298.5", "orders": 1}],
    "stops": [],
    "total": [{"asset": "USDT", "amount": "298.5", "orders": 1}],
    "estimated": 1,
    "unestimated": 0
  },
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

- `product` defaults to `spot`; `perpetual` selects the second backend. Every field below comes from
  that backend only, so sequences, books and event ids are independent between products.
- `backends.perpetual` is `null` when the demo was started without `--perp-engine`; the page then
  offers a Spot-only dropdown and disables the Perpetual entry. `GET /demo/api/backends` returns the
  same `{spot, perpetual}` object without touching an engine.
- `depth` defaults to 20 and is capped at 200.
- `orders.items` is capped (for example 200, sorted by sequence) with `total` and `truncated`. The
  demo fetches the full `/read orderbook_orders` list over HTTP and caps it after parsing; `stops`
  is likewise capped. The full-book endpoint returns the uncapped list.
- Each `orders.items`/`stops.items` entry is
  `{ "order": Order, "product": "spot"|"perpetual", "leverage"?: 1..100, "economics": OrderEconomics|null }`.
  `leverage` is omitted for spot. The nesting keeps the engine `Order` shape unchanged;
  `economics` is `null` when the demo cannot price the order (no price, stop price or last-trade
  fallback, or a perpetual order on a non-quote fee policy). Older demo clients that expected a bare
  order object must read `entry.order`; v2 clients must tolerate both products. `product` is the
  demo's own tag, not an engine field.

A perpetual entry carries margin terms and no spot legs:

```json
{
  "order": {"side": "Buy", "price": "50000", "volume": "1", "sequence": 5, "...": "..."},
  "product": "perpetual",
  "leverage": 10,
  "economics": {
    "product": "perpetual",
    "freeze": {
      "asset": "USDT", "amount": "5050", "basis": "perpetual_margin_plus_fee",
      "role": "maker", "fee_rate": "0.001", "reference_price": "50000"
    },
    "maker": null,
    "taker": null,
    "fee_funding": "deduct_from_margin",
    "perpetual": {
      "leverage": 10, "position": "long", "notional": "50000",
      "initial_margin": "5000",
      "fee": {"asset": "USDT", "amount": "50"}, "buffer": "0"
    }
  }
}
```

- `freeze` aggregates the estimates: `orders`, `stops` and `total` each group by asset (`asset`,
  `amount`, `orders`), while `estimated`/`unestimated` count priced and unpriced entries. It is
  computed over the full parsed lists before the `MAX_STATE_ORDERS` display cap, so the total does
  not silently omit truncated orders.
- `symbol.locked` is inferred from `orderbook_sequence > 0`: the engine has no lock read key, and
  the first accepted order is the point after which configuration is refused.
- `node` fields come from the engine's `/metrics` and the demo's own connection settings.
- `leader_addr` comes from the membership in `/metrics`, never from a user-supplied URL.
- Each `/read` call is a separate HTTP request; the demo assembles an owned DTO and does not hold
  any engine lock.

### 7.2.1 Full-book debug endpoint

`GET /demo/api/book?product=spot` returns the selected backend's complete resting book for the
debug view:

```json
{
  "schema_version": 2,
  "product": "spot",
  "market_seq": 44,
  "counts": {"bids": 642, "asks": 642, "stops": 2},
  "bids": ["...DeskOrder entries ({order, economics}), best first..."],
  "asks": ["...DeskOrder entries ({order, economics}), best first..."],
  "stops": ["...parked stop DeskOrder entries..."],
  "depth": {
    "bids": [{"price": "60000.01", "volume": "0.20000000", "orders": 1}],
    "asks": [{"price": "60000.02", "volume": "0.65000000", "orders": 2}]
  },
  "freeze": {
    "orders": [{"asset": "USDT", "amount": "...", "orders": 642}],
    "stops": [{"asset": "BTC", "amount": "...", "orders": 2}],
    "total": [{"asset": "BTC", "amount": "...", "orders": 2},
              {"asset": "USDT", "amount": "...", "orders": 642}],
    "estimated": 644,
    "unestimated": 0
  },
  "truncated": false
}
```

- `product` defaults to `spot` and selects the backend; the response never mixes two books.
- Full by default: every resting order, every aggregated price level and every parked stop. The
  order objects include `hidden_volume` and `display_volume` so iceberg reserves are inspectable,
  and each entry carries the same `{order, economics}` shape and `freeze` aggregate as `/state`.
- Fetches the complete `/read orderbook_orders` and `/read orderbook_stops` lists over HTTP and
  computes aggregated depth in the demo process. It is a debug view, not the engine's capped
  `orderbook_depth` read.
- This endpoint is **manual-refresh only** and is never part of the 1 s state poll. The UI warns
  that large books can be slow and pages the table client-side.
- An optional `limit` query parameter exists for constrained clients; the response then sets
  `truncated: true`. The demo UI requests the full set.

### 7.3 Events endpoint

`GET /demo/api/events?product=spot&after_log_index=10&after_ordinal=2&limit=100` wraps the selected
backend's `MatchEngineClient::events`:

```json
{
  "schema_version": 2,
  "product": "spot",
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
      "error": []
    }
  ],
  "next_after": {"log_index": 40, "ordinal": 0},
  "oldest_event_id": {"log_index": 10, "ordinal": 0},
  "last_event_id": {"log_index": 40, "ordinal": 2},
  "has_more": false
}
```

Both cursor parameters are omitted to start at the oldest retained event; supplying only one is a
`400`. `product` defaults to `spot`; cursors are backend-local and must never be carried across
products. The endpoint forwards to the selected backend's leader, so the tape works from a follower.
It maps
`ClientError`/`LinearizableReadError` to the stable error envelope below; it does not expose raw
OpenRaft types.

### 7.4 Order DTOs

`POST /demo/api/orders` accepts a demo ticket and builds a `Place` request server-side:

```json
{
  "side": "Buy",
  "product": "spot",
  "leverage": null,
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
- `product` defaults to `spot` and selects the backend (and therefore the engine, symbol and book).
  `perpetual` requires a configured `--perp-engine`, integer `leverage` in `1..=100`, and a backend
  symbol whose `fee_asset_policy` is `quote`; otherwise the route returns `400` before proposing to
  Raft. The engine receives an ordinary `Place` — no product or leverage is sent — and the demo
  remembers the tag by that backend's returned sequence for its book/state views.
- `type`, `side`, `volume_unit`, tick/lot and option combinations are validated for a useful `400`,
  but the engine still runs its own checks; a `rejected` event is returned in the normal response.
- If the symbol is locked and a `ConfigureSymbol` or incompatible symbol request arrives, return
  `409` before the write.

Response:

```json
{
  "schema_version": 2,
  "sequence": 12,
  "log_index": 34,
  "events": ["...MatchEvent objects..."],
  "events_truncated": false,
  "error": null
}
```

`Cancel` and `Amend` accept `{product, side, price, sequence, new_price?, new_volume?}`. The server
routes to that product's backend and rebuilds the key order with its `symbol_id`, side, current
price and sequence. Cancel and amend do not need the original volume; the engine keys the book by
side + price + sequence.

### 7.4.1 Freeze preview endpoint

`POST /demo/api/freeze` takes the same ticket body as `/demo/api/orders` (without
`client_order_id`), builds the order server-side against the current `SymbolConfig`, and returns a
demo estimate without writing anything:

```json
{
  "schema_version": 2,
  "economics": {
    "product": "spot",
    "freeze": {
      "asset": "USDT", "amount": "50000", "basis": "quote_notional",
      "role": "taker", "fee_rate": "0.002", "reference_price": "50000"
    },
    "maker": {
      "role": "maker", "fee_rate": "0.001",
      "delivers": {"asset": "USDT", "amount": "50000"},
      "receives": {"asset": "BTC", "amount": "1"},
      "fee": {"asset": "BTC", "amount": "0.001"}
    },
    "taker": {
      "role": "taker", "fee_rate": "0.002",
      "delivers": {"asset": "USDT", "amount": "50000"},
      "receives": {"asset": "BTC", "amount": "1"},
      "fee": {"asset": "BTC", "amount": "0.002"}
    },
    "fee_funding": "withhold_from_base_delivery",
    "perpetual": null
  },
  "reason": null
}
```

A perpetual preview for the same ticket adds `"product": "perpetual"` and `"leverage": 10` to the
request; the response replaces the spot legs with margin terms:

```json
{
  "schema_version": 2,
  "economics": {
    "product": "perpetual",
    "freeze": {
      "asset": "USDT", "amount": "5100", "basis": "perpetual_margin_plus_fee",
      "role": "taker", "fee_rate": "0.002", "reference_price": "50000"
    },
    "maker": null,
    "taker": null,
    "fee_funding": "deduct_from_margin",
    "perpetual": {
      "leverage": 10, "position": "long", "notional": "50000",
      "initial_margin": "5000",
      "fee": {"asset": "USDT", "amount": "100"}, "buffer": "0"
    }
  },
  "reason": null
}
```

- The preview uses the worse of the maker and taker rates, so `freeze.role` names the rate that won:
  `taker` in the example because 0.2% > 0.1%. The UI labels a restable limit result "worst-case
  rate" and a taker-only ticket "taker rate".
- A market order with no price or stop price reads `orderbook_last_trade_price` as its reference. If
  no reference exists, `economics` is `null` and `reason` explains what is missing. A limit order
  without a price never borrows the last trade; it has no honest estimate.
- The endpoint requires the write-action header like the write routes, but performs no `/write`
  call, takes no sequence and changes no book or event state.
- A `400` from ticket parsing or fee-rate validation uses the normal error envelope. The estimate is
  best-effort and never replaces the engine's tick/lot/precision checks at submit time.
- For `product = "perpetual"` the endpoint first requires a configured Perpetual backend, validates
  leverage `1..=100`, and reads that backend's `fee_asset_policy = quote`. A market ticket with no
  limit/stop price still falls back to that backend's `orderbook_last_trade_price` as its notional
  reference. If the policy is not `quote`, `economics` is `null` and `reason` names the required
  policy; if no backend is configured the route returns `400` with kind `not_configured`.

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

Kinds: `bad_request`, `not_configured`, `not_leader`, `no_leader`, `timeout`, `transport`,
`unavailable`, `symbol_locked`, `event_page_invalid`. HTTP status maps to 400/409/503/504.

### 7.6 Module layout

The demo is a separate crate, outside the engine binary:

- `demos/order_system/Cargo.toml` — standalone package; depends on `matchengine-raft` via a path.
- `demos/order_system/src/main.rs` — CLI, Actix listener and route registration.
- `demos/order_system/src/api.rs` — demo routes, assets and the write-action header check.
- `demos/order_system/src/state.rs` — engine HTTP reads, DTOs, order construction, depth aggregation and errors.
- `demos/order_system/src/freeze.rs` — demo-only freeze/settlement estimates for the cash/spot policy and the
  linear perpetual overlay, exact `Fixed` arithmetic and aggregation by asset.
- `demos/order_system/web/index.html`, `demos/order_system/web/app.js`, `demos/order_system/web/app.css` — demo UI.
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
- `demos/order_system/web/app.css` adds only the trading-desk-specific layout: instrument grid, depth ladder,
  ticket fields, event tape/detail, symbol cards and full-book tables.
- The console's HTML/CSS are untouched; `/demo/app.css` is a separate URL served by the demo
  process.

Reused classes include `.topbar`, `.brand`, `.chip`, `.badge`, `.dot`, `.btn`, `.page`,
`.page-head`, `.eyebrow`, `.panel`, `.panel-head`, `.stat-grid`, `.kv-grid`, `.grid-2`,
`.table-wrap`, `table`, `.filter-bar`, `.pager`, `.action-form`, `.check`, `.banner`, `.muted`,
`.mono`, `.good`, `.warn`, `.bad`.

New demo-only classes live in `demos/order_system/web/app.css` and cover:

- layout: `.desk-grid`, `.recent-events`, `.events-layout`, `.full-book-grid`, `.symbol-grid`
- instrument/book: `.instrument-grid`, `.ladder`, `.spread-row`, `.orders-table`, `.full-table`,
  `.full-levels`, `.full-book-toolbar`
- ticket/symbol forms: `.segmented`, `.segmented.product`, `.ticket-form`, `.ticket-row`,
  `.field`, `.static-value`, `.input-unit`, `.advanced`, `.result-banner`, `.symbol-form`,
  `.symbol-banner`, `.reset-note`
- freeze/economics: `.freeze-strip`, `.freeze-chips`, `.freeze-chip`, `.freeze-cell`,
  `.ticket-freeze`, `.economics-panel`, `.economics-freeze`, `.perp-terms`,
  `.settlement-table`, `.selected`
- events: `.tape`, `.events-table`, `.event-detail`, `.event-json`, `.event-id`, `.fee-note`,
  `.filter`, `.reason`
- shell: `.desk-footer`, `.debug-note`, `.grow`

### 8.2 JavaScript

- Vanilla ES module or classic script; no framework or build step.
- One `fetchJSON` helper with `AbortController` timeout (5 s), JSON content type, and a single
  in-flight guard per poll.
- Render with `textContent` and DOM creation; never `innerHTML` from server values.
- Keep the script small and auditable; it is currently ~45 KB, with a 50 KiB target.
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

1. On first load, if no saved cursor exists, request `GET /demo/api/events` with neither cursor
   parameter; this returns the oldest retained page.
2. Merge events into a client-side `Map` keyed by `${log_index}:${ordinal}`; update
   `cursor = next_after`.
3. If `has_more` is true, fetch the next page immediately with
   `after_log_index`/`after_ordinal` from the cursor; otherwise poll the same endpoint on the next
   tick.
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
  product: "spot",          // "spot" | "perpetual"
  fullBook: null,           // manual debug snapshot
  fullPage: 0,              // client-side page of the full book
  selectedEvent: null,      // event detail card
  selectedOrder: null,      // order economics panel
  freeze: null,             // /state freeze aggregate
  ticketPreview: null,      // debounced /demo/api/freeze result
};
```

- One in-flight state poll and one in-flight event poll per tab.
- A submit disables the ticket and uses the same `client_order_id` for its retry.
- The event map is the dedupe source; the DOM is a projection of it.
- No client-side calculation of balances, fees or freezes: the browser renders the engine's event
  fields verbatim and the demo BFF's clearly labelled estimates. Freeze and perpetual margin
  arithmetic happens once, in `demos/order_system/src/freeze.rs`, using exact `Fixed` values. The leverage `<select>`
  only supplies the `leverage` integer; the UI never derives margin or fee amounts.
- Product/leverage metadata is demo-process state keyed by order sequence. It is not persisted,
  replicated or snapshot-backed; after a demo restart, still-resting perpetual orders render as
  Spot until they leave the book, because the engine `Order` has no product or leverage field.

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
  `/write`; this is documented in `demos/order_system/README.md`.

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
- Freeze estimation for every fee-asset policy and side, quote/base volume units, iceberg reserve,
  taker-only economics (market/stop/IOC/FOK), worst-case ticket rates, precision rounding and
  missing reference prices. *(implemented in `demos/order_system/src/freeze.rs` — 23 cases)*
- Perpetual overlay: long/short mapping, margin plus quote fee, leverage bounds, quote-policy
  requirement (quote only; `received` and `base` refused), quote-volume market-sell collateral,
  worst-case ticket rate and zero buffer. *(implemented in `demos/order_system/src/freeze.rs`)*
- Perpetual edge cases: leverage-boundary estimates (1x and 100x) and out-of-range rejection,
  rebate clamping (the freeze never drops below the initial margin), margin-plus-fee rounding to
  quote precision, quote-volume limit normalization, stop-market reference price, IOC taker-only
  rate, missing-reference rejection for market and limit tickets, exact 18-decimal arithmetic
  with end-only rounding, overflow returning `None` instead of panicking, and margin aggregation
  by quote asset. *(implemented in `demos/order_system/src/freeze.rs`)*
- Perpetual DTO/state: `DeskOrder` product/leverage serialization, margin terms, ticket defaults,
  `validate_product` leverage/policy guards (spot bypasses both), leverage metadata surviving an
  amend's sequence change and being ignored on the spot backend, and fractional-leverage ticket
  rejection. *(implemented in `demos/order_system/src/state.rs`)*

### Demo crate tests

- `cargo test --manifest-path demos/order_system/Cargo.toml` (47 tests) covers ticket/TIF conversion, exact fee
  rates, best-first depth aggregation, `DeskOrder`/freeze-state payloads, product validation and
  leverage metadata in `demos/order_system/src/state.rs`, plus the freeze policy, settlement math and the
  perpetual margin/fee/rounding/rebate/overflow cases in `demos/order_system/src/freeze.rs`.
- `cargo clippy --manifest-path demos/order_system/Cargo.toml --all-targets -- -D warnings` and the demo fmt
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
- Freeze preview: debounced per ticket edit, two small `/read` calls (symbol config, last trade) and
  local `Fixed` arithmetic; no `/write`, no engine lock and no extra engine work. Freeze aggregation
  for state/book is linear in the returned orders and adds no engine calls beyond `symbol_config`.
- Browser: one state poll and one event poll in flight; pause when hidden; tape DOM capped at 500
  rows.
- The demo adds no work to `apply`, matching, WAL, snapshots or Raft.

## 15. Implementation plan

Implemented: **M0** and **M1** ship in `demos/order_system/`. **M2** remains optional future work.

### M0 — Standalone demo foundation

1. Create the `demos/order_system/` crate; depend on `matchengine-raft` by path and leave the engine binary
   unchanged.
2. Add HTTP state aggregation over `/read` and `/metrics`, event paging over `/events`, and order
   DTOs over `/write`.
3. Serve the demo assets and combine `web/console/app.css` with `demos/order_system/web/app.css`.
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

1. Canned scenarios (rest, cross, IOC/FOK, iceberg, stop, fee presets, perpetual long/short).
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
- **Accounts:** no mock balances. The demo reconstructs the §6.6 freeze/settlement policy as a
  labelled estimate from replicated config and order fields; it stores no account or freeze state
  and always states that the engine does not hold funds.
- **Perpetual metadata:** the engine `Order` has no product/leverage field, so the demo keeps the
  tag in process memory keyed by sequence. A demo restart loses it and resting perp orders render as
  Spot; a production derivative would need an engine-level instrument kind and leverage/risk fields.
- **Perpetual fidelity:** only a linear quote-margined contract with zero maintenance buffer is
  estimated. Funding, PnL, liquidation, contract multipliers and inverse/third-asset contracts are
  explicitly out of scope and must not be inferred from the displayed margin.
- **Reset:** no destructive endpoint; a fresh `--data-dir` is the reset mechanism.
- **CSS coupling:** the demo concatenates `web/console/app.css` with its own layer at build time.
  Console class renames can break the desk; the check is a visual smoke test of `/demo/` after
  console CSS changes.

## 18. Acceptance criteria

1. The `demos/order_system/` binary starts separately with `--addr`, `--engine` and `--engine-id`; the engine
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
9. Every priced order shows a demo-estimated freeze; the ticket previews the freeze before submit,
   and selecting an order shows its maker/taker deliveries, fees, rates and fee-funding rule. Every
   figure is labelled a demo estimate, and the engine's balance-agnostic contract is stated.
10. The ticket offers Spot/Perpetual and, for a quote-fee symbol, a leverage selection in
   `1..=100`; a perpetual order shows leverage, long/short, notional, initial margin, fee and zero
   buffer, and the demo rejects perpetual orders on a non-quote policy with a clear message.
11. `cargo test --manifest-path demos/order_system/Cargo.toml`, the demo fmt check and the demo Clippy run are
   clean; the engine's `cargo fmt`, Clippy and MSRV checks are unaffected.
