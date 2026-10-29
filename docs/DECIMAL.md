# Decimal and fixed-point options for price and volume

Status: **implemented end-to-end.** `Order`, `OrderBook.last_trade_price`, match events,
depth/BBO/spread, `/read`, the market-data feed, snapshot v9 and wire v5 use 18-scale `Fixed`
integers. JSON emits decimal strings and accepts legacy numeric input. One process/cluster trades
exactly one symbol, whose replicated `SymbolConfig` is enforced and locked after the first order;
multi-symbol isolation is deployment-level (one process per symbol). This document keeps the
comparison and benchmarks that led to the integer representation.

## Recommendation

1. **Use integer ticks/lots in the matching hot path.** Define per-instrument price ticks and
   quantity lots, exchange decimal strings at the API boundary, and keep `price`/`volume` as
   `i64`/`u64` where the scale permits, or `i128` for 18-decimal crypto. This is the fastest,
   smallest and most deterministic representation: comparisons are integer comparisons, and
   there is no rounding in the matcher.
2. **If a decimal library is needed as an interim step, use `rust_decimal` only after checking
   the range.** It is mature, 16 bytes, `Copy`, allocation-free and supports 28–29 significant
   digits. It covers 18-decimal BTC prices and individual order quantities, but **not** an
   18-decimal notional such as 21M BTC × $100k (31 significant digits).
3. **Use `fastnum` when 28 digits are not enough** (for example 18-decimal crypto amounts with
   large notionals). `D128`/`D256` are fixed-size, `Copy`, allocation-free and offer 38/76 digits,
   at a modest arithmetic cost. Check the MSRV and API stability of the exact version.
4. **Do not use `bigdecimal` or another arbitrary-precision type in the matching loop.** They are
   appropriate for settlement, reporting and import/export, where allocation and variable-size
   arithmetic are acceptable.
5. **Do not parse money through `f64`.** Parse decimal strings or an integer representation at the
   boundary and reject values with more precision than the instrument allows.

## BTC and 18-decimal values

Native Bitcoin has 8 decimal places (1 satoshi = 1e-8 BTC). An 18-decimal asset is normally a
wrapped/ERC-20-style representation or an exchange-internal fixed-point scale. The following
assumes the stricter case: **18 fractional digits for price, quantity and notional**.

Representative values:

| Value | Example | Significant digits |
|---|---|---:|
| BTC price | `123456.123456789012345678` | 24 |
| BTC supply/quantity | `21000000.123456789012345678` | 26 |
| Notional (21M BTC × $100k) | `2100000000000.000000000000000000` | 31 |

At scale 18, a type needs at least 31 significant digits to hold the notional exactly.

| Type | 18-decimal BTC price | 18-decimal quantity | 18-decimal notional | Verdict |
|---|---|---|---|---|
| `i128` ticks (scale 18) | yes (20 integer digits) | yes | yes as a stored value; multiply via `i256` | **recommended** |
| `rust_decimal` (28 digits) | yes (max ~7.9e10 at scale 18) | yes for 21M BTC | **no** (needs 31 digits; rounds/loses scale) | order fields only |
| `fastnum D128` (38 digits) | yes | yes | yes (20 integer digits at scale 18) | **best decimal library** |
| `fastnum D256` (76 digits) | yes | yes | yes | safe, overkill for BTC |
| `fastnum D64` (~19 digits) | no (only ~1 integer digit at scale 18) | no | no | unsuitable |
| `decimal` d128 (34 digits) | yes | yes | yes (16 integer digits at scale 18) | works, but FFI/C |
| `bigdecimal` | yes | yes | yes | settlement only |
| `f64` | approximate | approximate | approximate | reject |

Important consequences:

- A `D64` or `i64` cannot represent 18 fractional digits plus a BTC price: `i64` at scale 18 has
  only ~9.22 integer units of range.
- `rust_decimal` can represent the price and an individual quantity, but its 96-bit coefficient
  caps an 18-scale value at about `7.9e10`; a 31-digit notional is rounded to 28 significant digits
  and loses trailing fractional precision. If only the matcher (never notional) uses it, it can be
  an interim choice; do not use it for settlement values at 18 decimals.
- `fastnum D128` is the smallest fixed-size decimal that comfortably covers all three BTC values.
  It is 24 bytes, `Copy`, allocation-free, and has 20 integer digits at scale 18.
- With `i128` ticks, the notional multiplication needs a wider intermediate (`i256`/`U256`) or
  careful pre-scaling; `i128 × i128` cannot hold the full 36-decimal product.

## Candidates

### 1. Integer ticks/lots (custom newtypes)

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PriceTicks(i64); // scale is a per-instrument property

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuantityLots(u64);

pub struct Order {
    pub price: PriceTicks,
    pub volume: QuantityLots,
    // ...
}
```

- **Representation:** ordinary integers; scale is metadata, not part of every value.
- **Range:** `i64` has ~9.22e18 ticks. At 8 decimal places that is ~9.2e10 units; at 18 decimal
  places it is only ~9.22 units, so 18-decimal BTC needs `i128` (16 bytes).
- **Properties:** `Copy`, no allocation, no rounding, exact equality and ordering, trivial to
  serialize in a fixed binary format.
- **Hot path:** fastest option; integer add/compare is in the sub-nanosecond range.
- **Cost:** every instrument needs a tick size and lot size, and the API needs conversion code.
  This is normal exchange design, not a drawback.
- **Fit here:** best for the matching engine and market-data path.

### 2. `rust_decimal` 1.43

- **Representation:** 96-bit coefficient, sign and scale; 16 bytes.
- **Precision/range:** 28–29 significant decimal digits, scale 0–28; max value ≈ 7.9e28,
  minimum non-zero 1e-28.
- **Properties:** `Copy`, no heap allocation, mature, wide ecosystem support, serde and macros
  (`rust_decimal_macros`); MSRV 1.67.
- **Hot path:** close to integer performance; in local tests it was faster than `fastnum` for
  small values.
- **Limits:** 28 digits is enough for typical equities/FX and for BTC prices/individual 18-decimal
  quantities, but not for an 18-decimal BTC notional (31 digits) or arbitrary crypto amounts.
  Replacing five `f64` fields with 16-byte decimals grows `Order` by roughly 40 bytes.
- **Fit here:** good interim replacement for price/quantity fields; not the final design if integer
  ticks are feasible, and not sufficient for BTC notional at 18 decimals.

### 3. `fastnum` 0.7.x

- **Representation:** fixed N-bit coefficient plus a 64-bit control block (scale, sign, flags,
  context); `D64` 16 bytes, `D128` 24 bytes, `D256` 40 bytes.
- **Precision/range:** roughly 19 digits for `D64`, 38 for `D128`, 76 for `D256`; IEEE-style
  special values, rounding and signal flags.
- **Properties:** pure Rust, `Copy`, no allocation, `no_std`, serde support, const parsing and
  compile-time macros.
- **Hot path:** fixed-size arithmetic; in local tests roughly 2–4x slower than `rust_decimal` for
  small add/multiply/compare, still far faster than heap-based arbitrary precision.
- **Version note:** `fastnum 0.7.5` requires Rust 1.94; `0.7.4` supports Rust 1.88. The project's
  current MSRV is 1.89, so pin `=0.7.4` unless the MSRV is raised.
- **Fit here:** best fixed-size option when 28 digits are insufficient.

### 4. `bigdecimal` 0.4.x

- **Representation:** `BigInt` coefficient plus `i64` scale; 40 bytes for the handle before its
  heap buffer.
- **Precision/range:** arbitrary digits and scale.
- **Properties:** mature, serde support, useful for parsing/printing and calculations whose
  precision is not known ahead of time.
- **Hot path:** heap allocation and variable-size arithmetic. In local tests small add/compare
  was around 10–22 ns/op; wide values and repeated scaling/division are worse. It is not `Copy`,
  which would also break the matcher's current `Order: Copy` assumption.
- **Fit here:** settlement, reports, imports; **not** order matching or per-fill paths.

### 5. `decimal` 2.1 (IEEE 754 decimal128 via decNumber)

- **Representation:** IEEE 754 decimal128 (`d128`), 34 significant digits; wraps the C decNumber
  implementation through FFI.
- **Properties:** strict decimal128 semantics, fixed size, broad rounding/context behavior.
- **Hot path:** FFI and a C build; not a good fit for a pure-Rust matching engine, and the extra
  semantics are mostly unnecessary for price/quantity arithmetic.
- **Fit here:** avoid unless an external contract explicitly requires IEEE decimal128.

### 6. `fixed` / compile-time fixed-point

- **Representation:** integer with a compile-time number of fractional bits (for example
  `I64F64`); fixed scale, no decimal exponent.
- **Properties:** `Copy`, no allocation, near-integer speed, good embedded story.
- **Limits:** one scale per type; converting between instruments/scales is explicit; 2.x is still
  alpha. Integer ticks with per-instrument metadata are simpler for this codebase.
- **Fit here:** viable alternative to ticks if a single global scale is acceptable.

### 7. `bnum`, `ruint`, `i256` (fixed-size big integers)

- **Representation:** fixed-size 128/256/512-bit integers, not decimals.
- **Properties:** `Copy`, fast big-integer arithmetic; pair with an explicit scale for crypto or
  large notional values.
- **Fit here:** useful implementation building blocks for a custom tick type, not a complete
  decimal API.

### 8. `f64` (former)

- **Representation:** binary floating point.
- **Properties:** fast and compact, but cannot represent `0.1` exactly, has non-associative
  arithmetic, and makes exact equality/rounding unsafe. Not suitable for money.
- **Fit here:** remove from the matching, event, snapshot and wire paths.

## Indicative microbenchmark

Environment: Apple M4, Rust 1.93.1, release build with LTO, `std::hint::black_box`, same operands
for every iteration. Results are ns/op and are **not** an end-to-end matching benchmark; they only
show the relative cost of the arithmetic primitives.

Small operands (`12345.6789` and `0.0001`; 20M iterations, 300k for `bigdecimal`):

| Operation | `i128` ticks | `rust_decimal` | `fastnum D128` | `fastnum D256` | `bigdecimal` |
|---|---:|---:|---:|---:|---:|
| add | 1.11 | 1.65 | 3.51 | 4.86 | 10.04 |
| multiply (with rescale for ticks) | 1.80 | 1.47 | 3.97 | 6.24 | 5.80 |
| compare | 0.43 | 2.26 | 2.98 | 5.32 | 3.49 |

Larger operands (28 digits for `rust_decimal`, 38 for `D128`, 60 for `D256`/`bigdecimal`):

| Operation | `rust_decimal` 28d | `fastnum D128` 38d | `fastnum D256` 60d | `bigdecimal` 60d |
|---|---:|---:|---:|---:|
| add | 3.42 | 5.21 | 5.43 | 21.79 |
| compare | 2.36 | 2.89 | 4.60 | 16.05 |

`i128` ticks and `rust_decimal` are effectively in the same class for add/multiply/compare.
`fastnum` costs a little more because of its control block and decimal semantics. `bigdecimal`
becomes clearly more expensive as values widen; its small-value numbers also include cloning and
allocator effects. None of these measurements include parsing, formatting, serde or Raft.

BTC-like 18-decimal operands (`123456.123456789012345678` and
`0.000000000000000001`; 20M iterations, 300k for `bigdecimal`):

| Operation | `i128` ticks | `rust_decimal` | `fastnum D128` | `fastnum D256` | `bigdecimal` |
|---|---:|---:|---:|---:|---:|
| add | 0.91 | 1.99 | 3.46 | 4.74 | 19.85 |
| compare | 0.42 | 2.27 | 2.97 | 4.78 | 14.81 |

The arithmetic ranking is unchanged; the decisive BTC issue is range, not primitive cost.

## Implementation status

Implemented in `src/order_book/fixed.rs`, `Order` and the state machine:

- `price`, `volume`, `stop_price`, `display_volume` and `hidden_volume` use a common-scale
  `Fixed(i128)` with 18 fractional digits.
- `OrderBook.last_trade_price` is `Option<Fixed>`, so stops are elected by the exact print and the
  value survives snapshots without an `f64` round trip.
- `MatchEvent.price/volume` and `StoredMatchEvent` are `Fixed`; `/events`, `/write` events and the
  JSONL file sink emit decimal strings.
- `DepthLevel`, `DepthChange`, `spread`, `mid_price`, `/read` depth/BBO/spread/last-trade and the
  market-data records are `Fixed`. The shadow book is keyed by `Fixed`, not by `f64` bits.
- Market-data wire **v5** encodes every price, quantity, quote volume and fee as a little-endian
  scale-18 `i128` (16 bytes), plus the fee asset per side; older frames are not silently
  reinterpreted.
- Snapshot **v9** stores the last trade price, match events, base/quote asset metadata, tick/lot
  rules, maker/taker fee rates, fee assets and fill fees, plus the single-symbol precision
  configuration. Older snapshots are rejected with a clear error.
- `Fixed::notional(price, quantity)` multiplies through a 256-bit intermediate and truncates toward
  zero at 18 places. `Fixed * Fixed` uses the same checked path; the matching loop itself does not
  multiply price by quantity.
- `Order.maker_fee_rate`/`taker_fee_rate` are signed `Fixed` fractions. Fills charge the taker rate
  and the maker rate on exact notional; `fee_overflow` marks values outside the scale-18 range.
- JSON emits decimal strings for all `Fixed` values, so 18-digit values round-trip exactly. Legacy
  numeric JSON payloads are still accepted on input.
- `Order.symbol_id` must match the process/cluster's configured symbol; `SymbolConfig` declares
  the accepted price/quantity decimals (0..=18). Values are normalized to the common 18-digit
  scale, so a 2-decimal process and an 18-decimal process share the same fast code path.

Remaining exactness work:

- `SymbolConfig` is replicated and enforced for the one symbol a process/cluster trades; orders
  with a different `symbol_id` or excess precision are rejected. The configuration locks after the
  first accepted order. Multi-symbol scaling is deployment-level: run a separate process/cluster
  per symbol so resources and failures stay isolated.
- `to_f64` and `From<f64>` remain for legacy JSON input and test assertions. No public financial
  output or replicated financial field passes through `f64`.
- Notional settlement and fee rounding policies still need product decisions; the arithmetic
  primitive is in place.

Recommended next steps:

1. Document the one-process-per-symbol deployment contract and add gateway routing/isolation.
2. Define the notional/settlement rounding policy and use `Fixed::notional` or `i256` directly.
3. Benchmark the matching loop and feed before/after; keep the "matching must not regress" gate.
4. Record the wire v5/snapshot v9 compatibility policy in the deployment notes.

## Decision summary

| Requirement | Best choice |
|---|---|
| Maximum matching performance, exact exchange semantics | 18-scale `Fixed(i128)` (implemented end-to-end) |
| BTC 18-decimal price/quantity/notional, no allocation | `fastnum D128` |
| Minimal change from `f64`, 28 digits, proven range | `rust_decimal` |
| Fixed-size, 38–76 digits, no allocation | `fastnum D128/D256` |
| Arbitrary precision, reporting/settlement | `bigdecimal` |
| IEEE 754 decimal128 contract | `decimal` (decNumber), only if required |
| Global compile-time fixed scale | `fixed` or custom integer newtypes |

The former `f64` representation has been removed from replicated state and public financial outputs;
legacy numeric JSON input is still accepted for compatibility. One process/cluster enforces one
symbol's precision; multi-symbol isolation is deployment-level, not a second book in the same
process.
