//! Randomized differential tests for the limit-order book.
//!
//! A small reference model keeps a sorted vector per side. Each generated
//! operation is applied to both the real book and the model; the visible book,
//! trades, sequence counter, last trade price and volume accounting must agree
//! after every step. The same operation trace is replayed into a fresh book to
//! prove deterministic state transitions.

use super::*;

const SEEDS: u64 = 64;
const OPS_PER_SEED: usize = 256;

#[derive(Debug, Clone, Copy)]
enum Op {
    Place {
        side: OrderSide,
        price: Fixed,
        volume: Fixed,
        sequence: u64,
    },
    Cancel {
        side: OrderSide,
        price: Fixed,
        sequence: u64,
    },
    Amend {
        side: OrderSide,
        price: Fixed,
        sequence: u64,
        new_price: Option<Fixed>,
        new_volume: Option<Fixed>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefOrder {
    sequence: u64,
    side: OrderSide,
    price: Fixed,
    volume: Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefFill {
    maker_sequence: u64,
    maker_price: Fixed,
    maker_volume: Fixed,
    volume: Fixed,
}

struct RefBook {
    bids: Vec<RefOrder>,
    asks: Vec<RefOrder>,
    next_sequence: u64,
    last_trade_price: Option<Fixed>,
    submitted: Fixed,
    executed: Fixed,
    cancelled: Fixed,
}

struct RefAmendment {
    market_seq: u64,
    order: RefOrder,
    previous_sequence: u64,
    previous_price: Fixed,
    previous_volume: Fixed,
}

impl Default for RefBook {
    fn default() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            next_sequence: 0,
            last_trade_price: None,
            submitted: Fixed::ZERO,
            executed: Fixed::ZERO,
            cancelled: Fixed::ZERO,
        }
    }
}

impl RefBook {
    fn orders(&self, side: OrderSide) -> &[RefOrder] {
        match side {
            OrderSide::Buy => &self.bids,
            OrderSide::Sell => &self.asks,
        }
    }

    fn orders_mut(&mut self, side: OrderSide) -> &mut Vec<RefOrder> {
        match side {
            OrderSide::Buy => &mut self.bids,
            OrderSide::Sell => &mut self.asks,
        }
    }

    fn sort(&mut self, side: OrderSide) {
        let orders = self.orders_mut(side);
        match side {
            OrderSide::Buy => orders.sort_by(|a, b| {
                b.price
                    .cmp(&a.price)
                    .then_with(|| a.sequence.cmp(&b.sequence))
            }),
            OrderSide::Sell => orders.sort_by(|a, b| {
                a.price
                    .cmp(&b.price)
                    .then_with(|| a.sequence.cmp(&b.sequence))
            }),
        }
    }

    fn best_bid(&self) -> Option<RefOrder> {
        self.bids.first().copied()
    }

    fn best_ask(&self) -> Option<RefOrder> {
        self.asks.first().copied()
    }

    fn resting_orders(&self) -> Vec<RefOrder> {
        self.bids.iter().chain(&self.asks).copied().collect()
    }

    fn resting_volume(&self) -> Fixed {
        self.bids
            .iter()
            .chain(&self.asks)
            .fold(Fixed::ZERO, |total, order| total + order.volume)
    }

    fn place(
        &mut self,
        side: OrderSide,
        price: Fixed,
        volume: Fixed,
    ) -> (u64, Vec<RefFill>, Option<Fixed>) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.submitted += volume;

        let mut remaining = volume;
        let mut fills = Vec::new();
        let opposite = opposite(side);

        while remaining > Fixed::ZERO {
            let index = self.orders(opposite).iter().position(|maker| match side {
                OrderSide::Buy => maker.price <= price,
                OrderSide::Sell => maker.price >= price,
            });
            let Some(index) = index else {
                break;
            };

            let maker = self.orders(opposite)[index];
            let matched = remaining.min(maker.volume);
            fills.push(RefFill {
                maker_sequence: maker.sequence,
                maker_price: maker.price,
                maker_volume: maker.volume,
                volume: matched,
            });
            self.executed += matched;
            remaining -= matched;

            let makers = self.orders_mut(opposite);
            if matched == maker.volume {
                makers.remove(index);
            } else {
                makers[index].volume -= matched;
            }
        }

        if let Some(last) = fills.last() {
            self.last_trade_price = Some(last.maker_price);
        }

        let resting = if remaining > Fixed::ZERO {
            let order = RefOrder {
                sequence,
                side,
                price,
                volume: remaining,
            };
            self.orders_mut(side).push(order);
            self.sort(side);
            Some(remaining)
        } else {
            None
        };

        (sequence, fills, resting)
    }

    fn cancel(&mut self, side: OrderSide, price: Fixed, sequence: u64) -> Option<RefOrder> {
        let orders = self.orders_mut(side);
        let index = orders
            .iter()
            .position(|order| order.price == price && order.sequence == sequence)?;
        let removed = orders.remove(index);
        self.cancelled += removed.volume;
        Some(removed)
    }

    fn amend(
        &mut self,
        side: OrderSide,
        price: Fixed,
        sequence: u64,
        new_price: Option<Fixed>,
        new_volume: Option<Fixed>,
    ) -> Result<RefAmendment, AmendError> {
        let market_seq = self.next_sequence;
        self.next_sequence += 1;

        let stored = self
            .orders(side)
            .iter()
            .find(|order| order.price == price && order.sequence == sequence)
            .copied()
            .ok_or(AmendError::NotFound)?;
        let new_price = new_price.unwrap_or(stored.price);
        let new_volume = new_volume.unwrap_or(stored.volume);

        if new_price != stored.price {
            let would_cross = match side {
                OrderSide::Buy => self.best_ask().is_some_and(|ask| ask.price <= new_price),
                OrderSide::Sell => self.best_bid().is_some_and(|bid| bid.price >= new_price),
            };
            if would_cross {
                return Err(AmendError::WouldCross);
            }
        }

        if new_volume > stored.volume {
            self.submitted += new_volume - stored.volume;
        } else if new_volume < stored.volume {
            self.cancelled += stored.volume - new_volume;
        }
        let reinserted = new_price != stored.price || new_volume > stored.volume;
        let updated = if reinserted {
            RefOrder {
                sequence: market_seq,
                side,
                price: new_price,
                volume: new_volume,
            }
        } else {
            RefOrder {
                volume: new_volume,
                ..stored
            }
        };

        if reinserted {
            let orders = self.orders_mut(side);
            let index = orders
                .iter()
                .position(|order| order.price == price && order.sequence == sequence)
                .expect("the caller looked the order up");
            orders.remove(index);
            orders.push(updated);
            self.sort(side);
        } else {
            let orders = self.orders_mut(side);
            let index = orders
                .iter()
                .position(|order| order.price == price && order.sequence == sequence)
                .expect("the caller looked the order up");
            orders[index] = updated;
        }

        Ok(RefAmendment {
            market_seq,
            order: updated,
            previous_sequence: stored.sequence,
            previous_price: stored.price,
            previous_volume: stored.volume,
        })
    }
}

fn opposite(side: OrderSide) -> OrderSide {
    match side {
        OrderSide::Buy => OrderSide::Sell,
        OrderSide::Sell => OrderSide::Buy,
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, upper: u64) -> u64 {
        debug_assert!(upper > 0);
        self.next_u64() % upper
    }
}

fn random_price(rng: &mut Rng) -> Fixed {
    Fixed::from_units(90 + rng.below(21) as i128)
}

fn random_volume(rng: &mut Rng) -> Fixed {
    Fixed::from_units(1 + rng.below(10) as i128)
}

fn next_op(rng: &mut Rng, model: &RefBook) -> Op {
    let resting = model.resting_orders();
    if resting.is_empty() || rng.below(100) < 55 {
        return Op::Place {
            side: if rng.below(2) == 0 {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            },
            price: random_price(rng),
            volume: random_volume(rng),
            sequence: model.next_sequence,
        };
    }

    let target = resting[rng.below(resting.len() as u64) as usize];
    if rng.below(100) < 45 {
        Op::Cancel {
            side: target.side,
            price: target.price,
            sequence: target.sequence,
        }
    } else {
        let new_price = if rng.below(3) == 0 {
            None
        } else {
            Some(random_price(rng))
        };
        let new_volume = if rng.below(3) == 0 {
            None
        } else {
            Some(random_volume(rng))
        };
        Op::Amend {
            side: target.side,
            price: target.price,
            sequence: target.sequence,
            new_price,
            new_volume,
        }
    }
}

fn apply_op(book: &mut OrderBook, model: &mut RefBook, op: &Op, context: &str) {
    match *op {
        Op::Place {
            side,
            price,
            volume,
            sequence,
        } => {
            assert_eq!(sequence, model.next_sequence, "{context}");
            let mut order = Order::new(side, price, volume, 0);
            let outcome = book.execute_order(&mut order);
            let (expected_sequence, expected_fills, expected_resting) =
                model.place(side, price, volume);

            assert_eq!(expected_sequence, sequence, "{context}");
            assert_eq!(order.sequence, expected_sequence, "{context}");
            assert_eq!(outcome.market_seq, expected_sequence, "{context}");
            assert_eq!(outcome.rejected, None, "{context}");
            assert!(outcome.expired.is_empty(), "{context}");
            assert_eq!(outcome.cancelled, None, "{context}");
            assert_eq!(outcome.resting, expected_resting, "{context}");
            assert_eq!(outcome.fills.len(), expected_fills.len(), "{context}");
            for (fill, expected) in outcome.fills.iter().zip(&expected_fills) {
                assert_eq!(fill.taker.sequence, sequence, "{context}");
                assert_eq!(fill.maker.sequence, expected.maker_sequence, "{context}");
                assert_eq!(fill.maker.price, expected.maker_price, "{context}");
                assert_eq!(fill.maker.volume, expected.maker_volume, "{context}");
                assert_eq!(fill.matches, expected.volume, "{context}");
            }
        }
        Op::Cancel {
            side,
            price,
            sequence,
        } => {
            let key = Order::new(side, price, Fixed::from_units(1), sequence);
            let actual = book.cancel(&key);
            let expected = model.cancel(side, price, sequence);
            assert_eq!(
                actual.map(|order| (order.sequence, order.price, order.volume)),
                expected.map(|order| (order.sequence, order.price, order.volume)),
                "{context}"
            );
        }
        Op::Amend {
            side,
            price,
            sequence,
            new_price,
            new_volume,
        } => {
            let key = Order::new(side, price, Fixed::from_units(1), sequence);
            let actual = book.amend(&key, new_price, new_volume);
            let expected = model.amend(side, price, sequence, new_price, new_volume);
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => {
                    assert_eq!(actual.market_seq, expected.market_seq, "{context}");
                    assert_eq!(actual.order.sequence, expected.order.sequence, "{context}");
                    assert_eq!(actual.order.price, expected.order.price, "{context}");
                    assert_eq!(actual.order.volume, expected.order.volume, "{context}");
                    assert_eq!(
                        actual.previous_sequence, expected.previous_sequence,
                        "{context}"
                    );
                    assert_eq!(actual.previous_price, expected.previous_price, "{context}");
                    assert_eq!(
                        actual.previous_volume, expected.previous_volume,
                        "{context}"
                    );
                }
                (Err(actual), Err(expected)) => assert_eq!(actual, expected, "{context}"),
                (actual, expected) => {
                    panic!(
                        "{context}: amendment mismatch: actual={actual:?}, expected={:?}",
                        expected.map(|amendment| amendment.order)
                    );
                }
            }
        }
    }

    assert_book_matches(book, model, context);
}

fn assert_book_matches(book: &OrderBook, model: &RefBook, context: &str) {
    let actual_bids: Vec<_> = book
        .bids
        .values()
        .map(|order| (order.sequence, order.price, order.volume))
        .collect();
    let expected_bids: Vec<_> = model
        .bids
        .iter()
        .map(|order| (order.sequence, order.price, order.volume))
        .collect();
    assert_eq!(actual_bids, expected_bids, "{context}");

    let actual_asks: Vec<_> = book
        .asks
        .values()
        .map(|order| (order.sequence, order.price, order.volume))
        .collect();
    let expected_asks: Vec<_> = model
        .asks
        .iter()
        .map(|order| (order.sequence, order.price, order.volume))
        .collect();
    assert_eq!(actual_asks, expected_asks, "{context}");

    assert_eq!(book.sequence, model.next_sequence, "{context}");
    assert_eq!(
        book.last_trade_price, model.last_trade_price,
        "{context}: last trade price"
    );

    let actual_resting = book
        .bids
        .values()
        .chain(book.asks.values())
        .fold(Fixed::ZERO, |total, order| total + order.volume);
    assert_eq!(actual_resting, model.resting_volume(), "{context}");
    // Every trade consumes one unit on each side, so submitted volume equals
    // twice the one-sided fill volume plus what still rests or was cancelled.
    assert_eq!(
        model.submitted,
        model.executed + model.executed + model.cancelled + model.resting_volume(),
        "{context}: volume conservation"
    );

    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
        assert!(bid.price < ask.price, "{context}: crossed book");
    }
    assert_priority(
        book.bids
            .values()
            .map(|order| (order.price, order.sequence)),
        OrderSide::Buy,
        context,
    );
    assert_priority(
        book.asks
            .values()
            .map(|order| (order.price, order.sequence)),
        OrderSide::Sell,
        context,
    );
}

fn assert_priority(orders: impl Iterator<Item = (Fixed, u64)>, side: OrderSide, context: &str) {
    let mut previous: Option<(Fixed, u64)> = None;
    for (price, sequence) in orders {
        if let Some((previous_price, previous_sequence)) = previous {
            let ordered = match side {
                OrderSide::Buy => {
                    previous_price > price
                        || (previous_price == price && previous_sequence < sequence)
                }
                OrderSide::Sell => {
                    previous_price < price
                        || (previous_price == price && previous_sequence < sequence)
                }
            };
            assert!(
                ordered,
                "{context}: price-time priority broken for {side:?} at {price} ({sequence})"
            );
        }
        previous = Some((price, sequence));
    }
}

#[test]
fn randomized_limit_book_matches_reference_model() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut book = OrderBook::new();
        let mut model = RefBook::default();
        let mut ops = Vec::with_capacity(OPS_PER_SEED);

        for step in 0..OPS_PER_SEED {
            let op = next_op(&mut rng, &model);
            let context = format!("seed {seed} step {step} op {op:?}");
            apply_op(&mut book, &mut model, &op, &context);
            ops.push(op);
        }

        // Replaying the same trace into a fresh book must reproduce the final
        // state exactly, with no dependence on iteration order or wall time.
        let mut replay = OrderBook::new();
        let mut replay_model = RefBook::default();
        for (step, op) in ops.iter().enumerate() {
            let context = format!("replay seed {seed} step {step} op {op:?}");
            apply_op(&mut replay, &mut replay_model, op, &context);
        }
        assert_eq!(
            serde_json::to_value(&book).unwrap(),
            serde_json::to_value(&replay).unwrap(),
            "seed {seed}: replay diverged"
        );
    }
}

// ---------------------------------------------------------------------------
// Mixed order types and fee styles
// ---------------------------------------------------------------------------

use super::fixed::SCALE;

const MIXED_SEEDS: u64 = 32;
const MIXED_OPS: usize = 160;

#[derive(Debug, Clone, Copy)]
enum MixedOp {
    Place(Order),
    Cancel(Order),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VolumeCounters {
    submitted: Fixed,
    executed: Fixed,
    cancelled: Fixed,
}

fn random_fee_rate(rng: &mut Rng) -> Fixed {
    match rng.below(6) {
        0 => Fixed::ZERO,
        1 => Fixed::from_raw(1_000_000_000_000_000), // 0.001
        2 => Fixed::from_raw(-500_000_000_000_000),  // -0.0005 rebate
        3 => Fixed::from_raw(2_000_000_000_000_000), // 0.002
        4 => Fixed::from_raw(-1_000_000_000_000_000), // -0.001 rebate
        _ => Fixed::from_raw(100_000_000_000_000),   // 0.0001
    }
}

fn random_policy(seed: u64) -> FeeAssetPolicy {
    match seed % 3 {
        0 => FeeAssetPolicy::Received,
        1 => FeeAssetPolicy::Quote,
        _ => FeeAssetPolicy::Base,
    }
}

fn random_order(rng: &mut Rng) -> Order {
    let side = if rng.below(2) == 0 {
        OrderSide::Buy
    } else {
        OrderSide::Sell
    };
    let market = rng.below(100) < 20;
    let tif = if market {
        TimeInForce::Gtc
    } else {
        match rng.below(100) {
            0..=19 => TimeInForce::Ioc,
            20..=29 => TimeInForce::Fok,
            30..=39 => TimeInForce::Gtd(u64::MAX / 4),
            40..=49 => TimeInForce::Day,
            _ => TimeInForce::Gtc,
        }
    };
    let immediate = tif.is_immediate();
    let post_only = !market && tif == TimeInForce::Gtc && rng.below(100) < 10;
    let stop = !post_only && !immediate && rng.below(100) < 10;
    let iceberg = !post_only && !stop && !market && !immediate && rng.below(100) < 10;
    let volume_units = 1 + rng.below(10);
    let volume = Fixed::from_units(volume_units as i128);
    let price = random_price(rng);

    let mut order = Order::new(side, price, volume, 0);
    order.live = if market {
        OrderType::Market
    } else {
        OrderType::Limit
    };
    order.tif = tif;
    order.post_only = post_only;
    if stop {
        order.stop_price = random_price(rng);
    }
    if iceberg {
        order.display_volume = Fixed::from_units((1 + rng.below(volume_units)) as i128);
    }
    order.maker_fee_rate = random_fee_rate(rng);
    order.taker_fee_rate = random_fee_rate(rng);
    order
}

fn reference_mul(left: Fixed, right: Fixed) -> Fixed {
    let product = i256::I256::from_i128(left.raw())
        .checked_mul(i256::I256::from_i128(right.raw()))
        .expect("test values fit i256");
    let scaled = product
        .checked_div(i256::I256::from_i128(SCALE))
        .expect("test scale is non-zero");
    Fixed::from_raw(scaled.as_i128())
}

fn reference_notional(price: Fixed, volume: Fixed) -> Fixed {
    reference_mul(price, volume)
}

fn reference_fee(asset: FeeAsset, notional: Fixed, base_volume: Fixed, rate: Fixed) -> Fixed {
    let amount = match asset {
        FeeAsset::Base => base_volume,
        FeeAsset::Quote => notional,
    };
    reference_mul(amount, rate)
}

fn assert_fill_fees(fill: &MatchResult, policy: FeeAssetPolicy, context: &str) {
    let notional = reference_notional(fill.maker.price, fill.matches);
    let taker_asset = policy.fee_asset(fill.taker.side);
    let maker_asset = policy.fee_asset(fill.maker.side);
    let taker_fee = reference_fee(
        taker_asset,
        notional,
        fill.matches,
        fill.taker.taker_fee_rate,
    );
    let maker_fee = reference_fee(
        maker_asset,
        notional,
        fill.matches,
        fill.maker.maker_fee_rate,
    );

    assert_eq!(fill.notional, notional, "{context}: notional");
    assert_eq!(fill.taker_fee_asset, taker_asset, "{context}: taker asset");
    assert_eq!(fill.maker_fee_asset, maker_asset, "{context}: maker asset");
    assert_eq!(fill.taker_fee, taker_fee, "{context}: taker fee");
    assert_eq!(fill.maker_fee, maker_fee, "{context}: maker fee");
    assert!(!fill.fee_overflow, "{context}: unexpected fee overflow");
}

/// Applies one mixed operation, validates every fill's fee and returns the
/// accepted order when it may later be cancelled.
fn apply_mixed_op(
    book: &mut OrderBook,
    counters: &mut VolumeCounters,
    policy: FeeAssetPolicy,
    op: &MixedOp,
    context: &str,
) -> Option<Order> {
    match *op {
        MixedOp::Place(mut order) => {
            let config = SymbolConfig::default().with_fee_asset_policy(policy);
            let outcome = book.execute_order_with_symbol(&mut order, config);

            if let Some(reason) = outcome.rejected {
                assert!(
                    matches!(
                        reason,
                        RejectReason::FokUnfillable
                            | RejectReason::PostOnlyWouldCross
                            | RejectReason::StopWouldTrigger
                    ),
                    "{context}: unexpected rejection {reason:?}"
                );
                assert!(outcome.fills.is_empty(), "{context}: rejected order filled");
                assert_eq!(outcome.resting, None, "{context}: rejected order rested");
                assert_eq!(
                    outcome.cancelled, None,
                    "{context}: rejected order cancelled"
                );
                assert!(
                    outcome.triggers.is_empty(),
                    "{context}: rejected order triggered"
                );
            } else {
                counters.submitted += order.volume;
            }
            if outcome.parked {
                assert!(outcome.fills.is_empty(), "{context}: parked stop filled");
                assert_eq!(outcome.resting, None, "{context}: parked stop rested");
                assert_eq!(outcome.cancelled, None, "{context}: parked stop cancelled");
            }
            if order.live == OrderType::Market || order.tif.is_immediate() {
                assert_eq!(outcome.resting, None, "{context}: immediate order rested");
            }
            if order.post_only && outcome.rejected.is_none() {
                assert!(outcome.fills.is_empty(), "{context}: post-only filled");
                assert_eq!(
                    outcome.resting,
                    Some(order.volume),
                    "{context}: post-only did not rest whole"
                );
            }
            assert!(outcome.expired.is_empty(), "{context}: unexpected expiry");
            for fill in &outcome.fills {
                assert_fill_fees(fill, policy, context);
                counters.executed += fill.matches;
            }
            counters.cancelled += outcome.cancelled.unwrap_or(Fixed::ZERO);
            for trigger in &outcome.triggers {
                for fill in &trigger.fills {
                    assert_fill_fees(fill, policy, context);
                    counters.executed += fill.matches;
                }
                counters.cancelled += trigger.cancelled.unwrap_or(Fixed::ZERO);
            }

            if outcome.rejected.is_none() {
                Some(order)
            } else {
                None
            }
        }
        MixedOp::Cancel(order) => {
            if let Some(removed) = book.cancel(&order) {
                counters.cancelled += removed.open_volume();
            }
            None
        }
    }
}

fn open_volume(book: &OrderBook) -> Fixed {
    let resting = book
        .bids
        .values()
        .chain(book.asks.values())
        .fold(Fixed::ZERO, |total, order| total + order.open_volume());
    let stops = book
        .stops
        .iter()
        .fold(Fixed::ZERO, |total, order| total + order.open_volume());
    resting + stops
}

fn assert_mixed_invariants(book: &OrderBook, counters: &VolumeCounters, context: &str) {
    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
        assert!(bid.price < ask.price, "{context}: crossed book");
    }
    assert_priority(
        book.bids
            .values()
            .map(|order| (order.price, order.sequence)),
        OrderSide::Buy,
        context,
    );
    assert_priority(
        book.asks
            .values()
            .map(|order| (order.price, order.sequence)),
        OrderSide::Sell,
        context,
    );

    // Submitted volume is counted once per accepted order; each trade consumes
    // one unit on both sides, so fills contribute twice.
    assert_eq!(
        counters.submitted,
        counters.executed + counters.executed + counters.cancelled + open_volume(book),
        "{context}: mixed volume conservation"
    );
}

#[derive(Debug, Default)]
struct MixedCoverage {
    limit: bool,
    market: bool,
    gtc: bool,
    ioc: bool,
    fok: bool,
    gtd: bool,
    day: bool,
    post_only: bool,
    stop: bool,
    stop_market: bool,
    stop_limit: bool,
    iceberg: bool,
    positive_rate: bool,
    negative_rate: bool,
    zero_rate: bool,
    received: bool,
    quote: bool,
    base: bool,
}

impl MixedCoverage {
    fn observe_order(&mut self, order: &Order) {
        match order.live {
            OrderType::Limit => self.limit = true,
            OrderType::Market => self.market = true,
        }
        match order.tif {
            TimeInForce::Gtc => self.gtc = true,
            TimeInForce::Ioc => self.ioc = true,
            TimeInForce::Fok => self.fok = true,
            TimeInForce::Gtd(_) => self.gtd = true,
            TimeInForce::Day => self.day = true,
        }
        self.post_only |= order.post_only;
        if order.is_stop() {
            self.stop = true;
            match order.live {
                OrderType::Market => self.stop_market = true,
                OrderType::Limit => self.stop_limit = true,
            }
        }
        self.iceberg |= order.display_volume > Fixed::ZERO;
        for rate in [order.maker_fee_rate, order.taker_fee_rate] {
            if rate > Fixed::ZERO {
                self.positive_rate = true;
            } else if rate < Fixed::ZERO {
                self.negative_rate = true;
            } else {
                self.zero_rate = true;
            }
        }
    }

    fn observe_policy(&mut self, policy: FeeAssetPolicy) {
        match policy {
            FeeAssetPolicy::Received => self.received = true,
            FeeAssetPolicy::Quote => self.quote = true,
            FeeAssetPolicy::Base => self.base = true,
        }
    }

    fn assert_complete(&self) {
        let missing: Vec<_> = [
            ("limit", self.limit),
            ("market", self.market),
            ("gtc", self.gtc),
            ("ioc", self.ioc),
            ("fok", self.fok),
            ("gtd", self.gtd),
            ("day", self.day),
            ("post_only", self.post_only),
            ("stop", self.stop),
            ("stop_market", self.stop_market),
            ("stop_limit", self.stop_limit),
            ("iceberg", self.iceberg),
            ("positive_rate", self.positive_rate),
            ("negative_rate", self.negative_rate),
            ("zero_rate", self.zero_rate),
            ("received_policy", self.received),
            ("quote_policy", self.quote),
            ("base_policy", self.base),
        ]
        .into_iter()
        .filter_map(|(name, present)| (!present).then_some(name))
        .collect();
        assert!(
            missing.is_empty(),
            "mixed property coverage missing {missing:?}"
        );
    }
}

#[test]
fn randomized_order_types_and_fee_styles_preserve_invariants() {
    let mut coverage = MixedCoverage::default();
    for seed in 0..MIXED_SEEDS {
        let policy = random_policy(seed);
        coverage.observe_policy(policy);
        let mut rng = Rng::new(seed ^ 0x5EED_5EED);
        let mut book = OrderBook::new();
        let mut counters = VolumeCounters::default();
        let mut accepted = Vec::new();
        let mut ops = Vec::with_capacity(MIXED_OPS);

        for step in 0..MIXED_OPS {
            let op = if !accepted.is_empty() && rng.below(100) < 20 {
                MixedOp::Cancel(accepted[rng.below(accepted.len() as u64) as usize])
            } else {
                MixedOp::Place(random_order(&mut rng))
            };
            if let MixedOp::Place(order) = &op {
                coverage.observe_order(order);
            }
            let context = format!("mixed seed {seed} step {step} op {op:?}");
            if let Some(order) = apply_mixed_op(&mut book, &mut counters, policy, &op, &context) {
                accepted.push(order);
            }
            assert_mixed_invariants(&book, &counters, &context);
            ops.push(op);
        }

        // The same concrete trace must reproduce the same state and accounting.
        let mut replay = OrderBook::new();
        let mut replay_counters = VolumeCounters::default();
        let mut replay_accepted = Vec::new();
        for (step, op) in ops.iter().enumerate() {
            let context = format!("replay mixed seed {seed} step {step} op {op:?}");
            if let Some(order) =
                apply_mixed_op(&mut replay, &mut replay_counters, policy, op, &context)
            {
                replay_accepted.push(order);
            }
            assert_mixed_invariants(&replay, &replay_counters, &context);
        }
        assert_eq!(counters, replay_counters, "seed {seed}: replay accounting");
        assert_eq!(
            accepted.len(),
            replay_accepted.len(),
            "seed {seed}: replay accepts"
        );
        assert_eq!(
            serde_json::to_value(&book).unwrap(),
            serde_json::to_value(&replay).unwrap(),
            "seed {seed}: mixed replay diverged"
        );
    }
    coverage.assert_complete();
}
