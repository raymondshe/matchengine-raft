# Fee Deduction Design: Base vs Quote Volume

Status: **core implemented.** The engine now carries base/quote asset metadata, tick/lot rules,
`VolumeUnit`, quote-aware sweeps and `FeeAssetPolicy`. Snapshot **v9** and wire **v5** encode the
new fields. Extensions still open are listed in §13 (sessions/lifecycles, fixed/regulatory fees,
fee tokens, net-volume semantics).

## 1. Goals

- Support cash instruments with a **base asset** and a **quote asset**, including fiat/stablecoin
  quotes and ETF/equity/crypto bases.
- Allow an order to specify its gross size in either **base** or **quote** units.
- Charge maker and taker fees at user-level rates supplied with the order.
- Make the fee asset explicit and deterministic.
- Keep the matching engine independent of user accounts, balances and fee tiers.
- Preserve exact fixed-point arithmetic and deterministic replay.
- Define the changes required in `OrderBook::execute_order`, events, snapshots and the feed.

## 2. Terminology

| Term | Meaning |
|---|---|
| Base asset | The asset being bought or sold: QQQ/VOO/GOOGL shares, BTC, ... |
| Quote asset | The pricing asset: USD/GBP fiat, USDT/USDC stablecoin, ... |
| Price `P` | Quote units per one base unit. |
| Base quantity `Q` | Gross amount of the base asset traded. |
| Notional `N` | Gross quote value of a fill: `N = P * Q`. |
| Fee rate `r` | Signed fraction of notional. Positive = charge, negative = rebate. |
| Gross | Amount before fees. |
| Net | Amount after fees. |
| Fee asset | Asset in which a side pays or receives the fee. |
| Maker | The resting order. |
| Taker | The incoming order that crosses the spread. |

The engine works in scale-18 `Fixed` internally. Settlement precision is described by the symbol
configuration, not by the matching arithmetic.

## 3. Asset classes and instrument metadata

The design must not assume the base is a crypto coin or the quote is USDT. A pair can be:

| Side | Examples |
|---|---|
| Base | ETF/equity: QQQ, VOO, GOOGL. Spot crypto: BTC, ETH. |
| Quote | Fiat: USD, GBP. Stablecoin: USDT, USDC. |

This has several consequences:

- `USD` and `USDT` are different assets even when they trade near par. The engine must not convert
  or substitute them implicitly. The fee asset is the actual configured asset.
- `GBP` and `GBp` (pence) may be separate asset ids with different decimal places; the symbol table
  must be explicit.
- A stablecoin has an issuer/chain identity; an ETF/equity has an exchange/ISIN/ticker identity.
  `AssetId` should carry an asset class and enough identity to disambiguate.
- Equity/ETF quantities may be fractional shares, but a symbol can also require whole-share lots.
  Price tick and quantity lot are first-class metadata, not just decimal places.
- Fiat and stablecoin fees settle on different rails: fiat through the clearing ledger, stablecoin
  through the chain/custodian. The engine only reports the fee asset and amount.

Recommended metadata:

```rust
pub enum AssetClass {
    Fiat,
    Stablecoin,
    Crypto,
    Equity,
    Etf,
}

pub struct AssetId {
    pub code: String,       // USD, USDT, QQQ, BTC, ...
    pub class: AssetClass,
    pub decimals: u8,       // settlement precision
}

pub struct SymbolConfig {
    pub symbol_id: u32,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub price_decimals: u8,
    pub price_tick: Fixed,  // minimum price increment
    pub base_lot: Fixed,    // minimum base quantity increment
    pub fee_asset_policy: FeeAssetPolicy,
    // session calendar / corporate actions are deployment metadata; see below
}
```

Examples:

| Pair | Base | Quote | Tick / lot | Default fee asset |
|---|---|---|---|---|
| QQQ/USD | ETF | Fiat | 0.01 USD / 0.0001 share | Quote: USD for both sides |
| VOO/USD | ETF | Fiat | 0.01 USD / 0.0001 share | Quote: USD for both sides |
| GOOGL/USD | Equity | Fiat | 0.01 USD / 0.0001 share | Quote: USD for both sides |
| BTC/USDT | Crypto | Stablecoin | 0.01 USDT / 0.00000001 BTC | Received: BTC for buyers, USDT for sellers |
| BTC/USDC | Crypto | Stablecoin | 0.01 USDC / 0.00000001 BTC | Received |
| EUR/USD | Fiat | Fiat | 0.0001 USD / 0.0001 EUR | Received: EUR for buyers, USD for sellers |

Quote-volume orders are especially useful for instruments bought by cash amount, such as
“buy 1,000 USD of QQQ.” Base-volume orders remain natural for “sell 10 shares of QQQ” or
“sell 0.5 BTC.” The engine must still support both directions of both units.

### 3.1 Sessions, settlement and corporate actions

- Crypto pairs can trade continuously; equities/ETFs and fiat FX have sessions, holidays and
  auctions. The matching engine must not read a clock. The gateway validates the trading session
  and supplies `Day` close/GTD timestamps through the existing explicit purge path.
- Equity/ETF settlement may be T+1 or T+2 while stablecoin settlement is on-chain. The engine
  reports fees and net amounts; the clearing layer owns settlement timing.
- Splits, dividends, symbol changes and other corporate actions can change the base quantity or
  price basis. Those require a symbol halt and a controlled adjustment/re-listing, not a fee
  calculation. They are outside the matching engine but must be part of the symbol lifecycle.
- Notional-proportional maker/taker rates cover the common crypto and brokerage tiers. Fixed
  per-trade fees, per-share commissions, regulatory fees (for example SEC/FINRA fees), minimum
  tickets and taxes are not modelled by this design; they need a separate fee-rule extension.
- Short selling, margin, lending, dividends and tax withholding are outside this matching-engine
  design.

## 4. Baseline this design replaced

Before this design was implemented, the engine behaved as follows:

- `Order.volume` is always treated as a base quantity.
- `Order.maker_fee_rate` and `Order.taker_fee_rate` are fractions of notional.
- A fill computes `taker_fee` and `maker_fee` but does not say whether the amounts are base or
  quote.
- The order book assumes `volume` can be compared directly with maker base volume.
- There is no distinction between a buy spending 1,000 USD/USDT and a buy buying 1,000 USD/USDT
  worth of base at an unknown execution price.
- Stop, iceberg and amendment paths inherit the same base-quantity assumption.

Consequences:

- A quote-denominated order cannot be matched correctly by converting at one price after the fact.
- A fee charged in base cannot be compared or reported against a fee charged in quote.
- Net balances are ambiguous: a buyer paying a base fee receives less base, while a buyer paying a
  quote fee spends more quote.
- Settlement precision cannot be applied without knowing the fee asset.

## 5. Two independent axes

The design separates two concerns that the current code mixes:

1. **Volume unit** — how the client specifies the order size.
2. **Fee asset policy** — which asset is charged or rebated.

Fee rates remain per order and per role. The volume unit does not change the user-level rates.

### 5.1 Volume unit

Add an explicit order field:

```rust
pub enum VolumeUnit {
    Base,
    Quote,
}
```

`volume` is always a **gross** amount in that unit. Fees are reported separately and never reduce
the amount matched. This keeps the matching loop independent of fee arithmetic.

#### Base volume

`volume` is the gross base quantity `Q`.

- Matching consumes base quantity directly.
- Notional per fill is `P * Q`.
- Limit and market orders are both supported without special matching logic.

#### Quote volume, limit orders

For a limit order at limit price `P_limit`, normalize at admission:

```text
Q = floor_to_base_lot(volume / P_limit)
```

If `Q == 0`, reject the order rather than placing a zero-size book entry. The original quote amount
and unit should remain available for audit/events.

- A buy at or below its limit spends at most the requested quote amount.
- A sell at or above its limit receives approximately the requested quote value. Exact proceeds
  depend on the fill price and base-lot rounding.
- The normalized base quantity becomes the resting quantity; the order book does not need a
  quote-volume matching mode for limit orders.

Example: a QQQ/USD buy with `volume_unit = Quote`, `volume = "1000"`, `price = "400.10"` normalizes to `floor_to_share_lot(1000 / 400.10)` shares. It spends at most 1,000 USD at or below 400.10; a better fill spends less.

Decision: a quote limit sell rounds the base quantity **down**, matching the buy side. Rounding
down prevents selling more notional than requested; the alternative (rounding up) would guarantee
minimum gross proceeds but overshoot the requested quote value.

#### Quote volume, market orders

A market order has no limit price to normalize against, so it remains in quote mode while sweeping:

- **Market buy:** `volume` is a gross quote **spend cap**. At each maker price `P`:

  ```text
  Q_fill = min(maker.remaining, floor_to_base_lot(remaining_quote / P))
  remaining_quote -= P * Q_fill
  ```

  Stop when no positive base lot fits or liquidity is exhausted. Any quote remainder below one base
  lot is cancelled and reported as a remainder.

- **Market sell:** `volume` is a gross quote **proceeds target**. At each maker price `P`:

  ```text
  Q_fill = min(maker.remaining, ceil_to_base_lot(remaining_quote / P))
  remaining_quote -= P * Q_fill
  ```

  The final fill may overshoot the target by at most one base lot. Stop once proceeds reach the
  target or liquidity is exhausted. If exact/no-overshoot semantics are required, use floor and
  accept a small shortfall instead; that is an open decision.

Quote market orders never rest. A stop-market child uses quote mode when elected; a stop-limit
child converts at its limit price when placed.

#### Icebergs, stops and amendments

- `display_volume` and `hidden_volume` use the same unit as the order's volume.
- For quote limit orders, convert all three to base quantities at admission; the visible/hidden
  split must remain consistent.
- For quote stop-market orders, keep quote mode until the stop is elected.
- Amendments cannot change `volume_unit`; the new volume is interpreted in the existing unit.
- Fee rates are preserved across amendments, refills and stop children.

## 6. Fee asset policies

Add a pair-level policy to `SymbolConfig`:

```rust
pub enum FeeAssetPolicy {
    /// Buyer pays/receives fee in base, seller in quote. Default for crypto/spot and fiat FX.
    Received,
    /// Both sides pay/receive fees in quote.
    Quote,
    /// Both sides pay/receive fees in base.
    Base,
    // FeeToken(AssetId) is deliberately out of scope for this phase.
}
```

`SymbolConfig::with_assets` automatically selects `Quote` when the base asset is
`AssetClass::Etf` or `AssetClass::Equity`; `validate` rejects any other policy for those bases.
Crypto/spot and fiat-to-fiat pairs keep `Received` by default. A broker that wants cash fees for
crypto can explicitly select `Quote`.

### 6.1 Gross fee formulas

For one side of a fill:

```text
fee_quote = N * r
fee_base  = Q * r
```

where `r` is that side's maker or taker rate. The two formulas are economically equivalent before
rounding; the chosen fee asset determines which one is used.

### 6.2 Deduction table

Positive amounts are charges; negative amounts are rebates.

| Policy | Buyer fee | Seller fee |
|---|---|---|
| `Received` | Base: `Q * buyer_rate` | Quote: `N * seller_rate` |
| `Quote` | Quote: `N * buyer_rate` | Quote: `N * seller_rate` |
| `Base` | Base: `Q * buyer_rate` | Base: `Q * seller_rate` |

The buyer's rate is the taker rate when the buy order is the taker and the maker rate when it is
the maker. The same role rule applies to the seller.

“Quote” in this table means the symbol's configured quote asset, whether that is USD, GBP, USDT,
USDC or another asset. The engine never substitutes a stablecoin for fiat or vice versa.

### 6.3 Gross and net balances

`Received` (default for crypto/spot and fiat FX):

- Buyer receives `Q - fee_base` base and pays `N` quote.
- Seller delivers `Q` base and receives `N - fee_quote` quote.

`Quote`:

- Buyer receives `Q` base and pays `N + fee_quote` quote.
- Seller delivers `Q` base and receives `N - fee_quote` quote.

`Base`:

- Buyer receives `Q - fee_base` base and pays `N` quote.
- Seller delivers `Q + fee_base` base and receives `N` quote.

A negative fee reverses the direction: the side receives the rebate instead of paying it.

The engine only **reports** these amounts. It does not hold or debit balances. The gateway/clearing
system must reserve the required assets and apply the final settlement precision.

### 6.4 Interaction with volume unit

The fee formula is always based on the gross fill (`Q` and `N`); the volume unit only changes how
the order is sized and how the remainder is tracked:

| Order volume | Matching remainder | Fee in received asset | Fee in quote | Fee in base |
|---|---|---|---|---|
| Base | Base `Q` | Buyer: `Q * r` base; seller: `N * r` quote | Both: `N * r` quote | Both: `Q * r` base |
| Quote limit | Normalized base `Q` | Same as base volume | Same as base volume | Same as base volume |
| Quote market buy | Quote spend cap | Taker buy: `Q * r` base | Taker buy: `N * r` additional/debited quote | Taker buy: `Q * r` additional/debited base |
| Quote market sell | Quote proceeds target | Taker sell: `N * r` quote | Taker sell: `N * r` deducted from proceeds | Taker sell: `Q * r` additional/debited base |

Maker fees use the maker order's maker rate and the maker's received asset, exactly like taker
fees. A quote-market buy budget is a **gross notional** cap; when the fee asset is quote, the fee is
an additional quote debit unless the gateway defines the budget as fee-inclusive. That choice is
listed in the open decisions.

## 7. Rounding and precision

`SymbolConfig` should describe the pair, its two assets and its tradable increments:

```rust
pub struct SymbolConfig {
    pub symbol_id: u32,
    pub base_asset: AssetId,   // AssetId carries class + settlement decimals
    pub quote_asset: AssetId,
    pub price_decimals: u8,
    pub price_tick: Fixed,
    pub base_lot: Fixed,
    pub fee_asset_policy: FeeAssetPolicy,
    // optional fee-token configuration later
}
```

The current `quantity_decimals` cannot represent both base and quote volume; asset decimals belong
to `AssetId`, and the symbol adds tick/lot increments. Validation must reject prices that are not a
multiple of `price_tick` and quantities that are not a multiple of `base_lot`, not merely values
with too many decimal places.

Recommended rounding rules:

- Base quantity: floor/ceil to `base_lot` according to the volume-unit rule above.
- Quote notional: compute exactly at scale 18; settlement quantizes to `quote_asset.decimals`.
- Fee amounts: report at scale 18 with the fee asset; clearing rounds to that asset's precision.
- Positive fees round **up** and rebates round **down** at settlement, so the venue never
  under-collects or over-pays.
- Fiat fees settle in the fiat ledger's minor units; stablecoin fees in the chain's smallest unit;
  fractional-share fees in the base asset's share precision.
- If a fee rounds to zero at the asset's minimum unit, no minimum fee is applied in this phase.
- All rounding is deterministic and independent of node-local configuration.

Decision: the engine reports exact scale-18 fees plus the explicit fee asset; the clearing layer
applies the final asset precision. This keeps matching deterministic and avoids a node-local
rounding policy.

## 8. Impact on order-book execution

### 8.1 Admission

`OrderBook::execute_order` currently consumes `order.volume` as base. It needs an order
normalization step before validation/matching:

```text
Base volume        -> Q = volume
Quote limit        -> Q = floor_to_base_lot(volume / limit_price)
Quote market buy   -> matching mode BuyQuote(volume)
Quote market sell  -> matching mode SellQuote(volume)
```

Reject zero normalized quantity, invalid unit combinations, unsupported market/stop combinations,
and precision violations at admission.

### 8.2 Sweep loop

Introduce a remaining-size state instead of a single `Fixed` base remainder:

```rust
enum Remaining {
    Base(Fixed),
    BuyQuote(Fixed),   // gross quote spend cap
    SellQuote(Fixed),  // gross quote proceeds target
}
```

The maker side is always base-denominated, so each maker fill computes:

```text
Base:       Q = min(maker.remaining, remaining_base)
BuyQuote:   Q = min(maker.remaining, floor_to_base_lot(remaining_quote / price))
SellQuote:  Q = min(maker.remaining, ceil_to_base_lot(remaining_quote / price))
```

After the gross fill is chosen:

- decrement the maker and remaining base/quote amounts;
- compute `N = price * Q`;
- compute taker and maker fees from gross `N`/`Q` and their role rates;
- append the fill with base, quote, fees and fee assets;
- preserve price-time priority; fee logic never changes which maker is selected.

### 8.3 Resting orders

Only base-quantity orders rest. Quote limit orders are normalized to base before insertion.
Quote market orders never rest. This keeps `BidKey`/`AskKey` and the book comparator unchanged.

### 8.4 Stop and iceberg behavior

- A quote limit stop converts at its limit price before parking.
- A quote market stop keeps quote mode until elected.
- An iceberg's visible and hidden quantities are normalized consistently; a tranche cannot produce
  a zero base lot.
- The stop cascade uses the same `Remaining` logic as ordinary orders.
- Fee reporting follows every fill, including stop children and iceberg refills.

### 8.5 Invariants

- No fill consumes more base than the taker requested or more than the maker has.
- A `BuyQuote` order never spends more than its remaining quote budget.
- A `SellQuote` order stops at or just beyond its target; any overshoot is bounded by one base lot.
- All base quantities are whole base lots after normalization.
- Fees never change the matched gross quantity.
- Gross notional and both fees are deterministic from the fill and order rates.
- A rejected order never mutates the book or fee state.
- The book remains uncrossed after every successful execution.

## 9. Data model changes

### 9.1 `Order`

Add:

```rust
pub volume_unit: VolumeUnit, // default Base
pub maker_fee_rate: Fixed,   // already present
pub taker_fee_rate: Fixed,   // already present
```

For a quote limit order, the book needs the normalized base quantity. Recommended representation:

- Keep the client's gross requested amount and unit for API/audit (`requested_volume`,
  `requested_volume_unit`), and
- Store the normalized base quantity as the matchable `volume` on the resting order.

If the implementation prefers not to extend `Order`, introduce an internal `RestingOrder` wrapper
with `base_remaining` and the original metadata. The book should not compare quote amounts as if
they were base quantities.

### 9.2 `MatchResult`

Replace the current unitless fee fields with explicit asset-aware fields:

```rust
pub struct MatchResult {
    pub price: Fixed,
    pub base_quantity: Fixed,   // gross
    pub quote_notional: Fixed,  // gross
    pub taker_fee: Fixed,
    pub taker_fee_asset: FeeAsset,
    pub maker_fee: Fixed,
    pub maker_fee_asset: FeeAsset,
    pub fee_overflow: bool,
    // existing taker/maker order identity
}
```

`FeeAsset` names the side of the pair (`Base`/`Quote`); the actual `AssetId` comes from
`SymbolConfig`. A future fee-token extension can replace it with an explicit `AssetId` and a
fee-price model without changing the matching arithmetic.

### 9.3 Events and feed

Each fill event should expose:

- gross base quantity;
- gross quote notional;
- taker fee and fee asset;
- maker fee and fee asset;
- `fee_overflow`;
- requested volume unit (for audit);
- any quote remainder or quote overshoot.

The current `volume` field can remain the gross base quantity for compatibility, with new explicit
`quote_volume` and fee-asset fields. The feed's `TRADE` and fill `LIFECYCLE` records need the same
fields.

## 10. API examples

### Base volume

```json
{"Place": {"order": {
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "Base",
  "price": "100.25",
  "volume": "4.4",
  "maker_fee_rate": "0.001",
  "taker_fee_rate": "0.002"
}}}
```

### Quote volume, limit

```json
{"Place": {"order": {
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "Quote",
  "price": "100.25",
  "volume": "1000",
  "maker_fee_rate": "0.001",
  "taker_fee_rate": "0.002"
}}}
```

This normalizes to `floor_to_base_lot(1000 / 100.25)` base units at admission.

### Quote volume, market buy

```json
{"Place": {"order": {
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "Quote",
  "live": "Market",
  "volume": "500",
  "taker_fee_rate": "0.002"
}}}
```

This spends at most 500 quote units while sweeping; any remainder below one base lot is cancelled.

## 11. Versioning and migration

This was implemented as a breaking storage/wire change:

- `Order` gained `volume_unit` and normalized/requested volume metadata.
- `SymbolConfig` gained base/quote asset metadata and the fee policy.
- `MatchEvent`/`MatchResult` gained quote notional and fee-asset fields.
- Snapshot version is **v9**.
- Market-data wire format is **v5**.
- Existing snapshots cannot be decoded into the new shape; data directories must be recreated or
  a versioned decoder must be added.
- `volume_unit` defaults to `Base` and fee policy to `Received` for JSON compatibility, but old
  postcard entries are not compatible.

Migration order used:

1. Add `SymbolConfig` base/quote assets, asset decimals and fee policy; add `volume_unit` with
   `Base` default; add fee asset fields to events (no matching change).
2. Normalize quote limit orders at admission; keep base-volume matching.
3. Add quote market buy budget and quote market sell target matching in the sweep.
4. Add fee asset policies and settlement-aware reporting.
5. Bump snapshot/wire versions and update consumers.
6. Add compatibility/versioned decoders only if old data must be retained.

## 12. Test matrix

- Base buy/sell with fee in received asset.
- Base buy/sell with fee in quote.
- Base buy/sell with fee in base.
- Maker rebate in each fee asset.
- Quote limit buy/sell normalization, zero-lot rejection and base-lot rounding.
- Quote market buy spend cap, leftover dust and fee in base vs quote.
- Quote market sell proceeds target and final-lot overshoot policy.
- Partial fills and multiple makers.
- A taker that later rests, then fills as maker.
- Iceberg refills and stop children with both volume units.
- Amendments preserve unit/rates and normalize new quote volume.
- 18-digit exactness, snapshot restart and wire round-trip.
- Determinism across replicas and failover.
- No negative/over-budget remainders; no crossed book.

## 13. Decisions and remaining questions

Implemented choices:

1. Quote limit sell rounds base quantity **down** (cap notional), matching the buy side.
2. Quote market sell uses `ceil_to_base_lot` and permits a final overshoot bounded by one base lot.
3. `volume` is always gross; no net-volume mode is implemented.
4. The fee asset policy is **per symbol**, not per user tier or per order.
5. Only base/quote fee assets are implemented; third-asset fee tokens remain out of scope.
6. The engine reports exact fees at scale 18; clearing applies the asset's final rounding.
7. A quote market buy budget is **gross notional**; quote fees are additional debits.
8. `base_lot` is configurable per symbol and zero disables lot validation.
9. ETF/equity bases force `FeeAssetPolicy::Quote`; crypto/spot and fiat-to-fiat pairs default to
   `Received`.

Still open:

10. Which asset classes must be supported at launch: fiat, stablecoin, crypto, equity, ETF, or a
   subset? `AssetId`/`AssetClass` exist; production asset issuance/validation is still a deployment
   concern.
11. Are fixed per-trade fees, per-share commissions, regulatory fees, minimum tickets or taxes in
    scope? They are not notional-proportional rates.
12. Are equity/ETF sessions, holidays, auctions and corporate actions handled entirely by the
    gateway, or does the engine need a symbol lifecycle/halt state?
13. Can fees be charged in an asset other than base or quote (for example a broker fee in USD on a
    BTC/USDT trade)? That requires a fee-token price and balance handling.

## 14. Recommendation

Adopt the following default contract:

- `volume_unit` is explicit and defaults to `Base`.
- Limit quote orders are normalized to base at the limit price before resting.
- Market buy quote orders are spend-capped; market sell quote orders are proceeds-targeted.
- Fees are computed on gross notional; volumes are gross.
- `FeeAssetPolicy::Received` is the default for crypto/spot and fiat FX: buyers pay base fees,
  sellers pay quote fees. ETF/equity bases force `Quote`, so both sides pay cash fees.
- `AssetId` must identify fiat, stablecoin, crypto, equity and ETF instruments distinctly; USD and
  USDT are never interchangeable.
- Price tick and base lot are first-class metadata; decimals alone are not enough for equities or
  crypto.
- The engine reports exact fee amounts and fee assets; clearing owns balances, fiat/chain settlement
  and final asset precision.
- Equity/ETF trading sessions, holidays and corporate actions stay upstream of the deterministic
  matching engine; the gateway supplies session-close/GTD timestamps.
- Snapshot v9 and wire v5 gate the change.

This keeps the order book’s price-time priority unchanged while extending the execution path with
one explicit quote-aware sweep mode and asset-aware fee reporting.
