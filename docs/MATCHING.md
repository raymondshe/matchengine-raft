# Matching

## Order types

An order is one flat `Order` record; every optional type is a field on it. All new fields have serde
defaults, so payloads written before they existed still deserialize. The semantics follow
[OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs):

| Type | Field | Behaviour |
|---|---|---|
| GTC (default) | — | Rests until filled or cancelled. |
| IOC | `"tif": "IOC"` | Executes what crosses now; the remainder is cancelled and reported by a `cancelled` event. |
| FOK | `"tif": "FOK"` | A preflight sums the reachable visible + hidden depth at or inside the limit. Unless the whole order fits, it is refused (`rejected`, reason `fok_unfillable`) and the book is untouched. |
| GTD | `"tif": {"GTD": 1700000000000}` (Unix ms) | Rests until a mutating request's intake timestamp reaches the deadline (automatic purge) or an explicit `PurgeExpired` evicts it. |
| Day | `"tif": "DAY"` | Expires when `PurgeExpired` is called with `day_close_ms` and `now_ms >= close`; without a close it behaves like GTC. |
| Post-only | `"post_only": true` | Never takes liquidity: it rests in full, or is refused (`post_only_would_cross`) when it would trade. |
| Iceberg | `"display_volume": 100.0` | `volume` is the total; only one display tranche rests. The rest waits in `hidden_volume`, invisible to depth, and is refilled tranche by tranche as the visible part is consumed. |
| Stop-market | `"live": "Market", "stop_price": 95.0` | Parks off the book until a trade at or through the stop price (sell: `<=` stop, buy: `>=` stop), then sweeps every level. The child never rests: its remainder is cancelled. |
| Stop-limit | `"live": "Limit", "price": 96.0, "stop_price": 95.0` | Same election, child bounded by `price`; the remainder is cancelled. |

Market orders (`"live": "Market"`) ignore their own price, sweep the opposite side and cancel the
remainder; they never rest.

Expiry is deterministic: `apply` never reads a clock. Every admissible mutating request
(`Place`, `Cancel`, `Amend`) runs the automatic GTD purge at its intake timestamp before it is
applied, so a taker cannot trade against liquidity that is already due. `Day` still needs a market
close, so only the explicit request evicts it:

```json
{"PurgeExpired": {"now_ms": 1700000000000, "day_close_ms": null}}
```

`PurgeExpired` itself consumes a market sequence; its response is that sequence. Every evicted
order emits an `expired` event with its still-open volume.

A stop is only elected by a *new* trade: one the last trade price already crosses is refused
(`stop_would_trigger`) instead of turning into a market order on entry. One trade can cascade
through several stops; elected stops execute in admission order.

Unsupported combinations (a stop that is also post-only or an iceberg, a post-only market order, a
zero/negative size or price, a GTD deadline not after the order's own `timestamp`) are refused with
a `rejected` event carrying the reason.

## Order amendment

`Amend` changes the price and/or open volume of a resting limit order. The request identifies the
order exactly like `Cancel` — side, current price and sequence — and each new field is optional:

```json
{"Amend": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 7}, "price": 4002.0, "volume": 3.0}}
```

- A same-price reduction keeps the order's sequence, so it keeps time priority.
- A new price or a larger volume re-inserts the order with a new sequence (it loses priority); the
  `/write` response carries the sequence to use from then on.
- An amendment whose new price would cross the opposite side is refused (`amend_would_cross`): a
  resting book never crosses, so taking liquidity is still cancel + place.
- Icebergs, parked stops and market orders are not amendable (`unsupported_order`); cancel and
  re-place them instead.
- A successful amendment emits an `amended` event carrying both the previous and the new order
  sequence; the `/write` response is the amendment's `market_seq` (the new order sequence when the
  order was re-inserted). A refused amendment still consumes a sequence and publishes a `rejected`
  event with the reason.
