# Trading terminal

A Binance-style spot trading terminal for the matchengine fleet: order book,
trade tape, candlestick chart and an order ticket, deployed to the OrbStack
cluster as a static React SPA behind nginx.

## Architecture — frontend/backend separation

This is a **pure frontend project**: React + TypeScript with its own
`package.json` and Vite build. There is no Rust here, no crate dependency, and
the build is never embedded into a Rust binary. The terminal is a third client
of two existing services and shares nothing with their embedded WebUIs except
the wire contracts:

| Concern | Backend | Contract |
| --- | --- | --- |
| Market data | `market_data_system` (global terminal service) | Binance-style WebSocket: `SUBSCRIBE`/`UNSUBSCRIBE`/`LIST_SUBSCRIPTIONS`, `bookTicker`, `trade`, `depthSnapshot` + `depthUpdate` (contiguous `U`/`u`), `kline_1s`/`kline_1m`, `status`, `resync`; `GET /info`, `GET /ready` |
| Order entry | `order_system` BFF | `GET /demo/api/backends`, `/demo/api/state`, `/demo/api/events` (cursor-follow), `POST /demo/api/orders`, `/demo/api/orders/cancel`, `/demo/api/orders/amend`, `/demo/api/freeze` — every POST carries `X-Demo-Action: submit` |

The browser never talks to the matching engine directly; the order path is
terminal → nginx → order-system BFF → engine. The BFF's same-origin
(`Origin` host) guard stays intact because the terminal proxies
same-origin instead of using CORS.

### Deployment plumbing only

`nginx/default.conf.template` serves the static SPA and reverse-proxies
same-origin:

- `/md/ws`, `/md/stream`, `/md/*` → the global market-data service
- `/demo/api/*` → the order-system BFF
- `Host $host` is preserved so the BFF's `X-Demo-Action`/Origin check passes.

Upstreams resolve at request time through `resolver ${DNS_UPSTREAM}` (the
cluster DNS ClusterIP injected by the manifest), so the terminal pod can start
before or after either backend.

## Reused logic

The client logic is ported from the two existing WebUIs rather than
reinvented:

- `src/lib/feed.ts` — socket + control frames + symbol switching from
  `market_data_system/web/app.js`
- `src/lib/book.ts` — the snapshot/diff assembler (`U <= lastUpdateId + 1 <= u`,
  gap → stale until resync) from the same page
- `src/lib/desk.ts` — the order-desk REST client (`fetchJSON`/`postJSON`,
  ticket bodies) from `order_system/web/app.js`
- `src/hooks/` — the desk's state poll (1 s), event cursor-follow (700 ms,
  trim-aware reset) and debounced freeze preview (250 ms) as React hooks

New in the terminal: the Binance trade-page layout (book+tape left, chart over
order tabs center, ticket right), cumulative depth bars on a ladder anchored
on the spread, and amend via a form instead of `window.prompt`.

### Chart data path

The engine's feed only publishes a kline bar when a later fill lands in the same
or a newer window, so a quiet market produces no `kline_*` frames at all (see
`TODO.md` P2 item 11). The terminal therefore builds bars from the trade stream
itself (`src/lib/bars.ts`): every print folds into the `1s` and `1m` windows, and
a service kline frame overwrites the window it covers, because it carries the
exact volume/trade count and the closed flag. The chart calls `setData` with the
whole merged series on each 10 fps flush, so switching symbol or interval cannot
desynchronise it, and a single fill paints a candle immediately. The service
replays its retained bars on subscribe, which gives a cold page load its history.

## Development

```sh
cd trading_terminal
npm install
npm run dev          # proxies /md and /demo/api to the live fleet
npm run check        # tsc --noEmit
npm run build        # dist/
```

## Deploy (OrbStack lab)

```sh
npm run build                                     # or let Docker build it
docker build -t matchengine-trading-terminal:arm64-v2 .
kubectl apply -f k8s_control_plane/deploy/terminal.yaml
open http://terminal.match-eu-1.example/
```

`k8s_control_plane/deploy/terminal.yaml` renders Deployment/Service/Ingress
(`terminal.match-eu-1.example`) plus two NetworkPolicies: the terminal's own
(ingress from the gateway namespace; egress to DNS, the global market-data
service on 24804 and the order-system BFF on 24300) and a supplementary
additive policy admitting terminal pods to the global market-data pods,
whose operator-rendered policy otherwise only admits gateway/observability/
control-plane namespaces.

A `TradingTerminal` CRD rendered by the operator (like `MarketDataService`/`OrderSystem`)
is a deliberate follow-up; this manifest is applied and torn down by hand.

## Notes

- The freeze/estimate panel is a demo estimate from the order-system BFF;
  the engine holds no balances.
- Symbols/targets: the terminal lists markets from `/md/info` and pairs each
  with the order-desk target of the same id (7 targets on the reference
  fleet). A target whose engine is stopped shows the desk's transport error
  in the warning banner.
- `client_order_id` is generated per submit, so double-clicks place two
  orders — same semantics as the order desk.
