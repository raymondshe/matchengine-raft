# Fee Deduction Design: Base vs Quote Volume

Status: **core implemented for cash/spot; equity/ETF and fiat-to-fiat (FX) pairs are covered by the
same model, while derivatives and margin-asset settlement remain a documented extension.** The
engine carries base/quote asset metadata, tick/lot rules, `VolumeUnit`, quote-aware sweeps and
`FeeAssetPolicy`. Snapshot **v9** and wire **v5** introduced those fields; snapshot v10 and wire v6 replaced the
boolean fee-overflow flag with the ordered `error`/`MatchErrors` code set. The current snapshot
**v11** and wire **v7** keep that contract.
Derivatives and margin-asset settlement are a documented extension, not implemented; the model is in
§3.2, the freeze policy is in §6.6.3 and the open items are §13 items 11-13. Sessions, settlement,
corporate actions and fixed/regulatory/minimum/tax fees are owned by the external order system.
Asset freezing and fee funding are also order-system responsibilities: the order system reserves the
trading asset before submission and clearing deducts fees when it processes match results. The engine
never holds balances or enforces a freeze; §6.6 defines the policy.

Restricted or missing today:

- Quote volume supports plain limit and market orders only; quote icebergs and quote FOK are
  rejected at admission (§5.1).
- One process/cluster trades exactly one symbol; multi-symbol is deployment-level isolation.
- Derivatives, third-asset/margin fees, funding, liquidation and PnL are out of scope for the
  implemented engine; the extension model and freeze sizing are in §3.2 and §6.6.3.
- The engine does not hold balances, freezes or margin. The order system must size the freeze and
  clearing must deduct fees before crediting proceeds (§6.6).
- Fee rates outside ±100% are rejected; a zero tick or lot disables only that increment check.

## 1. Goals

- Support cash instruments with a **base asset** and a **quote asset**, including fiat/stablecoin
  quotes and ETF/equity/crypto bases.
- Allow an order to specify its gross size in either **base** or **quote** units.
- Charge maker and taker fees at user-level rates supplied with the order.
- Make the fee asset explicit and deterministic.
- Define the order-system freeze and clearing fee-funding policy for cash/spot, FX, equity/ETF and
  derivatives.
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
| Fee rate `r` | Signed fraction of the fee basis: notional for a quote fee, base quantity for a base fee. Positive = charge, negative = rebate. |
| Gross | Amount before fees. |
| Net | Amount after fees. |
| Fee asset | Asset in which a side pays or receives the fee. |
| Freeze `F` | Amount the order system reserves in a freeze asset before submitting an order. |
| Freeze asset `A_f` | Asset reserved by the order system; it is not necessarily the fee asset. |
| Margin | Collateral reserved for a leveraged derivative, normally `notional / leverage`. |
| Clearing | The external layer that consumes match results, deducts fees and settles frozen assets. |
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
  auctions. The matching engine must not read a clock. The external order system validates the
  trading session and supplies `Day` close/GTD timestamps through the existing explicit purge path.
- Equity/ETF settlement may be T+1 or T+2 while stablecoin settlement is on-chain. The engine
  reports fees and net amounts; the external order/clearing system owns settlement timing.
- Splits, dividends, symbol changes and other corporate actions can change the base quantity or
  price basis. They are owned by the external order system; the matching engine has no
  corporate-action logic and is halted/reconfigured through that system.
- Notional-proportional maker/taker rates cover the common crypto and brokerage tiers. Fixed
  per-trade fees, per-share commissions, regulatory fees (for example SEC/FINRA fees), minimum
  tickets and taxes are owned by the external order system and are out of scope for this engine.
- Spot short selling, margin lending, dividends and tax withholding are outside this
  matching-engine design. Derivatives margin and third-asset settlement are discussed in §3.2; they
  require an explicit extension and are not part of the implemented `FeeAssetPolicy`.

### 3.2 Derivatives and margin assets (not implemented)

The implemented engine is cash/spot: the base asset is delivered, the quote asset is paid, and a
fee can only be denominated in one of those two assets. A derivative introduces a third role — the
**settlement or margin asset** — and a different notional formula. The common shapes do not all fit
the current `FeeAssetPolicy`:

| Instrument | Price and quantity | Margin / fee asset | Fit with the current model |
|---|---|---|---|
| Linear USDT-margined perpetual or future (BTC/USDT) | USDⓈ per BTC; quantity in BTC or contracts | USDT (`quote`) | Fee asset can be `Quote`, but instrument kind and any contract multiplier are missing |
| Inverse coin-margined perpetual or future (BTC/USD) | USD per BTC; quantity in contracts with a fixed USD value | BTC (`base`) | Fee is `(contracts × contract_value / price) × rate`, not `base_quantity × rate` for raw contract quantity |
| Third-asset margin, for example USDC-collateralised BTC/USD | USD per BTC | USDC, neither base nor quote | No `FeeAsset` variant and no settlement-asset field |
| Quanto or multi-collateral contract | Underlying price and settlement currency differ | A settlement asset or portfolio currency | No FX/conversion metadata; cannot infer the asset |

The engine must not infer a margin asset from `quote_asset` or treat `FeeAssetPolicy::Quote` as a
synonym for “margin”. The USDT-margined linear case happens to have `margin == quote`, so it can be
reported with `Quote` once the instrument metadata exists. Inverse and third-asset contracts need an
explicit settlement asset and their own fee formula.

Recommended metadata extension:

```rust
pub enum InstrumentKind {
    Spot,
    LinearPerpetual,
    LinearFuture,
    InversePerpetual,
    InverseFuture,
    Option,
}

pub struct SymbolConfig {
    // existing fields...
    pub instrument_kind: InstrumentKind,
    // Linear: base units per contract. Inverse: quote value per contract. 1 for spot.
    pub contract_multiplier: Fixed,
    pub settlement_asset: AssetId, // margin, PnL and default fee asset
}

pub enum FeeAsset {
    Base,
    Quote,
    Settlement, // resolves to SymbolConfig::settlement_asset
}
```

Define `Order.volume` explicitly as either underlying quantity or contracts. If it is contracts,
`base_lot` is a contract lot and `contract_multiplier` converts contracts to base-equivalent
quantity for fees and risk. For linear contracts:

```text
N = price * contracts * contract_multiplier
fee_settlement = N * rate
fee_base       = contracts * contract_multiplier * rate
```

For inverse contracts the quote notional is fixed per contract:

```text
N_quote = contracts * contract_multiplier   // multiplier is quote value per contract
Q_base  = N_quote / price
fee_base = Q_base * rate
```

A third-asset fee also needs an explicit conversion from the contract's quote currency to the
settlement asset, or a fee schedule denominated directly in settlement units. The matching engine
must not invent that conversion.

Derivatives add cash flows and risk state that this engine does not implement:

- funding payments for perpetuals;
- liquidation, insurance-fund and auto-deleveraging cash flows;
- initial and maintenance margin, leverage limits and portfolio netting;
- premium, settlement and option exercise/expiry;
- realised and unrealised PnL in the settlement asset.

Those belong to the gateway, risk and clearing layers. The matching engine can still own
price-time matching and report gross fills and fees, but it must not claim to settle margin or
calculate liquidation prices.

## 4. Baseline this design replaced

Before this design was implemented, the engine behaved as follows:

- `Order.volume` is always treated as a base quantity.
- `Order.maker_fee_rate` and `Order.taker_fee_rate` are fractions of notional.
- A fill computes `taker_fee` and `maker_fee` but does not say whether the amounts are base or
  quote.
- The order book assumes `volume` can be compared directly with maker base volume.
- There is no distinction between spending 1,000 USD/USDT and buying 1,000 USD/USDT of base at an
  unknown execution price.
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

  Stop when no positive base lot fits or liquidity is exhausted. The leftover quote is cancelled and
  reported as a `Cancelled` event whose `volume` is the quote amount and whose `volume_unit` is
  `quote`. The leftover is bounded by the quote cost of one base lot (`price * base_lot`), unless the
  sweep stopped because the implied notional cannot be represented (§7.1).

- **Market sell:** `volume` is a gross quote **proceeds target**. At each maker price `P`:

  ```text
  Q_fill = min(maker.remaining, ceil_to_base_lot(remaining_quote / P))
  remaining_quote -= P * Q_fill
  ```

  The final fill may overshoot the target by at most one base lot. Stop once proceeds reach the
  target or liquidity is exhausted; accepting that overshoot instead of a shortfall is the
  implemented choice recorded in §13 item 2.

Quote market orders never rest. A stop-market child uses quote mode when elected; a stop-limit
child converts at its limit price when placed.

Only plain limit and market orders may use quote volume. Combining quote volume with iceberg fields
(`display_volume`/`hidden_volume`) or with `FOK` is deliberately rejected as `invalid_order` at
admission: tranche and fill-or-kill semantics need an explicit base-normalization rule that this
API does not define. Submit those orders in base volume instead. A quote amount must also carry no
more than `quote_asset.decimals` fractional digits.

#### Icebergs, stops and amendments

- `display_volume` and `hidden_volume` use the same unit as `volume`, but icebergs must be placed
  with base volume; the quote combinations are rejected (§5.1).
- A quote stop-market order keeps quote mode until the stop is elected; a quote stop-limit child is
  normalized at its limit price before it is parked.
- An amendment carries no unit: its `volume` is always a base quantity, even for an order placed in
  quote volume, because quote limit orders are normalized at admission. The client's original amount
  stays on the order as `requested_volume`/`requested_volume_unit`.
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

The policy also determines the default order-system freeze asset for cash/spot orders. §6.6 defines
the freeze table, base-volume estimation under `Base`, and the margin-plus-fee rule for derivatives.

### 6.1 Gross fee formulas

For one side of a fill:

```text
fee_quote = N * r
fee_base  = Q * r
```

where `r` is that side's maker or taker rate. They are different numbers: `fee_quote` is worth
`price * fee_base` at the fill price. Either is a valid representation of the same value before
rounding, and the chosen fee asset decides which one is used.

### 6.2 Deduction table

Positive amounts are charges; negative amounts are rebates.

| Policy | Buyer fee | Seller fee |
|---|---|---|
| `Received` | Base: `Q * buyer_rate` | Quote: `N * seller_rate` |
| `Quote` | Quote: `N * buyer_rate` | Quote: `N * seller_rate` |
| `Base` | Base: `Q * buyer_rate` | Base: `Q * seller_rate` |

The buyer's rate is the taker rate when the buy order is the taker and the maker rate when it is
the maker. The same role rule applies to the seller.

The table names only the **fee** asset. The asset the order system freezes and the settlement leg
used to fund the fee are defined in §6.6.

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

These net legs are funded from the order-system freeze described in §6.6. A positive fee is
withheld from the asset being delivered or credited (or from a pre-funded fee reserve) before any
proceeds are credited, so clearing never creates an unfunded negative balance. For `Received`, the
frozen principal is quote for the buyer and base for the seller; the fee is taken from the other
leg.

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
fees. A quote-market buy budget is a **gross notional** cap: when the fee asset is quote, the fee is
an additional quote debit on top of the cap, which is the implemented choice recorded in §13 item 7.
A fee-inclusive budget remains a gateway convention, not an engine option. The order-system freeze
must cover the cap plus any quote fee; under `Base` it also carries a quote-equivalent reserve for
the base fee (§6.6).

### 6.5 Derivatives and margin-asset fees

For derivatives, the relevant fee asset is normally the **settlement/margin asset**, not the
“received” spot asset:

- Linear USDT-margined contracts: margin, PnL and fees are in USDT. If the symbol's quote is USDT,
  `FeeAssetPolicy::Quote` already names the right asset; the notional must still include the
  contract multiplier.
- Inverse coin-margined contracts: margin, PnL and fees are in the base coin, but the fee is derived
  from a fixed quote value per contract and the current price. `FeeAssetPolicy::Base` names the
  right asset, but `fee = Q * rate` is only correct when `Q` is the base-equivalent fill quantity,
  not the raw contract count.
- Third-asset or multi-collateral margin: the settlement asset is neither the symbol's base nor
  quote. Add `FeeAsset::Settlement` (resolved through `SymbolConfig::settlement_asset`) or an
  explicit asset id; do not misuse `Quote` or `Base`.
- `Received` is a spot-delivery policy (buyer receives base, seller receives quote). It is not the
  default for a derivative; a derivative has no base/quote delivery on the fill.

The engine should keep reporting `taker_fee_asset` and `maker_fee_asset` explicitly, even when the
fee amount is zero because of overflow. Risk and clearing must know which asset the zero refers to.
Freeze sizing for derivatives is in §6.6.3: the order system reserves the settlement/margin asset,
adds the estimated fee and applies the leverage set by risk; the engine does not compute margin,
leverage or liquidation.

### 6.6 Asset freezing and fee funding

The matching engine is balance-agnostic: it does not hold accounts, reserve assets or inspect a
freeze. The **order system** freezes the required asset before submitting an order, and the
**clearing/settlement** layer consumes that freeze when it processes match results. This section
defines the order-system contract; it does not add balance state to `OrderBook`.

#### 6.6.1 Cash/spot freeze by policy

For a cash/spot order, the freeze follows the policy below and the fee is funded from the frozen
asset or the settlement leg:

| Policy | Side | Frozen principal | Fee asset | Fee funding at clearing |
|---|---|---|---|---|
| `Received` | Buy | Quote: maximum gross notional `N_max` | Base | Withhold `fee_base` from the base delivery (`Q - fee_base`) |
| `Received` | Sell | Base: gross quantity `Q` | Quote | Withhold `fee_quote` from the quote proceeds (`N - fee_quote`) |
| `Quote` | Buy | Quote: `N_max + fee_quote_max` | Quote | Deduct from the frozen quote |
| `Quote` | Sell | Base: `Q` | Quote | Withhold `fee_quote` from the quote proceeds |
| `Base` | Buy | Quote: `N_max + fee_quote_reserve + buffer` | Base | Withhold `fee_base` from the base delivery; release the matching quote reserve |
| `Base` | Sell | Quote: `N_max + fee_quote_reserve + buffer` | Base | Withhold or acquire `fee_base` from the quote collateral; release the matching quote reserve |

`N_max` is the maximum gross notional the order can execute: `Q * price` for a limit order, or
`Q * protection_price` for a market order. For a `Base` sell it is the worst-case quote cost of
covering or delivering the base (a short-collateral model), not the expected proceeds. Under
`Base`, `fee_quote_reserve` is the base fee converted to quote at a conservative conversion price.
The reserve is not a second fee; it is released when clearing withholds the base fee from the base
leg.

`Received` is the standard spot-delivery policy: the buyer freezes quote to pay the seller, and the
seller freezes base to deliver. Fees are taken from the asset the side receives, so no second freeze
is needed. This is the default for crypto/spot and fiat FX.

`Quote` buys need a quote freeze that covers both notional and the quote fee. `Quote` sells deliver
base and the quote fee is withheld from proceeds; because the fee rate is at most 100%, each fill's
proceeds cover its own fee.

`Base` is a **quote-collateral** policy: the order system freezes quote for both sides even though
the fee is charged in base. It is intended for quote-collateralised instruments such as margin,
short/derivative and quote-settled setups. A spot seller using `Base` must have an independent base
inventory or locate; the quote freeze covers the economic obligation, not physical delivery.

#### 6.6.2 Base policy: estimating the quote freeze from base volume

When a `Base`-policy order is submitted with `volume_unit = base`, the engine matches the gross base
quantity `Q`; it never converts the order to quote and never spends a quote balance. The order system
must translate `Q` into a quote freeze using a conservative risk price and then cap the submitted
base quantity so every possible fill plus fee is covered.

Inputs:

- `Q`: requested gross base volume;
- `P_ref`: conservative risk price. For a buy, use the limit price or a protection price at or above
  expected execution. For a sell, use the price at which quote collateral can buy back or deliver
  the base, including a risk buffer;
- `fee_quote_per_base`: the side's positive fee per base unit, expressed in quote. For `Base` this is
  `max(rate, 0) * P_fee`, where `P_fee` is a conservative conversion price; for `Quote` it is
  `P_ref * max(rate, 0)`;
- `buffer`: a quote buffer for slippage and settlement rounding;
- `leverage`: the risk-set leverage for a derivative; absent for cash.

For a cash order:

```text
cost_per_base = P_ref + fee_quote_per_base
F_quote       = ceil_to_quote_lot(Q * cost_per_base + buffer)
```

For a derivative:

```text
cost_per_base = P_ref / leverage + fee_quote_per_base
F_quote       = ceil_to_quote_lot(Q * cost_per_base + buffer)
```

If the client supplies a quote freeze `F_quote`, invert the same formula:

```text
Q_max = floor_to_base_lot((F_quote - buffer) / cost_per_base)
```

The order system submits `Q <= Q_max`. If the client requests a larger base quantity, reject it with
an insufficient-funds error (or resize only with explicit client consent). Do **not** submit the full
`Q` and rely on the engine to stop at the freeze: the engine does not see `F_quote`. If the market
moves beyond `P_ref` or the fee exceeds the estimate, the order system must top up, cancel the
remainder, or let clearing reject the excess; a fill that breaches the freeze is an order-system risk
event, not a matching-engine rejection.

Size `P_ref`, `fee_quote_per_base` and `buffer` so the highest price the order can execute at
(including market protection and slippage) plus the maximum fee still fits `F_quote`. If that cannot
be guaranteed, the order system must not submit the full base quantity; the engine will still match
it because it has no freeze to stop at.

For a quote-volume order under `Base`, normalize it to base at admission and apply the same `N_max`
formula. The requested quote amount alone is not sufficient: it excludes the fee reserve and, for a
sell, can understate the conservative buy-back price. A freeze that does not exceed `buffer` funds
no base quantity and must be rejected.

A hard engine-side guarantee that no fill exceeds the freeze is not available today. It would need
an explicit versioned limit such as `max_quote_spend` on the order (§9.1); the engine must not infer
it from a balance or a node-local account lookup.

For a sell under `Base`, the quote collateral covers buy-back/delivery of the base and the base fee.
For a buy, the quote collateral covers the notional and a quote-equivalent reserve for the base fee;
clearing withholds the fee from the base delivery.

#### 6.6.3 Derivatives: margin plus fee

For derivatives the freeze asset is the contract's settlement/margin asset: quote for a linear
USDT-margined contract, base for an inverse coin-margined contract, or an explicit third asset for
third-asset margin. When volume is expressed in contracts, first convert it to base-equivalent units
with `contract_multiplier` (§3.2). The order system determines leverage `L` and reserves:

```text
linear:   margin = price * contracts * contract_multiplier / L
inverse:  margin = contracts * contract_multiplier / (price * L)   // in base
fee_est  = fee formula from §3.2/§6.5, in the settlement asset
F        = ceil_to_freeze_lot(margin + fee_est + maintenance_buffer)
```

- Leverage, initial/maintenance margin rates and liquidation are risk-layer inputs; the engine does
  not calculate them.
- A linear contract with quote margin/fee freezes quote; an inverse contract freezes base; a
  third-asset contract freezes its configured settlement asset.
- If the fee asset differs from the freeze asset, reserve the quote/base equivalent explicitly.
  The engine reports the fee asset and amount; it does not convert assets.
- The fee is deducted from the frozen margin when clearing processes each fill. Remaining margin is
  updated by PnL, funding and settlement outside the engine.
- A fill carrying an overflow error code must not be used as a zero fee or zero notional in margin or
  liquidation calculations (§7.1).

#### 6.6.4 Match-result handling and release

For each match result, clearing should:

1. Resolve the fee asset from the policy and side, using the explicit `taker_fee_asset` and
   `maker_fee_asset` on the fill.
2. Consume the frozen principal for the filled quantity.
3. Deduct a positive fee from the frozen asset when it is the fee asset; otherwise withhold it from
   the asset being delivered or credited (Received buy: base delivery; Received sell: quote
   proceeds; Base: base leg) or from a pre-funded fee reserve.
4. Credit the net amount; credit a rebate in the fee asset.
5. Release the unused freeze on cancellation, expiry or terminal fill.

This is per fill and repeated across partial fills. A fee must never create an unfunded negative
balance. If the fee asset differs from the frozen asset and the settlement leg is insufficient, the
order system must reject or top up before submission, or clearing must quarantine the fill for
manual handling.

Example freeze requirements (ignoring buffers and rounding):

| Order | Policy | Freeze |
|---|---|---|
| Buy 1 BTC at 50,000 USDT | `Received` | 50,000 USDT; base fee withheld from BTC delivery |
| Sell 1 BTC at 50,000 USDT | `Received` | 1 BTC; quote fee withheld from USDT proceeds |
| Buy 10 QQQ at 400 USD | `Quote` | 4,000 USD + quote fee |
| Sell 10 QQQ at 400 USD | `Quote` | 10 QQQ; quote fee from USD proceeds |
| Buy 10 QQQ at 400 USD, 0.1% base fee | `Base` | 4,000 USD + 4 USD fee reserve |
| Linear perp, 1 BTC at 50,000 USDT, 10x | `Quote` | 5,000 USDT margin + fee + buffer |
| Inverse perp, fee in BTC, 10x | `Base` | BTC margin + BTC fee + buffer |

The standalone demo order desk (`demos/order_system/src/freeze.rs`) implements a labelled **demo estimate** of the
cash/spot rows above and of the **linear quote-margined perpetual** row: it derives the freeze,
maker/taker settlement legs and, for a perpetual ticket, the leverage, position side, notional,
initial margin and quote fee from the replicated `SymbolConfig`, the order fields and the fee rates.
The perpetual overlay is demo-only — the engine still matches the configured cash symbol and has no
instrument-kind, leverage or margin field. The demo requires `fee_asset_policy = quote` for perpetual
orders, models no contract multiplier, funding, PnL, liquidation or maintenance margin, and treats a
zero `buffer`. It holds no account state, never enforces the freeze, and does not estimate inverse or
third-asset contracts. The engine remains authoritative for the fill's fee fields only.

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
    // base/quote only; third-asset fee tokens are outside the engine
}
```

The earlier single `quantity_decimals` setting could not represent both base and quote volume;
decimals now live on each `AssetId`, and the symbol adds tick/lot increments. Validation rejects
prices that are not a multiple of `price_tick` and quantities that are not a multiple of `base_lot`,
not merely values with too many decimal places; a zero increment disables that check.

Recommended rounding rules:

- Base quantity: floor/ceil to `base_lot` according to the volume-unit rule above.
- Quote notional: compute exactly at scale 18; settlement quantizes to `quote_asset.decimals`.
- Fee amounts: report at scale 18 with the fee asset; clearing rounds to that asset's precision.
- At settlement, positive fees round **up** and rebates round **toward zero** (never more
  negative), so the venue never under-collects or over-pays. The engine itself truncates toward zero
  inside `checked_mul`; this rule only quantizes the reported amount.
- Fiat fees settle in the fiat ledger's minor units; stablecoin fees in the chain's smallest unit;
  fractional-share fees in the base asset's share precision.
- If a fee rounds to zero at the asset's minimum unit, no minimum fee is applied in this phase.
- Order-system freeze amounts round **up** to the freeze asset's settlement precision/lot, never
  down. An order whose freeze cannot cover the maximum obligation is rejected or topped up before
  submission; the engine does not resize it.
- All rounding is deterministic and independent of node-local configuration.

Decision: the engine reports exact scale-18 fees plus the explicit fee asset; the clearing layer
applies the final asset precision. This keeps matching deterministic and avoids a node-local
rounding policy.

### 7.1 Overflow and `error`

Every price, quantity, notional and fee is a scale-18 `Fixed` stored in an `i128`. The largest
representable value is `i128::MAX / 10^18`, about `1.7014e20` units. `Fixed::checked_mul` multiplies
in `i128` when the unscaled product fits and falls back to a 256-bit intermediate otherwise; it
divides the unscaled product by the scale, truncating toward zero, and returns `None` only when the
scaled result cannot fit `i128`. `Fixed::notional` is the same path applied to `price * quantity`.

One fill computes three dependent values independently:

```text
N          = maker_price * matched_volume
taker_fee  = (taker_fee_asset  == Base ? matched_volume : N) * taker.taker_fee_rate
maker_fee  = (maker_fee_asset  == Base ? matched_volume : N) * maker.maker_fee_rate
```

A failure in any one computation sets an error code on the fill:

| Code | Value that is unavailable |
|---|---|
| `notional_overflow` | `quote_volume` (the gross quote notional `N`) |
| `taker_fee_overflow` | `taker_fee` |
| `maker_fee_overflow` | `maker_fee` |

The fill carries `error`, an ordered set of these codes. An empty set (`[]` in JSON) means every
dependent value is exact. Multiple codes can be present and are emitted in the fixed order above.
Non-fill events also carry `[]` together with `null` fee assets; for them the empty set means "no
fill values to report", not "fees computed exactly".

- If `N` overflows, `quote_volume` is `0`; if a fee calculation overflows, that fee is `0`.
- The fill itself is still valid: price, volume, matched gross quantity, order identities and fee
  assets remain exact. Matching never saturates or wraps, and overflow never changes book state.
- `error` is **fill-scoped and non-fatal**. It is not `MatchEngineResponse.error`, which rejects a
  write; a fill carrying codes has already been accepted and committed.
- Quote-asset fees depend on `N`, so a notional overflow normally zeroes both quote fees.
- Base-asset fees depend only on matched volume. If `N` overflows but a base fee still fits, the
  base fee remains nonzero and exact while `error` contains `notional_overflow`.
- A flagged fill can therefore mix valid and unavailable values. Because zero is also a valid fee,
  consumers must use the codes: a zero is exact only when its code is absent. If exact settlement
  is required, quarantine or recompute the flagged fill.
- Fee assets are not affected by overflow: `taker_fee_asset` and `maker_fee_asset` are still
  reported from the symbol policy and order sides. Signed rebates use the same checked path; an
  overflowing rebate becomes `0` with `maker_fee_overflow` or `taker_fee_overflow` set rather than
  being clamped.
- A quote-market taker stops sweeping when the remaining budget and the maker price imply a notional
  that cannot be represented; the leftover quote is cancelled rather than matched further.

JSON example:

```json
{
  "quote_volume": "0",
  "taker_fee": "20000000",
  "taker_fee_asset": "base",
  "maker_fee": "0",
  "maker_fee_asset": "quote",
  "error": ["notional_overflow", "maker_fee_overflow"]
}
```

The base taker fee stays exact; only the quote maker fee and the notional are unavailable. Both fee
assets are still named, so a consumer knows which asset each zero refers to.

Examples:

- QQQ/USD under the ETF quote policy: price `100,000,000,000`, volume `10,000,000,000` gives
  `N = 10^21`, above the scale-18 range. Both quote fees are `0`, assets remain `quote`, and
  `error` is `["notional_overflow", "taker_fee_overflow", "maker_fee_overflow"]`.
- The same fill under a Base/Base policy: `quote_volume` is `0` and `error` contains
  `notional_overflow`, but the base-denominated maker and taker fees remain exact (`0.001` and
  `0.002` of the matched volume).
- Under the default `Received` policy the buyer taker pays in base and the seller maker in quote.
  The same overflowing fill reports `error = ["notional_overflow", "maker_fee_overflow"]` with an
  exact `taker_fee` and an unavailable `maker_fee`. `tests/replay/fee_overflow_base_orders/` pins
  this shape.

`MatchErrors` is a copyable bitmask internally, one `u8` in the snapshot and on the wire, and a
JSON string array externally. New non-fatal fill conditions can add a code without changing the event
shape. The affected numeric fields stay `0`; a zero is authoritative only when its code is absent.
Use the `MatchResult::exact_notional` / `exact_taker_fee` / `exact_maker_fee`,
`MatchEvent::exact_quote_volume` / `exact_taker_fee` / `exact_maker_fee` and the market-data
`TradeRecord`/`LifecycleRecord` equivalents in Rust, or cross-check `error` in another language.
Nullable fields were rejected because they would force a
snapshot v11/wire v7 migration without adding information beyond the error set; the zero sentinel is
now part of the v11/v7 contract.

Derivatives make overflow easier to reach because notional includes the contract multiplier and the
settlement asset may require a conversion. A fill with an overflow code must not be fed into margin,
PnL or liquidation calculations as if its zero fields were real.

This is a breaking event/wire/snapshot change: snapshot **v11** and wire **v7** carry `MatchErrors`.
The v9/v5 boolean `fee_overflow` is legacy and is not decoded into the new shape.

## 8. Impact on order-book execution

### 8.1 Admission

`OrderBook::execute_order` normalizes before validation and matching, which is why every later step
sees base quantities:

```text
Base volume        -> Q = volume
Quote limit        -> Q = floor_to_base_lot(volume / limit_price); stored as volume_unit = Base
Quote market buy   -> SweepMode::BuyQuote(volume)
Quote market sell  -> SweepMode::SellQuote(volume)
```

Admission rejects a zero normalized quantity, quote volume combined with iceberg fields or `FOK`, a
quote amount with more than `quote_asset.decimals` fractional digits, fee rates outside ±100%, and
every tick/lot/decimal violation. A rejected order consumes no book or fee state.

### 8.2 Sweep loop

The sweep keeps one `Fixed` remainder together with the mode that remainder is in, so the same loop
tracks either a base quantity or a quote budget:

```rust
enum SweepMode {
    Base,      // remainder is a base quantity
    BuyQuote,  // remainder is a gross quote spend cap
    SellQuote, // remainder is a gross quote proceeds target
}
```

The maker side is always base-denominated, so each maker fill computes:

```text
Base:       Q = min(maker.remaining, remaining_base)
BuyQuote:   Q = min(maker.remaining, floor_to_base_lot(remaining_quote / price))
SellQuote:  Q = min(maker.remaining, ceil_to_base_lot(remaining_quote / price))
```

After the gross fill is chosen:

- decrement the maker and the remainder: `Base` by `Q`, either quote mode by `N` (saturating);
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
- Icebergs are base-volume only: a quote order carrying `display_volume`/`hidden_volume` is
  rejected, so no tranche normalization exists.
- The stop cascade uses the same `SweepMode` logic as ordinary orders.
- Fee reporting follows every fill, including stop children and iceberg refills.

### 8.5 Invariants

- No fill consumes more base than the taker requested or more than the maker has.
- A `BuyQuote` order never spends more than its remaining quote budget.
- A `SellQuote` order stops at or just beyond its target; any overshoot is bounded by one base lot.
- All base quantities are whole base lots after normalization.
- Fees never change the matched gross quantity.
- Gross notional and both fees are deterministic from the fill and order rates.
- The engine matches only the submitted base quantity; it does not read, hold or mutate order-system
  balances or freezes. The order system must admit an order whose base quantity fits its freeze; the
  engine has no per-order balance cap (§6.6).
- A rejected order never mutates the book or fee state.
- The book remains uncrossed after every successful execution.

## 9. Data model changes

### 9.1 `Order`

Implemented:

```rust
pub volume_unit: VolumeUnit,           // default Base
pub requested_volume: Fixed,           // client's gross amount; zero means it was `volume`
pub requested_volume_unit: VolumeUnit, // unit of `requested_volume`
pub maker_fee_rate: Fixed,
pub taker_fee_rate: Fixed,
```

Both rates are validated against `MIN_FEE_RATE..=MAX_FEE_RATE` (±100%); anything outside is
`invalid_order`.

For a quote limit order, the book needs the normalized base quantity. Recommended representation:

- Keep the client's gross requested amount and unit for API/audit (`requested_volume`,
  `requested_volume_unit`), and
- Store the normalized base quantity as the matchable `volume` on the resting order.

If the implementation prefers not to extend `Order`, introduce an internal `RestingOrder` wrapper
with `base_remaining` and the original metadata. The book should not compare quote amounts as if
they were base quantities.

The derivatives extension in §3.2 also requires `VolumeUnit::Contract`, the instrument kind,
contract multiplier and settlement asset in `SymbolConfig`, and `FeeAsset::Settlement`. These are
wire and snapshot changes: snapshot v11 and wire v7 cannot carry them without another version and
migration.

Freeze metadata is not part of `Order`: balances, reservations and accounts stay in the order
system, and the engine receives only the base quantity it should match. If a deployment later wants
an in-engine spend cap (for example a `max_quote_spend` field), that must be an explicit order field
with its own wire/snapshot version and matching invariant, not an implicit balance lookup.

### 9.2 `MatchResult`

`MatchResult` is asset-aware: it keeps both order identities, the gross base quantity and quote
notional, both fees with their assets, and the error set.

```rust
pub struct MatchResult {
    pub taker: Order,        // the incoming order that crossed
    pub maker: Order,        // the resting order it matched
    pub matches: Fixed,      // gross base quantity of this fill
    pub notional: Fixed,     // gross quote notional N; zero when it overflowed
    pub taker_fee: Fixed,    // signed: positive charge, negative rebate
    pub taker_fee_asset: FeeAsset,
    pub maker_fee: Fixed,
    pub maker_fee_asset: FeeAsset,
    pub error: MatchErrors,  // ordered set of non-fatal codes (overflow codes today)
}
```

`FeeAsset` names the side of the pair (`Base`/`Quote`); the actual `AssetId` comes from
`SymbolConfig`. Third-asset fee tokens are out of scope: supporting them would require an explicit
`AssetId` plus a fee-price and balance model, both owned by the external order/clearing system.

`error` is a fill-scoped set of codes, not proof that both fees are zero; see §7.1. A base fee can
remain exact when only the quote notional overflows. Downstream consumers must not treat a zeroed
dependent field as real when its code is present.

### 9.3 Events and feed

Each fill event exposes:

- `volume`: gross base quantity, always with `volume_unit = base`;
- `quote_volume`: gross quote notional;
- `taker_fee` with `taker_fee_asset`, and `maker_fee` with `maker_fee_asset`;
- `error` under the fill-level rules in §7.1.

`volume_unit` describes the unit of that event's own `volume`: rests and fills are base, while a
quote-market `Cancelled` event reports the leftover quote with `volume_unit = quote`. The client's
requested unit is not part of an event - it stays on the stored order
(`requested_volume`/`requested_volume_unit`) and is visible through `/read orderbook_orders`. No
event reports a quote overshoot. The feed's `TRADE` and fill `LIFECYCLE` records carry the same
fields, `error` included.

## 10. API examples

Units are lower-case (`base`/`quote`), `live` and `id` and `price` have no serde defaults, so even a
market order must carry `"price": "0"`.

### Base volume

```json
{"Place": {"order": {
  "live": "Limit", "id": 1, "sequence": 0,
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "base",
  "price": "100.25",
  "volume": "4.4",
  "maker_fee_rate": "0.001",
  "taker_fee_rate": "0.002"
}}}
```

### Quote volume, limit

```json
{"Place": {"order": {
  "live": "Limit", "id": 2, "sequence": 0,
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "quote",
  "price": "100.25",
  "volume": "1000",
  "maker_fee_rate": "0.001",
  "taker_fee_rate": "0.002"
}}}
```

This normalizes to `floor_to_base_lot(1000 / 100.25)` base units at admission; the order rests with
`volume_unit` `base`, and keeps the requested quote amount in `requested_volume`/
`requested_volume_unit`.

### Quote volume, market buy

```json
{"Place": {"order": {
  "live": "Market", "id": 3, "sequence": 0,
  "symbol_id": 0,
  "side": "Buy",
  "volume_unit": "quote",
  "price": "0",
  "volume": "500",
  "taker_fee_rate": "0.002"
}}}
```

This spends at most 500 quote units while sweeping. The budget is gross: a quote-asset fee is
debited on top of it. The leftover, bounded by the quote cost of one base lot, is cancelled as a
`Cancelled` event whose `volume` is the quote amount and whose `volume_unit` is `quote`.

## 11. Versioning and migration

This was implemented as a breaking storage/wire change:

- `Order` gained `volume_unit` and normalized/requested volume metadata.
- `SymbolConfig` gained base/quote asset metadata and the fee policy.
- `MatchEvent`/`MatchResult` gained quote notional and fee-asset fields; the boolean
  `fee_overflow` was replaced by the ordered `error`/`MatchErrors` code set.
- Snapshot version is **v11**.
- Market-data wire format is **v7**.
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

Covered today by unit tests, the cluster tests and the replay matrix:

- Base buy/sell with fee in the received asset, in quote and in base.
- Maker rebate in each fee asset.
- Quote limit buy/sell normalization, zero-lot rejection and base-lot rounding.
- Quote market buy spend cap, leftover quote dust and fee in base vs quote.
- Quote market sell proceeds target and final-lot overshoot policy.
- Partial fills and multiple makers; a taker that later rests, then fills as maker.
- Iceberg refills and stop children in base volume (quote icebergs and quote FOK are rejected).
- Amendments preserve rates; the amended volume is always base.
- 18-digit exactness, snapshot restart and wire round-trip.
- Replay fixtures for quote-policy overflow (`fee_overflow_orders`) and a `Received` fill where
  only the quote maker fee is unavailable (`fee_overflow_base_orders`).
- Determinism across replicas and failover.
- No negative/over-budget remainders; no crossed book.
- Fee rates outside ±100%, and quote amounts with too many decimals, are rejected.

Still to cover if the extensions in §3.2 and §13 items 11-13 are built:

- Derivatives: linear USDT-margined fees in quote, inverse fees in base, third-asset settlement,
  contract-multiplier notional, maker rebates and overflow quarantine; margin/PnL and funding stay
  outside the engine.

Order-system freeze and clearing cases (outside this repository, not part of `cargo test`):

- `Received` buy: quote principal is frozen, the base fee is withheld from the base delivery and the
  unused quote is released. `Received` sell: base principal is frozen, the quote fee is withheld
  from proceeds and the unused base is released.
- `Quote` buy: the freeze covers `N_max` plus the quote fee; a partial fill and a cancel release the
  remainder exactly once. `Quote` sell: base principal is frozen and the quote fee is deducted from
  proceeds per fill.
- `Base` with base volume: the estimator produces `Q_max`; an order for `Q > Q_max` is rejected or
  explicitly resized before submission; an accepted `Q` matches in full even when the realized
  notional plus fee would exceed the quote freeze, which must raise an order-system risk event.
- `Base` buy and sell: the quote collateral reserve for the base fee is released once clearing has
  funded or withheld the base fee, and no fee is charged twice.
- Partial fills, amendments, cancellation, GTD/Day expiry and a stopped child: each terminal state
  releases only the unused freeze, and a fee is deducted once per fill.
- A fill with `notional_overflow`, `maker_fee_overflow` or `taker_fee_overflow` is quarantined by
  clearing rather than settled from a zero amount.
- Derivatives: margin plus fee plus buffer is frozen in the settlement asset at risk-set leverage;
  the fee is deducted from the freeze on every fill; PnL, funding and liquidation adjust the
  remaining margin outside the engine.

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
10. Fee rates must be within ±100% (`MIN_FEE_RATE..=MAX_FEE_RATE`); anything farther is
   `invalid_order`.

Still open:

11. Which asset classes a deployment must enable: fiat, stablecoin, crypto, equity, ETF or a
    subset? `AssetId`/`AssetClass` cover all five; asset issuance and validation are a deployment
    concern.
12. Derivatives support requires an instrument kind, contract multiplier, settlement/margin asset,
    `VolumeUnit::Contract` and `FeeAsset::Settlement`; the fee formula depends on whether the
    contract is linear, inverse, quanto or third-asset margined. This is a snapshot/wire-version
    change, not an incremental `FeeAssetPolicy` value.
13. For third-asset margin, decide whether fees are charged directly in settlement units or
    converted from the contract's quote currency at an explicit, versioned rate. The engine must not
    infer an FX rate.

Resolved:

14. **Numeric sentinel policy.** Keep zero plus `MatchErrors` for snapshot v11 and wire v7. A zero
    is authoritative only when its code is absent; `MatchResult` and `MatchEvent` expose
    `exact_notional`/`exact_quote_volume`, `exact_taker_fee` and `exact_maker_fee` accessors, and
    market-data records expose the same helpers. Nullable fields were rejected because they would
    require a v11/v7 migration without adding information beyond the error set.
15. **Mixed-unit iceberg/FOK.** Quote volume remains limited to plain limit and market orders.
    Quote icebergs and FOK are deliberately rejected at admission; clients must submit base volume
    for those shapes. There is no planned base-normalization rule.
16. **Fee-token scope.** Third-asset fee tokens remain out of scope. The engine supports only the
    pair's base/quote fee assets; conversion, fee-token balances and freeze/deduction are owned by
    the external order/clearing system, which must supply any explicit conversion policy.
17. Fixed per-trade fees, per-share commissions, regulatory fees, minimum tickets and taxes are
    owned by the external order system; the engine computes only notional-proportional maker/taker
    rates.
18. Equity/ETF sessions, holidays, auctions, T+1/T+2 settlement and corporate actions are owned by
    the external order system; the engine consumes explicit timestamps and has no clock or
    corporate-action logic.
19. Funding, liquidation, insurance funds, auto-deleveraging, initial/maintenance margin, leverage
    limits, portfolio netting, option exercise and realised/unrealised PnL are owned by the
    risk/clearing layer. The matching engine reports fills and fees only.
20. Balance checks, asset freezes and freeze release are order-system state. The engine never holds
    balances or enforces a per-order spend cap; it matches the base quantity it receives (§6.6).
21. Under `Received`, the order system freezes quote for a buy and base for a sell; the fee is taken
    from the received asset. Under `Quote`, both sides settle the fee in quote; a sell still freezes
    the base principal. Under `Base`, the order system freezes quote collateral for both sides and
    the fee is charged in base (§6.6.1).
22. Base-volume orders under `Base` use the conservative estimate
    `F_quote = ceil_to_quote_lot(Q * (P_ref + fee_quote_per_base) + buffer)`, with `P_ref` replaced
    by `P_ref / leverage` for a derivative. The order system accepts `Q <= Q_max` and rejects or
    explicitly resizes the rest; the engine matches the submitted base volume and has no visibility
    of `F_quote` (§6.6.2).
23. Derivative freezes are `margin + fee_est + maintenance_buffer` at the risk-set leverage, in the
    contract's settlement/margin asset. Leverage, margin rates and liquidation remain risk-layer
    inputs (§6.6.3).
24. Clearing deducts each fill's positive fee from the frozen asset or the settlement leg before
    crediting proceeds, and releases the unused freeze on cancel, expiry or terminal fill. A fill
    must never create an unfunded negative balance; if it would, clearing rejects or quarantines it
    (§6.6.4).

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
- The engine reports exact fee amounts and fee assets; the external order/clearing system owns
  balances, fiat/chain settlement and final asset precision.
- The order system freezes the trading asset before submission and clearing deducts fees from that
  freeze or the settlement leg, then releases the unused amount. `Received` freezes quote for buys
  and base for sells; `Quote` freezes the quote fee and, on sells, the base principal; `Base`
  freezes quote collateral for both sides.
- A base-volume order under `Base` is converted by the order system with a conservative price, fee
  reserve and buffer, rounded up, and the submitted base quantity is capped at the resulting
  `Q_max`. The engine matches that base quantity exactly and must not be assumed to stop at the
  freeze, because it does not see balances.
- A derivative freeze covers risk-set initial margin, the estimated fee and a maintenance buffer in
  the settlement/margin asset. The engine reports the fee asset and amount but computes neither
  margin nor leverage.
- The engine stores no freeze or balance state. If in-engine spend enforcement is ever required,
  add an explicit versioned order field (for example `max_quote_spend`) rather than an implicit
  balance lookup.
- Equity/ETF sessions, holidays, auctions, T+1/T+2 settlement, corporate actions and
  fixed/regulatory/minimum/tax fees stay upstream in the external order system; the engine consumes
  explicit session-close/GTD timestamps and notional-proportional maker/taker rates.
- Keep the implemented cash/spot contract as-is. Before derivatives, add instrument kind, contract
  multiplier, settlement/margin asset, `VolumeUnit::Contract` and `FeeAsset::Settlement`; do not
  reinterpret `Quote` as “margin”.
- Derivatives funding, liquidation, margin and PnL stay upstream; the engine matches orders and
  reports gross fills, fees and fee assets. A fill with an overflow error code must be quarantined
  rather than treated as zero fee/notional by the risk layer.
- Snapshot v11 and wire v7 gate the cash/spot change; a derivatives extension needs a new
  snapshot/wire version.

This keeps the order book’s price-time priority unchanged while extending the execution path with
two explicit quote-aware sweep modes (spend-capped buys, proceeds-targeted sells) and asset-aware
fee reporting.
