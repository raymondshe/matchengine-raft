# Schema v11 order-type batch: plan

Status: **implemented 2026-11-10.** Baseline moved from snapshot v10/wire v6 to **v11/v7**.
This document fixes the data model, semantics, apply order and migration for one batch adding
market-to-limit, trailing stops, pegged orders and self-trade prevention. The four features share
one snapshot migration, one wire-version bump and one conformance batch; do not land them
separately.

Out of scope: custom iceberg reserve replenishment and stop-protection collars (product-conditional,
P3), fee-token support, derivatives and multi-symbol books.

## 1. Why one batch

Each feature touches at least one persisted `Order` field or `SymbolConfig`, and each adds behaviour
that must be replayed identically on every replica. Splitting the work would force multiple
snapshot versions and multiple market-data wire versions. One batch pays for:

- one `SNAPSHOT_VERSION` bump (v10 -> v11);
- one `market_data::wire::VERSION` bump (v6 -> v7) for new cancellation reasons;
- one conformance/property/replay matrix covering the new order shapes;
- one API and operator migration note.

Old snapshots are rejected rather than migrated; the node's data directory must be recreated. The
wire decoder is strict, so old and new market-data consumers do not share a stream across the bump.

## 2. Data model

### 2.1 `Order` additions

All fields are `#[serde(default)]` and fit the existing flat, `Copy` order record. Internal fields
are persisted because snapshots and the Raft log must replay the same state.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `owner_id` | `Option<u64>` | `None` | Trader identity used only by STP. `None` means "no identity"; STP never matches anonymous orders. |
| `market_to_limit` | `bool` | `false` | Set on a `Market` order whose unfilled remainder should rest at the last execution price. |
| `trail_offset` | `Fixed` | `0` | Positive trailing distance for a trailing stop; `0` means not a trailing stop. |
| `trail_reference` | `Fixed` | `0` | Best price observed since placement (sell: high-water mark; buy: low-water mark); `0` means not initialised yet. |
| `peg` | `Option<PegReference>` | `None` | Peg target; `None` means a normal limit order. |
| `peg_offset` | `Fixed` | `0` | Signed price offset applied to the peg target. |

`OrderType` stays `Market`/`Limit`; market-to-limit is a flag rather than a third enum variant so
existing `live` matches and validators keep two execution styles. A market-to-limit order starts as
`live = Market`, `market_to_limit = true`, and is rewritten to `live = Limit` only for the resting
remainder.

`Order::new` adds all fields with defaults, so existing callers remain plain GTC limit orders.
`Order` equality keeps its current identity/size fields (`id`, `side`, `symbol_id`, `sequence`,
`price`, `volume`); the new fields do not change equality.

`PegReference` is a new `Copy` enum:

```rust
pub enum PegReference {
    Primary,   // own-side best: buy -> best bid, sell -> best ask
    Midpoint,  // midpoint of the non-pegged BBO
    LastTrade, // most recent trade price
}
```

`Order::is_stop()` becomes `stop_price > 0 || trail_offset > 0`. A trailing stop stores its current
derived trigger in `stop_price` for display, cancel/amend keys and snapshot readability; the
authoritative state is `trail_reference + trail_offset` (see §3.2).

### 2.2 `SymbolConfig` addition

```rust
pub enum StpMode {
    Off,             // default; preserves v10 behaviour
    CancelResting,   // cancel the same-owner maker, keep matching
    CancelAggressor, // cancel the taker remainder, keep the maker
    CancelBoth,      // cancel both and stop
}

pub struct SymbolConfig {
    // ... existing fields ...
    pub stp: StpMode,
}
```

`StpMode` defaults to `Off`, so deployments that do not populate `owner_id` keep the current
behaviour. A deployment enabling STP must set non-`None` owner ids at the gateway; two orders with
`None` never self-trade-prevent, and `Some(0)` is a real owner.

### 2.3 New rejection/cancellation reasons

Add to `RejectReason`:

- `SelfTradePrevented` - a maker or taker was cancelled by STP.
- `PegWouldCross` - a pegged order's target would cross the opposite touch.
- `PegReferenceUnavailable` - no non-pegged BBO / last trade exists for the chosen peg.

`Cancelled` events gain a reason only for these cases; an ordinary cancel keeps `reason: None`.
`MatchEvent::for_cancelled_volume` gets a sibling with an explicit `reason`. This is why the wire
must move to v7: `reason_code`/`decode_reason` gain codes 9, 10 and 11.

## 3. Semantics

### 3.1 Market-to-limit (MTL)

- Entry: `live = Market`, `market_to_limit = true`; `price` is ignored (send `0`).
- Sweep: exactly like a market order, across every level, with the same quote-volume rules as a
  market order (base volume only for this batch; quote MTL is rejected).
- Residual: if at least one fill occurred, the same order rests as a limit at the **last fill
  price** (the maker price of the final fill), preserving its GTC/GTD/Day time in force and its
  taker sequence. `live` becomes `Limit` and `market_to_limit` is cleared, so the resting record is
  an ordinary limit. It must not cross: the sweep has consumed every better level, so the next
  opposite level is worse.
- No fill: the whole order is cancelled; there is no reference price to rest at. It is not a
  rejection, because the market was merely empty.
- Invalid combinations: `market_to_limit` with `tif = IOC`/`FOK`, `post_only`, `stop_price > 0`,
  iceberg fields or quote volume -> `InvalidOrder`. `price` is ignored (send `0`), matching the
  existing market-order rule.
- Amendments: a rested MTL remainder is an ordinary limit order and follows the existing amend
  rules. The original market-to-limit intent is not retained.
- Events: fills first, then either `Rested` at the last fill price or `Cancelled` for the remainder.

### 3.2 Trailing stops

- Entry: `trail_offset > 0`, `stop_price = 0`, `trail_reference = 0`; `live` is `Limit` or `Market`
  and the child behaves like the existing stop-limit/stop-market child. A trailing stop cannot be
  an iceberg, post-only, pegged or quote-volume order.
- Initialisation: if `last_trade_price` exists, `trail_reference` is that print and
  `stop_price` is derived immediately; otherwise the stop waits for the first print to initialise
  the reference. A fresh trailing stop cannot be `StopWouldTrigger` on entry because its trigger is
  one offset behind the reference. A trailing stop whose `trail_reference` is still `0` is not
  electable; prices are positive, so `0` is an unambiguous sentinel.
- `trail_reference` is engine-managed state. A client-sent value is overwritten at placement; the
  field exists in `Order` only because snapshots and Raft replay must carry it.
- Derivation:
  - Sell: `stop_price = trail_reference - trail_offset`; a new print raises
    `trail_reference = max(trail_reference, print)`.
  - Buy: `stop_price = trail_reference + trail_offset`; a new print lowers
    `trail_reference = min(trail_reference, print)`.
- Election: existing rule - sell elects at `last <= stop_price`, buy at `last >= stop_price`.
  Reference updates happen on every print before the stop cascade, so one trade can update a
  trailing stop and a later trade can elect it; the same print that raises/lowers the reference
  does not immediately elect the stop. Reference updates are internal and emit no event; only the
  election and its child events are published.
- Child: the triggered child never rests; whatever it cannot execute is cancelled.
- Amend: `trail_offset` and volume are amendable. Echoing the current derived `stop_price` is
  accepted; a different non-zero `stop_price` is rejected (`UnsupportedOrder`) because the trigger
  is derived. Reducing the offset updates the
  trigger; if the new trigger is already crossed, the amend is refused (`StopWouldTrigger`).
- Expiry: GTD/Day apply to the parked trailing stop exactly as they do to a fixed stop.
- Snapshot: `trail_reference` and the current derived `stop_price` are persisted.

### 3.3 Pegged orders

- Entry: `peg = Some(...)`, `live = Limit`, `price = 0` (computed), base volume only, `tif` GTC/GTD/
  Day, no stop/trailing, no iceberg/post-only. A pegged order is always a resting limit order.
- Reference: pegs use **non-pegged liquidity** only. A peg never uses another pegged order as its
  reference. This is deliberate: it makes one reprice pass deterministic and prevents pegs from
  chasing each other.
  - `Primary`: own-side best non-pegged bid/ask.
  - `Midpoint`: midpoint of the non-pegged BBO (integer midpoint, truncated consistently).
  - `LastTrade`: most recent print.
  - If the reference does not exist, reject at entry (`PegReferenceUnavailable`).
- Price: `target = reference + peg_offset` for both sides; `peg_offset` is signed. A target that
  would cross the opposite touch is refused at entry (`PegWouldCross`) and cancels a
  resting peg on reprice with the same reason.
- Resting and repricing: after every applied entry that can change the non-pegged BBO or last
  trade, reprice every resting pegged order once, in `(side, sequence)` order. A changed target:
  1. removes the old book entry and depth delta;
  2. sets the new price and assigns a **new sequence** (price change loses priority);
  3. re-inserts and records the depth delta;
  4. emits an `Amended` event carrying the previous sequence/price and the new sequence/price.
  A peg whose target would cross is removed and emits `Cancelled` with `PegWouldCross`.
- Placement price: the initial target is computed before validation/resting; the order is rejected
  if the target is missing or crossing. The client-sent `price` must be zero; a non-zero price is
  ignored or rejected? Choose **reject** (`InvalidOrder`) so a client cannot silently submit two
  different prices.
- Amendment: `price` is derived, so an ordinary `Amend` price change is refused
  (`UnsupportedOrder`). `peg_offset` and volume are amendable; changing the offset recomputes the
  target and follows the reprice rules (new sequence if the price changes; cancel if it crosses).
  A peg type change is cancel + place.
- Snapshot: `peg` and `peg_offset` are persisted; the current `price` and `sequence` are the
  resting identity.
- Reprice pass: `OrderBook::reprice_pegged()` is called by `apply` after **every** mutating request
  (`Place`, `Cancel`, `Amend`, `PurgeExpired`) that can change the non-pegged BBO or last trade, not
  only after a place. The method returns `Amended`/`Cancelled` updates in deterministic side/sequence
  order. Direct `OrderBook` callers (unit tests and book benchmarks) call it explicitly when they
  need pegged semantics; `StateMachineStore::apply` is the production caller.
- Performance: the first implementation may scan the book for pegged orders and for the best
  non-pegged quote. A dedicated peg index is a later optimization only if measurements justify it.

### 3.4 Self-trade prevention (STP)

- Identity: `Order.owner_id: Option<u64>`. STP applies only when both taker and maker are `Some`
  and equal. `None` never matches, including `None` vs `None`.
- Modes:
  - `Off`: current behaviour.
  - `CancelResting`: remove the same-owner maker from the book, emit `Cancelled` with
    `SelfTradePrevented`, and continue matching the taker against later makers.
  - `CancelAggressor`: stop the sweep at the first same-owner maker, cancel the taker's remaining
    volume with `SelfTradePrevented`, and leave the maker resting.
  - `CancelBoth`: remove that maker and cancel the taker remainder, both with
    `SelfTradePrevented`, then stop.
- FOK preflight must use the same policy:
  - `CancelResting`: sum reachable volume from makers with a different owner; same-owner makers are
    ignored (they will be cancelled, not filled).
  - `CancelAggressor`/`CancelBoth`: stop at the first same-owner maker; only liquidity before it
    counts. A FOK that would meet that maker is `FokUnfillable` before any state changes.
- Order: same-owner maker cancellations are emitted in match order; the taker cancellation is
  emitted after the last fill/cancel from the sweep. Stop children inherit the parent's owner id
  and apply the same policy.
- Event reasons: `Cancelled` events for both maker and taker carry `SelfTradePrevented`. Do not use
  `Rejected` for a taker that already traded; it was cancelled, not refused.
- Outcome plumbing: `Outcome` gains `stp_cancelled: Vec<Order>` (maker cancellations in match
  order) and a taker `cancel_reason: Option<RejectReason>` so `apply` can publish the events in the
  same order the sweep produced them.
- Snapshot: `owner_id` travels in the order and `StpMode` in `SymbolConfig`.

## 4. Validation matrix

| Shape | Accept | Reject |
|---|---|---|
| MTL | `live=Market`, `market_to_limit=true`, base volume, GTC/GTD/Day, price ignored | IOC/FOK, post-only, stop, iceberg, quote volume |
| Trailing stop | `trail_offset>0`, `stop_price=0`, Limit/Market child, base volume | iceberg, post-only, peg, quote volume, `trail_offset<=0`, non-zero `stop_price` |
| Pegged | `peg=Some`, Limit, price `0`, base volume, GTC/GTD/Day | non-zero price, IOC/FOK, stop/trailing, iceberg, post-only, quote volume |
| STP | `owner_id` optional; config `StpMode` | - |

`Order::new` adds the new fields with defaults; every existing caller remains a plain GTC limit.
`ConfigureSymbol` validates `stp` as an enum (unknown JSON fails deserialization) and locks with
the rest of the config after the first accepted order.

## 5. Apply order and determinism

`StateMachineStore::apply` must produce identical state and events on every replica. The order is:

1. Validate the request and the new order shape.
2. Purge due GTD at the request's intake timestamp (existing rule).
3. If a trailing stop, initialise/derive its reference from the current last trade.
4. If a pegged order, compute the initial target from non-pegged liquidity.
5. Run FOK preflight with the configured STP policy.
6. Sweep with STP, then update `last_trade_price` after every print.
7. Apply MTL residual resting or cancellation.
8. Run the existing stop cascade; trailing references update on each print before election.
9. After the primary request and stop cascade, reprice pegged orders once against the final
   non-pegged BBO/last trade, in deterministic side/sequence order.
10. Append events in this order: primary fills/rests/cancels, STP maker cancels in match order,
    stop-trigger and child events, trailing/expiry events, pegged amendments/cancels.

No wall clock is read. Trailing references and peg targets depend only on replicated state: the
last trade, the non-pegged BBO and the request's recorded fields. A snapshot or log replay therefore
reconstructs the same triggers and prices. The pegged pass is a pure function of the state after
step 8.

## 6. Snapshot and wire migration

### 6.1 Snapshot v11

- `SNAPSHOT_VERSION` becomes **11**.
- v11 adds `Order.owner_id`, `market_to_limit`, `trail_offset`, `trail_reference`, `peg`,
  `peg_offset` and `SymbolConfig.stp`.
- Older snapshots are rejected by the existing version check. There is no v10 -> v11 migration:
  wipe the node's `--data-dir` (or use a fresh one) when moving a deployment across the change.
- Update the snapshot-version tests and `docs/STORAGE.md`; the magic/CRC/atomic-write format is
  unchanged.

### 6.2 Market-data wire v7

- `market_data::wire::VERSION` becomes **7**.
- Add reason codes 9 (`self_trade_prevented`), 10 (`peg_would_cross`) and 11
  (`peg_reference_unavailable`).
- No new market-data fields are required: `LifecycleRecord` already carries the event kind,
  sequence, price/volume, fees and optional reason. The new order-type fields live in the Raft log
  and snapshot, not in the market-data records.
- Update the strict reference decoder, golden round-trip tests, `demos/market_data_system` and the
  wire-version test. Old consumers must be upgraded with the engine; the stream is version-gated.

### 6.3 API

- `Order` accepts the new optional fields; `OrderType` is unchanged.
- `SymbolConfig` accepts `stp` (`"off"`, `"cancel_resting"`, `"cancel_aggressor"`,
  `"cancel_both"`).
- `docs/API.md`, `docs/MATCHING.md`, `docs/MATCH_RESULTS.md`, `docs/FEE_DEDUCTION.md` (quote-volume
  exclusions), `docs/STORAGE.md`, `docs/MARKET_DATA_PLAN.md`, `README.md`, `README_CN.md` and
  `AGENTS.md` are updated in the same change.
- Clients that omit the new fields get the exact v10 behaviour: plain limit/market orders, no
  trailing/peg, STP off.

## 7. Conformance and test matrix

### 7.1 Unit / conformance (`src/order_book/`)

- **MTL**: sweep with residual rests at the last fill price; no-liquidity cancels whole; residual
  does not cross; IOC/FOK/post-only/stop/iceberg/quote rejection; `market_to_limit` is cleared on
  the rested remainder; events.
- **Trailing**: sell and buy reference updates; first print initialisation; no same-print election;
  election on a later trade; offset amend; crossed-trigger amend refused; GTD/Day expiry; stop
  cascade; snapshot fields; exact 18-decimal offsets.
- **Pegged**: primary/midpoint/last-trade initial targets; no non-pegged reference; reprice on BBO
  and last-trade changes; non-pegged reference excludes pegs; price-change loses priority/new
  sequence; cross cancels; amend offset; snapshot restart; reject combos.
- **STP**: all three active modes; anonymous owners never match; FOK preflight per mode; stop-child
  inheritance; maker/taker event reasons; no crossed book; cancel/amend of an STP-cancelled order
  is `OrderNotFound`.

### 7.2 Property tests

Extend the reference model with owner ids and the three active STP modes, and add generators for
MTL/trailing/peg. Assert quantity conservation, no crossed book, deterministic replay, and that
`CancelResting` never removes non-same-owner liquidity. Keep `Off` as the default seed corpus so the
existing model remains a strong baseline.

### 7.3 Cluster / replay / snapshot

- `tests/cluster/test_feature_matrix.rs`: one MTL, trailing and peg lifecycle through `/write` and
  `/events`; STP under every mode; event reconstruction of the visible book.
- `src/store/state_machine/tests.rs`: v11 snapshot reopen with trailing reference, peg target,
  owner ids and STP config; old v10 snapshots are rejected by the version check.
- Wire v7 golden round-trip for the three new reasons; market-data service decode.
- Replay fixtures for the new shapes remain optional follow-ups; the cluster feature matrix covers
  the same paths through `/write` end-to-end.

## 8. Delivery sequence

1. **Data model + snapshot v11** (no behaviour change; default fields, version bump, reopen tests).
2. **Market-to-limit** (sweep + residual + events + conformance).
3. **Trailing stops** (reference update, election, amend, expiry, conformance).
4. **STP** (owner id, config, FOK preflight, sweep policy, events, property tests).
5. **Pegged orders** (reference selection, placement, deterministic reprice pass, events).
6. **Wire v7** (reasons, decoder, golden tests, market-data demo).
7. **Docs, replay matrix and roadmap** (API/MATCHING/MATCH_RESULTS/STORAGE/AGENTS).

Each step keeps the default `StpMode::Off` and omitted new fields equivalent to v10 behaviour, but
the snapshot version is bumped once in step 1 and the whole batch lands behind one release note.

## 9. Open decisions resolved by this plan

| Decision | Choice |
|---|---|
| STP owner identity | `Order.owner_id: Option<u64>`; `None` never matches; `SymbolConfig.stp` defaults `Off` |
| STP policies | `CancelResting`, `CancelAggressor`, `CancelBoth`; no decrement mode in this batch |
| MTL residual price | Last fill price; no fill -> cancel whole |
| Trailing reference | Last trade at placement, else first print; derived trigger stored in `stop_price` |
| Peg reference | Non-pegged primary/midpoint/last-trade; one reprice pass; no peg-to-peg loops |
| Peg price change | New sequence, `Amended` event; cross -> `Cancelled(PegWouldCross)` |
| Snapshot | v11, reject v10, no migration |
| Wire | v7, new reason codes, strict decoder |
| Out of scope | custom reserve replenishment, stop-protection collars, fee tokens, derivatives |

## 10. Implementation notes (2026-11-10)

All seven delivery steps landed in one change. Notes for maintainers:

- MTL rests the residual on the same order record, preserves GTC/GTD/Day and the taker sequence,
  and clears `market_to_limit`; a no-fill MTL is an ordinary `Cancelled` with no reason.
- Trailing references are updated on every fill, including stop-cascade fills. The derived
  `stop_price` is updated in the same pass; reference updates emit no event.
- Peg repricing is a single pass over pegged orders in `(side, sequence)` order. Pegs reference
  non-pegged liquidity only, so the pass cannot chase itself. `OrderBook::reprice_pegged` is public
  for direct book tests; `apply` calls it after every mutating request.
- A pegged `Amend` uses a non-zero `order.peg_offset` as the new offset; zero keeps the current one
  (cancel and place to reset it to zero). `price` on a pegged amend is refused.
- STP maker cancellations are interleaved with fills using `StpCancel.after_fills`; the taker
  remainder cancellation follows the fills. `Cancelled` carries `self_trade_prevented`, not
  `Rejected`.
- Snapshot v11 and wire v7 are one-way. Old snapshots are rejected; old market-data consumers must
  be upgraded with the engine.

## 11. Risks and guardrails

- **Peg reference loops**: avoided by excluding pegged orders from the reference. If a future change
  allows peg-to-peg references, it needs a bounded fixed-point iteration and a new conformance
  suite.
- **STP and FOK**: the preflight and the sweep must use the same owner/policy function; test every
  mode with a multi-maker FOK.
- **Trailing reference state**: persist `trail_reference`; do not derive it from the last trade
  alone, or a restart after a retrace would reset the trigger.
- **Event ordering**: STP maker cancels interleave with fills; pegged amendments come last. A
  consumer that assumes one event per request must be updated.
- **Wire compatibility**: v7 rejects v6. Upgrade the engine and the market-data service together;
  do not mix versions across a stream.
- **No silent default change**: `StpMode::Off`, no trail/peg fields and no MTL flag must reproduce
  the current book and event stream exactly.
