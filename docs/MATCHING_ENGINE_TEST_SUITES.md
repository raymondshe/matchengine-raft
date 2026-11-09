# Public matching-engine test suites: inventory and port backlog

Date: 2026-11-09 · Baseline: `417e095` (sustained CPU profiling in the e2e benchmark)

This document records the public matching-engine test suites reviewed for this
project, what they actually cover, which behaviours are worth porting, and the
first conformance batch added from that review. It is an inventory, not a
compatibility claim: the engine implements the behaviours listed in
[`AGENTS.md`](../AGENTS.md), not every behaviour found upstream.

The short version:

- No public suite covers every order type this engine supports, and no public
  venue-grade conformance suite is available. CME's order-type cases live behind
  AutoCert+; SGX publishes a FIX scope document rather than executable cases.
- The most useful material is behavioural: FIFO oracles, adversarial FOK cases,
  iceberg replenishment, stop cascades, event-stream reconstruction and exact
  fee arithmetic.
- Licences are compatible for reading and re-implementing behaviour, but code
  must not be copied from LGPL or GPL-gated sources. NautilusTrader is reference
  only. New tests in this repository are original.

## 1. Inventory

Test counts are approximate counts of test functions/cases visible in each
repository on 2026-11-09. They are useful for judging breadth, not for comparing
quality. "Harness" means the project is primarily a benchmark or differential
tester rather than an order-book implementation with its own expected outcomes.

| Project | Stack | Cases | Licence | Strongest material |
|---|---|---:|---|---|
| [`flash1-dev/matching-engine-benchmark`](https://github.com/flash1-dev/matching-engine-benchmark) | harness | small | MIT | Differential FIFO oracle across engines; catches phantom or zero-quantity fills |
| [`joaquinbejar/OrderBook-rs`](https://github.com/joaquinbejar/OrderBook-rs) | Rust | 2,069 | MIT | Broadest Rust unit suite: price-time priority, modifications, iceberg refill, depth |
| [`intrepidkarthi/orderbook`](https://github.com/intrepidkarthi/orderbook) | not recorded | 260 | MIT | Adversarial FOK cases and event reconstruction after mixed flows |
| [`enewhuis/liquibook`](https://github.com/enewhuis/liquibook) | C++ | 157 | BSD-3-Clause | All-or-none (AON) semantics and a mature order-type state machine |
| [`ArjunVachhani/order-matcher`](https://github.com/ArjunVachhani/order-matcher) | not recorded | ~199 | MIT | Combinatorial order-type matrix (limit/market/IOC/FOK/stop variants) |
| [`lvv123/lob-engine`](https://github.com/lvv123/lob-engine) | not recorded | 41 | MIT | Sequence discipline and stop-price precision |
| [`nautechsystems/nautilus_trader`](https://github.com/nautechsystems/nautilus_trader) | Rust/Python | 116 matching cases | LGPL-3.0 | Venue-shaped order lifecycle and event model; reference only |
| [`openbook-dex/openbook-v2`](https://github.com/openbook-dex/openbook-v2) | Rust/TypeScript | 44 | MIT (GPL-gated parts) | On-chain order book, expiry and self-trade handling |
| [`exchange-core/exchange-core`](https://github.com/exchange-core/exchange-core) | Java | 36 unit + 7 integration + 4 Cucumber | Apache-2.0 | Order modification, risk, fees and event journals at scale |

Notes on individual suites:

- **FlashOne benchmark.** It covers only limit/new/cancel/modify/IOC. It has no
  FOK, stop, iceberg, post-only or GTD cases. It lists this repository as
  conforming "with fix": a phantom zero-quantity fill was found at pinned commit
  `5597532` and fixed in `a44d9ae`. The regression cases live in
  `src/order_book/tests.rs`. The benchmark is still the best cheap external
  check of basic FIFO behaviour, but it is not a conformance suite.
- **OrderBook-rs.** The largest suite reviewed. It is valuable as a source of
  edge cases for amendments, queue priority and iceberg replenishment. Its API
  and data model differ substantially, so cases must be translated rather than
  pasted.
- **intrepidkarthi/orderbook.** The most directly useful adversarial suite for
  FOK atomicity and for rebuilding book state from events. It tests sequences
  and visible state after mixed accept/reject flows.
- **Liquibook.** The only reviewed suite with mature AON behaviour. AON is not
  implemented here, so only its state-machine and expiry ideas are relevant.
- **order-matcher.** Good at enumerating combinations, but its assertions are
  tied to a different order model and a different notion of sequence identity.
- **lob-engine.** Small but precise about sequences and stop prices. Useful for
  checking that a rejected request still consumes exactly one sequence.
- **NautilusTrader.** A production-grade venue model, but LGPL-3.0. Use it to
  understand lifecycles, never as a source of copied code.
- **OpenBook v2.** Interesting for expiry and on-chain constraints. Most of the
  repository is MIT, but the Solana program includes GPL-gated parts behind the
  `enable-gpl` feature; do not copy those parts.
- **exchange-core.** Strong on order modification, fee arithmetic and event
  journals. Its FOK is "FOK budget" rather than the simple all-or-nothing
  quantity rule used here.

## 2. Coverage matrix

`●` = explicit cases, `○` = partial or indirect coverage, blank = not covered.
This is a qualitative map from test names and READMEs, not an exhaustive audit.

| Behaviour | FlashOne | OrderBook-rs | intrepidkarthi | Liquibook | order-matcher | lob-engine | Nautilus | OpenBook v2 | exchange-core |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Limit / GTC | ● | ● | ● | ● | ● | ● | ● | ● | ● |
| Market | ● | ● | ● | ● | ● | ● | ● | ○ | ● |
| IOC | ● | ● | ● | ● | ● | ○ | ● | ○ | ● |
| FOK |  | ○ | ● | ○ | ● |  | ○ |  | ● |
| GTD / expiry |  | ○ | ○ | ● | ○ |  | ● | ● | ○ |
| Day |  | ○ |  |  | ○ |  | ○ |  |  |
| Post-only |  | ○ |  |  | ○ |  | ○ |  |  |
| Stop market / stop limit |  |  |  | ○ | ● | ● | ○ |  |  |
| Iceberg / reserve |  | ● |  |  | ○ |  | ○ |  |  |
| Amend / modify priority |  | ● | ○ | ● | ● |  | ● | ○ | ● |
| Cancel | ● | ● | ● | ● | ● | ● | ● | ● | ● |
| Maker/taker fees |  | ○ |  |  |  |  | ● | ● | ● |
| Event-stream reconstruction |  |  | ● |  |  |  | ● | ○ | ● |
| Sequence / cursor discipline |  | ○ | ● | ○ | ○ | ● | ○ | ○ | ● |
| Replicated / failover invariants |  |  |  |  |  |  | ○ |  |  |

What the matrix shows: breadth is spread across projects, and the intersection
of "all order types this engine supports" plus "event reconstruction" plus
"replicated sequence invariants" is empty. That intersection is what this
repository's own cluster and replay suites are for.

## 3. What this repository covers today

Before the batch in section 5:

| Layer | Count | What it proves |
|---|---:|---|
| `src/` unit tests | 167 | Order-book algorithms, fees, property traces, protocol/storage units |
| `tests/cluster/` integration tests | 61 | Raft, HTTP/gRPC, persistence, events, failover, market data |
| `tests/replay/` cases | 16 | End-to-end order-type lifecycles against checked-in expected events |
| `src/order_book/conformance_tests.rs` | 5 | Explicit edge cases the randomized suite should not have to rediscover |

The replay matrix already covers limit, market, IOC, FOK, GTD, Day, post-only,
iceberg, stop-market, stop-limit, amend, cancel, default fees, quote-asset fees
and fee overflow. The randomized property tests cover mixed order types and fee
policies. The gap was not raw type coverage; it was explicit invariants that are
easy to miss when a type works "normally".

Existing conformance cases already cover: market FOK all-or-nothing, empty-book
market cancellation, hidden iceberg volume on cancel/expiry, GTD/Day stop
expiry, and fees on market/IOC/stop-child fills.

## 4. What no public suite covers

These are venue-policy behaviours rather than matching-core behaviour, and the
engine either does not implement them or deliberately leaves them to the order
system:

- AON, stop-trailing, pegged, OCO/OTO/OUO, market-to-limit, reserve orders,
  reduce-only, price bands/collars, pro-rata allocation, auctions/uncrossing,
  sessions, multi-book routing and risk/margin.
- Venue-specific self-trade prevention (STP), mass cancel, order masks and
  regulatory fee rules.
- Cross-node determinism under snapshot install, log truncation and failover.
  This repository covers that in `tests/cluster/`, not in order-book unit tests.

A public suite being silent on these is not a defect in the suite. It is a
reminder not to treat a matching-engine test suite as an exchange conformance
program.

## 5. First conformance batch added

The batch deliberately targets invariants, not new order types. Each case is
small enough to diagnose from its name and asserts conservation or atomicity
rather than only the happy-path fill list.

### `src/order_book/conformance_tests.rs`

1. **`rejected_fok_is_atomic_but_consumes_one_sequence`**
   - Snapshots asks, bids, parked stops and last trade.
   - Submits an unfillable FOK and asserts it is rejected with no fills, rest,
     cancel, trigger or book/last-trade mutation.
   - Asserts `book.sequence` advanced by exactly one, pinning the engine rule
     that even a rejected submission consumes one replicated sequence.

2. **`iceberg_partial_fill_and_cancel_conserve_volume`**
   - Rests an iceberg with 4 visible and 6 hidden.
   - Fills 1 and asserts the visible tranche shrinks to 3 while the reserve stays
     at 6.
   - Cancels and asserts the removed order still owns the fill plus the cancelled
     quantity: `1 filled + 9 removed = 10 submitted`.

3. **`iceberg_refill_keeps_identity_and_queue_priority`**
   - Queues two icebergs at one price: A (4 visible / 6 hidden) then B (4
     visible).
   - A taker for 6 must match A twice, `[4, 2]`, refilling A from its reserve
     between trades and never touching B.
   - A second taker for 4 must finish A, `[2, 2]`, before B can trade. All four
     fills keep A's original sequence.
   - Asserts B is untouched with 4 visible and A's total 10 is fully accounted
     for.

4. **`stop_cascade_conserves_each_child_and_never_rests_one`**
   - Rests bids at 100/90/80 and parks a sell stop-limit at 80 (trigger 100,
     6 units) plus a sell stop-market at 90 (10 units).
   - A trade at 100 elects the first stop; its child fills 3 at 90 and 2 at 80,
     then cancels 1. The print at 80 elects the second stop, whose child finds an
     empty book and cancels all 10.
   - Asserts trigger order, trigger prices, fill prices, and
     `fills + cancelled == submitted` for each stop child.
   - Asserts no child rests, both stops leave the parked set, and the book ends
     empty.

### `tests/cluster/test_feature_matrix.rs`

5. **`event_stream_reconstructs_visible_book`** (port 21934)
   - Drives limit, IOC, FOK-reject, market and stop-cascade flows through
     `/write` on a single-node cluster.
   - Reads the replicated log back through `POST /events` in small pages,
     checking cursor monotonicity and that no event is duplicated or lost across
     pages.
   - Replays `Rested` / `Fill` / `Cancelled` / `Rejected` / `Triggered` events
     into a mirror of visible resting orders and compares it with
     `/read orderbook_orders`, plus the final sequence and empty stop set.
   - Asserts the rejected FOK contributed a `Rejected` event but no state
     mutation.

Known limits of event reconstruction, recorded by the test:

- A parked stop emits **no event** until it is triggered. The event stream can
  reconstruct the visible book, not the parked-stop set.
- An iceberg `Rested` event carries only the visible tranche. A refill emits
  further `Fill` events, but the hidden reserve is not exposed. Full L3
  reconstruction including hidden volume is therefore impossible from
  `MatchEvent` alone.
- The test restricts its mirror to plain, visible orders for those reasons.

## 6. Port backlog

Prioritized by invariant value divided by translation cost.

### P1 — next batch

- **Post-only and amend interactions.** A same-price reduction keeps sequence;
  a size increase or price move loses it. Assert the event's
  `previous_sequence`/`order_sequence` pair and queue order for both outcomes.
- **GTD boundary and purge idempotence.** Expiry exactly at the deadline, one
  millisecond before, repeated purges, and an expiry event's sequence anchoring.
- **Iceberg plus FOK.** FOK preflight counts hidden reserve as reachable depth;
  an accepted FOK consumes tranches and leaves the correct visible/reserve
  split.
- **Stop-limit boundary.** Child fills exactly at its limit, does not fill one
  tick away, and cancels the remainder without resting it.
- **Fee arithmetic.** Maker rebates, both fee assets, and overflow cases where
  one fee is exact and the other is not (the replay matrix has cases; unit-level
  invariants are still thin).
- **Event pagination under retention.** Eviction, cursor restart, and a
  snapshot/restart replay that proves event IDs remain stable.

### P2 — useful but larger

- **Modify-priority matrix** translated from exchange-core/OrderBook-rs:
  price up/down, size up/down, and the resulting queue position.
- **AON semantics** from Liquibook, only if AON is added to the engine.
- **Self-trade prevention** and order masks, only if the product requires them.
- **Multi-symbol isolation** at the deployment level (one process per symbol),
  including configuration lock and restart.
- **Differential FIFO fuzzing** against a tiny independent oracle, extending the
  FlashOne-style check beyond its five operations.

### Out of scope for now

Auctions, pro-rata, price bands, pegged/trailing stops, OCO/OTO/OUO, sessions,
margin/risk and clearing. These belong to the order system or to a product
decision, not to a matching-core conformance batch.

## 7. Licence and attribution rules

- **MIT** (FlashOne, OrderBook-rs, intrepidkarthi, order-matcher, lob-engine):
  behaviour and test ideas may be re-implemented. Keep a comment or document
  reference when a case is directly inspired by one.
- **BSD-3-Clause** (Liquibook): same practical approach; retain attribution if
  code is ever copied, which it should not be for this batch.
- **Apache-2.0** (exchange-core): compatible, but keep NOTICE obligations in
  mind if code is copied.
- **LGPL-3.0** (NautilusTrader): reference only. Do not copy implementation
  code into this crate.
- **OpenBook v2**: majority MIT, but GPL-gated Solana parts exist. Do not copy
  the GPL-gated parts.

This batch contains original Rust tests written against this engine's public
API. No upstream code was copied. Names and scenario structure are attributed
here rather than in every test body; individual comments point at the behaviour
being pinned when it is non-obvious.

## 8. Reproducing the inventory

Test counts were obtained from each repository's test listing (`cargo test --
--list`, `npm test -- --list`, JUnit/Cucumber discovery) or, where a repository
could not be fetched because of rate limits, from its checked-in test tree. The
count for this repository is reproducible with:

```sh
cargo test --lib -- --list | tail -1
cargo test --test cluster -- --list | tail -1
ls -d tests/replay/*/ | wc -l
```

The first two lines report the number of unit and cluster tests; the third
reports replay cases.

## 9. Sources

- FlashOne differential benchmark: <https://github.com/flash1-dev/matching-engine-benchmark>
- OrderBook-rs: <https://github.com/joaquinbejar/OrderBook-rs>
- intrepidkarthi/orderbook: <https://github.com/intrepidkarthi/orderbook>
- Liquibook: <https://github.com/enewhuis/liquibook>
- order-matcher: <https://github.com/ArjunVachhani/order-matcher>
- lob-engine: <https://github.com/lvv123/lob-engine>
- NautilusTrader: <https://github.com/nautechsystems/nautilus_trader>
- OpenBook v2: <https://github.com/openbook-dex/openbook-v2>
- exchange-core: <https://github.com/exchange-core/exchange-core>
- CME order-type conformance (access-controlled): <https://www.cmegroup.com/confluence/display/EPICSANDBOX/Order+Types>
- SGX FIX order-entry scope: <https://www.sgx.com/securities/trading>
