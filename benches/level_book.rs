//! Prototype: per-price-level buckets vs the current per-order `BTreeMap`.
//!
//! Run with `cargo bench --bench level_book`. This is a **prototype**, not the
//! production book. It keeps one `Vec<Option<Order>>` per price level, a
//! `head` cursor for the consumed prefix, a sequence-keyed index for
//! cancel/amend and cached best bid/ask prices.
//!
//! Scope: plain limit orders only. The prototype skips stops, icebergs, GTD
//! expiry, validation, events, the depth journal and the market-data tap, so the
//! numbers isolate the book data structure. It is compared side-by-side with
//! `OrderBook` on the same workloads; adoption remains a P2 decision.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::hint::black_box;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::order_book::Amendment;
use matchengine_raft::order_book::DepthLevel;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchResult;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;

/// One price level: live orders in time priority plus a consumed prefix.
#[derive(Clone, Default)]
struct Level {
    /// Slots are `None` after a fill or cancel. Filled slots before `head`
    /// stay allocated until the whole level is removed; cancels can tombstone
    /// any slot.
    orders: Vec<Option<Order>>,
    /// First slot not yet consumed by a sweep.
    head: usize,
    /// Number of live (non-`None`) slots.
    live: usize,
}

impl Level {
    fn push(&mut self, order: Order) -> usize {
        let slot = self.orders.len();
        self.orders.push(Some(order));
        self.live += 1;
        slot
    }

    fn live_orders(&self) -> impl Iterator<Item = &Order> {
        self.orders[self.head..].iter().filter_map(Option::as_ref)
    }
}

#[derive(Clone, Copy)]
struct Location {
    side: OrderSide,
    price: Fixed,
    slot: usize,
}

/// Identity hasher for the monotonic sequence keys. A production index would
/// use an integer hasher (FxHash-style); the standard SipHash variant is
/// measured separately in the performance notes.
#[derive(Default)]
struct SequenceHasher(u64);

impl Hasher for SequenceHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(5) ^ u64::from(*byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type SequenceIndex = HashMap<u64, Location, BuildHasherDefault<SequenceHasher>>;

/// The prototype book: price levels, a sequence index and cached BBO.
#[derive(Clone, Default)]
struct LevelBook {
    /// Ascending price. The best bid is the last key.
    bids: BTreeMap<Fixed, Level>,
    /// Ascending price. The best ask is the first key.
    asks: BTreeMap<Fixed, Level>,
    /// O(1) cancel/amend lookup: sequence -> side/price/slot.
    index: SequenceIndex,
    best_bid: Option<Fixed>,
    best_ask: Option<Fixed>,
    sequence: u64,
}

impl LevelBook {
    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    fn insert_order(&mut self, order: Order) {
        let slot = match order.side {
            OrderSide::Buy => self.bids.entry(order.price).or_default().push(order),
            OrderSide::Sell => self.asks.entry(order.price).or_default().push(order),
        };
        self.index.insert(
            order.sequence,
            Location {
                side: order.side,
                price: order.price,
                slot,
            },
        );
        match order.side {
            OrderSide::Buy => {
                if self.best_bid.is_none_or(|best| order.price > best) {
                    self.best_bid = Some(order.price);
                }
            }
            OrderSide::Sell => {
                if self.best_ask.is_none_or(|best| order.price < best) {
                    self.best_ask = Some(order.price);
                }
            }
        }
    }

    fn take_from_level(&mut self, side: OrderSide, price: Fixed, slot: usize) -> Option<Order> {
        let (order, live) = {
            let level = match side {
                OrderSide::Buy => self.bids.get_mut(&price)?,
                OrderSide::Sell => self.asks.get_mut(&price)?,
            };
            let order = level.orders.get_mut(slot)?.take()?;
            level.live -= 1;
            (order, level.live)
        };
        if live == 0 {
            match side {
                OrderSide::Buy => {
                    self.bids.remove(&price);
                    if self.best_bid == Some(price) {
                        self.best_bid = self.bids.keys().next_back().copied();
                    }
                }
                OrderSide::Sell => {
                    self.asks.remove(&price);
                    if self.best_ask == Some(price) {
                        self.best_ask = self.asks.keys().next().copied();
                    }
                }
            }
        }
        Some(order)
    }

    fn cancel(&mut self, sequence: u64) -> Option<Order> {
        let location = self.index.remove(&sequence)?;
        self.take_from_level(location.side, location.price, location.slot)
    }

    fn amend(
        &mut self,
        sequence: u64,
        new_price: Option<Fixed>,
        new_volume: Option<Fixed>,
    ) -> Option<Amendment> {
        // Match the current book: every amendment consumes one market sequence.
        let market_seq = self.next_sequence();
        let location = *self.index.get(&sequence)?;
        let (order, previous_volume) = {
            let level = match location.side {
                OrderSide::Buy => self.bids.get_mut(&location.price)?,
                OrderSide::Sell => self.asks.get_mut(&location.price)?,
            };
            let order = level.orders.get_mut(location.slot)?.as_mut()?;
            let previous_volume = order.volume;
            if let Some(volume) = new_volume {
                if volume <= Fixed::ZERO {
                    return None;
                }
                order.volume = volume;
            }
            (*order, previous_volume)
        };

        if let Some(price) = new_price {
            if price <= Fixed::ZERO {
                return None;
            }
            if price != order.price {
                self.index.remove(&sequence);
                self.take_from_level(location.side, location.price, location.slot);
                let mut moved = order;
                moved.price = price;
                moved.sequence = market_seq;
                self.insert_order(moved);
                return Some(Amendment {
                    market_seq,
                    order: moved,
                    previous_sequence: sequence,
                    previous_price: location.price,
                    previous_volume,
                });
            }
        }

        Some(Amendment {
            market_seq,
            order,
            previous_sequence: sequence,
            previous_price: location.price,
            previous_volume,
        })
    }

    fn match_taker(&mut self, taker: &mut Order, limit: Option<Fixed>) -> Vec<MatchResult> {
        taker.sequence = self.next_sequence();
        let mut fills = Vec::new();
        let mut remaining = taker.volume;
        let mut consumed = Vec::new();

        while remaining > Fixed::ZERO {
            let price = match taker.side {
                OrderSide::Buy => match self.asks.keys().next() {
                    Some(price) => *price,
                    None => break,
                },
                OrderSide::Sell => match self.bids.keys().next_back() {
                    Some(price) => *price,
                    None => break,
                },
            };
            if let Some(limit) = limit {
                match taker.side {
                    OrderSide::Buy if price > limit => break,
                    OrderSide::Sell if price < limit => break,
                    _ => {}
                }
            }

            let level_empty = {
                let level = match taker.side {
                    OrderSide::Buy => self.asks.get_mut(&price).expect("ask key exists"),
                    OrderSide::Sell => self.bids.get_mut(&price).expect("bid key exists"),
                };
                while remaining > Fixed::ZERO && level.head < level.orders.len() {
                    let Some(order) = level.orders[level.head].as_mut() else {
                        level.head += 1;
                        continue;
                    };
                    let maker = *order;
                    let matched = remaining.min(maker.volume);
                    let taker_at_fill = Order {
                        volume: remaining,
                        ..*taker
                    };
                    fills.push(MatchResult::new(&taker_at_fill, &maker, matched));
                    remaining -= matched;
                    order.volume -= matched;
                    if order.volume == Fixed::ZERO {
                        let sequence = order.sequence;
                        level.orders[level.head] = None;
                        level.head += 1;
                        level.live -= 1;
                        consumed.push(sequence);
                    } else {
                        // `matched` was bounded by the taker, so it is empty.
                        break;
                    }
                }
                level.live == 0
            };

            for sequence in consumed.drain(..) {
                self.index.remove(&sequence);
            }

            if level_empty {
                match taker.side {
                    OrderSide::Buy => {
                        self.asks.remove(&price);
                        if self.best_ask == Some(price) {
                            self.best_ask = self.asks.keys().next().copied();
                        }
                    }
                    OrderSide::Sell => {
                        self.bids.remove(&price);
                        if self.best_bid == Some(price) {
                            self.best_bid = self.bids.keys().next_back().copied();
                        }
                    }
                }
            }
        }

        fills
    }

    fn best_bid(&self) -> Option<Fixed> {
        self.best_bid
    }

    fn best_ask(&self) -> Option<Fixed> {
        self.best_ask
    }

    fn spread(&self) -> Option<Fixed> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) => ask.checked_sub(bid),
            _ => None,
        }
    }

    fn depth(&self, side: OrderSide, levels: usize) -> Vec<DepthLevel> {
        let mut depth = Vec::with_capacity(levels.min(64));
        if levels == 0 {
            return depth;
        }

        let iter: Box<dyn Iterator<Item = (&Fixed, &Level)>> = match side {
            OrderSide::Buy => Box::new(self.bids.iter().rev()),
            OrderSide::Sell => Box::new(self.asks.iter()),
        };

        for (&price, level) in iter {
            if level.live == 0 {
                continue;
            }
            let mut volume = Fixed::ZERO;
            let mut orders = 0u32;
            for order in level.live_orders() {
                volume += order.volume;
                orders += 1;
            }
            if orders == 0 {
                continue;
            }
            depth.push(DepthLevel {
                price,
                volume,
                orders,
            });
            if depth.len() == levels {
                break;
            }
        }

        depth
    }
}

fn current_sweep_book(levels: u32, per_level: u32) -> (OrderBook, Order) {
    let mut book = OrderBook::new();
    for i in 0..levels * per_level {
        let price = 100.0 + f64::from(i / per_level) * 0.01;
        let mut ask = Order::new(OrderSide::Sell, price, 1.0, 0);
        ask.id = u64::from(i);
        book.place_order(&mut ask);
    }
    let total = f64::from(levels * per_level);
    let taker = Order::new(OrderSide::Buy, 100.0 + f64::from(levels) + 1.0, total, 0);
    (book, taker)
}

fn prototype_sweep_book(levels: u32, per_level: u32) -> (LevelBook, Order) {
    let mut book = LevelBook::default();
    for i in 0..levels * per_level {
        let price = 100.0 + f64::from(i / per_level) * 0.01;
        let mut ask = Order::new(OrderSide::Sell, price, 1.0, 0);
        ask.id = u64::from(i);
        ask.sequence = book.next_sequence();
        book.insert_order(ask);
    }
    let total = f64::from(levels * per_level);
    let taker = Order::new(OrderSide::Buy, 100.0 + f64::from(levels) + 1.0, total, 0);
    (book, taker)
}

fn current_cancel_book(levels: u32, per_level: u32) -> (OrderBook, Vec<Order>) {
    let mut book = OrderBook::new();
    let mut deep = Vec::new();
    for level in 0..levels {
        for offset in 0..per_level {
            let price = 100.0 - f64::from(level) * 0.01;
            let mut bid = Order::new(OrderSide::Buy, price, 1.0, 0);
            bid.id = u64::from(level * per_level + offset);
            book.place_order(&mut bid);
            if level >= levels / 2 {
                deep.push(bid);
            }
        }
    }
    (book, deep)
}

fn prototype_cancel_book(levels: u32, per_level: u32) -> (LevelBook, Vec<u64>) {
    let mut book = LevelBook::default();
    let mut deep = Vec::new();
    for level in 0..levels {
        for offset in 0..per_level {
            let price = 100.0 - f64::from(level) * 0.01;
            let mut bid = Order::new(OrderSide::Buy, price, 1.0, 0);
            bid.id = u64::from(level * per_level + offset);
            bid.sequence = book.next_sequence();
            book.insert_order(bid);
            if level >= levels / 2 {
                deep.push(bid.sequence);
            }
        }
    }
    (book, deep)
}

fn current_bbo_book(levels: u32) -> (OrderBook, Vec<Order>, Vec<Order>) {
    let mut book = OrderBook::new();
    let mut bids = Vec::with_capacity(levels as usize);
    let mut asks = Vec::with_capacity(levels as usize);
    for level in 0..levels {
        let mut bid = Order::new(OrderSide::Buy, 100.0 - f64::from(level) * 0.01, 1.0, 0);
        bid.id = u64::from(level);
        book.place_order(&mut bid);
        bids.push(bid);

        let mut ask = Order::new(OrderSide::Sell, 101.0 + f64::from(level) * 0.01, 1.0, 0);
        ask.id = u64::from(levels + level);
        book.place_order(&mut ask);
        asks.push(ask);
    }
    (book, bids, asks)
}

fn prototype_bbo_book(levels: u32) -> (LevelBook, Vec<u64>, Vec<u64>) {
    let mut book = LevelBook::default();
    let mut bids = Vec::with_capacity(levels as usize);
    let mut asks = Vec::with_capacity(levels as usize);
    for level in 0..levels {
        let mut bid = Order::new(OrderSide::Buy, 100.0 - f64::from(level) * 0.01, 1.0, 0);
        bid.id = u64::from(level);
        bid.sequence = book.next_sequence();
        book.insert_order(bid);
        bids.push(bid.sequence);

        let mut ask = Order::new(OrderSide::Sell, 101.0 + f64::from(level) * 0.01, 1.0, 0);
        ask.id = u64::from(levels + level);
        ask.sequence = book.next_sequence();
        book.insert_order(ask);
        asks.push(ask.sequence);
    }
    (book, bids, asks)
}

fn current_amend_book(levels: u32, per_level: u32) -> (OrderBook, Vec<Order>) {
    let mut book = OrderBook::new();
    let mut orders = Vec::with_capacity((levels * per_level) as usize);
    for level in 0..levels {
        let price = 100.0 - f64::from(level) * 0.01;
        for offset in 0..per_level {
            let mut bid = Order::new(OrderSide::Buy, price, 1.0, 0);
            bid.id = u64::from(level * per_level + offset);
            book.place_order(&mut bid);
            orders.push(bid);
        }
    }
    (book, orders)
}

fn prototype_amend_book(levels: u32, per_level: u32) -> (LevelBook, Vec<(u64, Fixed)>) {
    let mut book = LevelBook::default();
    let mut entries = Vec::with_capacity((levels * per_level) as usize);
    for level in 0..levels {
        let price = 100.0 - f64::from(level) * 0.01;
        for offset in 0..per_level {
            let mut bid = Order::new(OrderSide::Buy, price, 1.0, 0);
            bid.id = u64::from(level * per_level + offset);
            bid.sequence = book.next_sequence();
            book.insert_order(bid);
            entries.push((bid.sequence, bid.price));
        }
    }
    (book, entries)
}

#[derive(Clone, Copy)]
struct Measurement {
    duration: Duration,
    units: usize,
}

/// Clones `base` outside the timer, runs `op` and keeps the best of five
/// per-round averages. This mirrors `book_replay`.
fn measure<T>(
    rounds: u32,
    base: &T,
    clone: impl Fn(&T) -> T,
    mut op: impl FnMut(T) -> usize,
) -> Measurement {
    let mut best = Duration::MAX;
    let mut best_units = 0;

    for _ in 0..5 {
        let mut elapsed = Duration::ZERO;
        let mut units = 0usize;
        for _ in 0..rounds {
            let state = clone(base);
            let start = Instant::now();
            units += op(state);
            elapsed += start.elapsed();
        }
        let per_round = elapsed / rounds;
        if per_round < best {
            best = per_round;
            best_units = units / rounds as usize;
        }
    }

    Measurement {
        duration: best,
        units: best_units,
    }
}

fn report(name: &str, current: Measurement, prototype: Measurement, unit: &str) {
    let current_rate = current.units as f64 / current.duration.as_secs_f64();
    let prototype_rate = prototype.units as f64 / prototype.duration.as_secs_f64();
    let speedup = if current_rate > 0.0 {
        prototype_rate / current_rate
    } else {
        f64::INFINITY
    };
    println!(
        "{name:46} current {current:>10?} ({current_rate:>12.0} {unit}/s)  prototype {prototype:>10?} ({prototype_rate:>12.0} {unit}/s)  {speedup:>5.2}x",
        current = current.duration,
        prototype = prototype.duration,
    );
}

fn place_both(current: &mut OrderBook, prototype: &mut LevelBook, mut order: Order) -> Order {
    current.place_order(&mut order);
    let sequence = prototype.next_sequence();
    assert_eq!(order.sequence, sequence);
    let mut prototype_order = order;
    prototype_order.sequence = sequence;
    prototype.insert_order(prototype_order);
    order
}

fn assert_books_match(current: &OrderBook, prototype: &LevelBook) {
    assert_eq!(
        current.best_bid().map(|order| order.price),
        prototype.best_bid()
    );
    assert_eq!(
        current.best_ask().map(|order| order.price),
        prototype.best_ask()
    );
    assert_eq!(current.spread(), prototype.spread());
    for side in [OrderSide::Buy, OrderSide::Sell] {
        assert_eq!(
            current.depth(side, 10),
            prototype.depth(side, 10),
            "{side:?}"
        );
    }
}

fn assert_fills_match(current: &[MatchResult], prototype: &[MatchResult]) {
    assert_eq!(current.len(), prototype.len());
    for (current, prototype) in current.iter().zip(prototype) {
        assert_eq!(current.taker.side, prototype.taker.side);
        assert_eq!(current.taker.sequence, prototype.taker.sequence);
        assert_eq!(current.maker.sequence, prototype.maker.sequence);
        assert_eq!(current.maker.price, prototype.maker.price);
        assert_eq!(current.matches, prototype.matches);
        assert_eq!(current.notional, prototype.notional);
        assert_eq!(current.taker_fee, prototype.taker_fee);
        assert_eq!(current.maker_fee, prototype.maker_fee);
    }
}

/// Deterministic mixed workload; the prototype must match the current book's
/// BBO, depth, fills, cancels and amendments before any timing is reported.
fn verify() {
    let mut current = OrderBook::new();
    let mut prototype = LevelBook::default();

    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Buy, 99.0, 1.0, 0),
    );
    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Buy, 99.0, 1.0, 0),
    );
    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Buy, 98.0, 1.0, 0),
    );
    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Sell, 100.0, 1.0, 0),
    );
    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Sell, 100.0, 1.0, 0),
    );
    place_both(
        &mut current,
        &mut prototype,
        Order::new(OrderSide::Sell, 101.0, 1.0, 0),
    );
    assert_books_match(&current, &prototype);

    let mut current_taker = Order::new(OrderSide::Buy, 101.0, 2.5, 0);
    let mut prototype_taker = current_taker;
    let current_fills = current.place_order(&mut current_taker);
    let prototype_fills = prototype.match_taker(&mut prototype_taker, Some(101.0.into()));
    assert_fills_match(&current_fills, &prototype_fills);
    assert_books_match(&current, &prototype);

    // The 101 ask was partially filled; its index entry must still resolve.
    let current_cancelled = current
        .cancel(&Order::new(OrderSide::Sell, 101.0, 0.5, 5))
        .expect("partial ask exists");
    let prototype_cancelled = prototype.cancel(5).expect("partial ask exists");
    assert_eq!(current_cancelled.price, prototype_cancelled.price);
    assert_eq!(current_cancelled.volume, prototype_cancelled.volume);
    assert_books_match(&current, &prototype);

    let current_cancelled = current
        .cancel(&Order::new(OrderSide::Buy, 98.0, 1.0, 2))
        .expect("bid exists");
    let prototype_cancelled = prototype.cancel(2).expect("bid exists");
    assert_eq!(current_cancelled.price, prototype_cancelled.price);
    assert_eq!(current_cancelled.volume, prototype_cancelled.volume);
    assert_books_match(&current, &prototype);

    let current_amendment = current
        .amend(
            &Order::new(OrderSide::Buy, 99.0, 1.0, 0),
            None,
            Some(Fixed::from(0.5)),
        )
        .expect("amend succeeds");
    let prototype_amendment = prototype
        .amend(0, None, Some(Fixed::from(0.5)))
        .expect("amend succeeds");
    assert_eq!(current_amendment.market_seq, prototype_amendment.market_seq);
    assert_eq!(
        current_amendment.order.sequence,
        prototype_amendment.order.sequence
    );
    assert_eq!(
        current_amendment.order.volume,
        prototype_amendment.order.volume
    );
    assert_books_match(&current, &prototype);

    let current_amendment = current
        .amend(
            &Order::new(OrderSide::Buy, 99.0, 0.5, 0),
            Some(Fixed::from(98.5)),
            None,
        )
        .expect("amend succeeds");
    let prototype_amendment = prototype
        .amend(0, Some(Fixed::from(98.5)), None)
        .expect("amend succeeds");
    assert_eq!(current_amendment.market_seq, prototype_amendment.market_seq);
    assert_eq!(
        current_amendment.order.sequence,
        prototype_amendment.order.sequence
    );
    assert_eq!(
        current_amendment.order.price,
        prototype_amendment.order.price
    );
    assert_eq!(
        current_amendment.order.volume,
        prototype_amendment.order.volume
    );
    assert_books_match(&current, &prototype);
}

fn main() {
    verify();
    println!("prototype verification: OK");
    println!("per-level bucket prototype vs current OrderBook (book-only, best-of-5)");
    println!("scope: plain limit orders; no stops, icebergs, GTD, events, feed or journal\n");

    let (current_book, current_taker) = current_sweep_book(100, 10);
    let (prototype_book, prototype_taker) = prototype_sweep_book(100, 10);
    let current = measure(
        100,
        &(current_book, current_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let fills = book.place_order(&mut taker);
            assert_eq!(1_000, fills.len());
            assert!(book.asks.is_empty());
            fills.len()
        },
    );
    let prototype = measure(
        100,
        &(prototype_book, prototype_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let limit = taker.price;
            let fills = book.match_taker(&mut taker, Some(limit));
            assert_eq!(1_000, fills.len());
            assert!(book.asks.is_empty());
            fills.len()
        },
    );
    report("match 1k asks / 100 levels", current, prototype, "fills");

    let (current_book, current_taker) = current_sweep_book(1, 20_000);
    let (prototype_book, prototype_taker) = prototype_sweep_book(1, 20_000);
    let current = measure(
        20,
        &(current_book, current_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let fills = book.place_order(&mut taker);
            assert_eq!(20_000, fills.len());
            assert!(book.asks.is_empty());
            fills.len()
        },
    );
    let prototype = measure(
        20,
        &(prototype_book, prototype_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let limit = taker.price;
            let fills = book.match_taker(&mut taker, Some(limit));
            assert_eq!(20_000, fills.len());
            assert!(book.asks.is_empty());
            fills.len()
        },
    );
    report("match 20k asks / 1 level", current, prototype, "fills");

    let (current_book, current_orders) = current_cancel_book(100, 100);
    let (prototype_book, prototype_sequences) = prototype_cancel_book(100, 100);
    let current = measure(
        50,
        &(current_book, current_orders),
        |(book, orders)| (book.clone(), orders.clone()),
        |(mut book, orders)| {
            let mut cancelled = 0usize;
            for order in orders.iter().rev() {
                if book.cancel(order).is_some() {
                    cancelled += 1;
                }
            }
            assert_eq!(orders.len(), cancelled);
            cancelled
        },
    );
    let prototype = measure(
        50,
        &(prototype_book, prototype_sequences),
        |(book, sequences)| (book.clone(), sequences.clone()),
        |(mut book, sequences)| {
            let mut cancelled = 0usize;
            for &sequence in sequences.iter().rev() {
                if book.cancel(sequence).is_some() {
                    cancelled += 1;
                }
            }
            assert_eq!(sequences.len(), cancelled);
            cancelled
        },
    );
    report(
        "cancel 5k at depth / 50 levels",
        current,
        prototype,
        "cancels",
    );

    let (current_book, current_orders) = current_amend_book(5_000, 1);
    let (prototype_book, prototype_entries) = prototype_amend_book(5_000, 1);
    let reduced_volume = Fixed::from(0.5);
    let current = measure(
        50,
        &(current_book, current_orders),
        |(book, orders)| (book.clone(), orders.clone()),
        |(mut book, orders)| {
            let mut amended = 0usize;
            for order in &orders {
                if book.amend(order, None, Some(reduced_volume)).is_ok() {
                    amended += 1;
                }
            }
            assert_eq!(orders.len(), amended);
            amended
        },
    );
    let prototype = measure(
        50,
        &(prototype_book, prototype_entries),
        |(book, entries)| (book.clone(), entries.clone()),
        |(mut book, entries)| {
            let mut amended = 0usize;
            for &(sequence, _) in &entries {
                if book.amend(sequence, None, Some(reduced_volume)).is_some() {
                    amended += 1;
                }
            }
            assert_eq!(entries.len(), amended);
            amended
        },
    );
    report("amend volume 5k (same price)", current, prototype, "amends");

    let (current_book, current_orders) = current_amend_book(1_000, 1);
    let (prototype_book, prototype_entries) = prototype_amend_book(1_000, 1);
    let price_step = "0.005".parse::<Fixed>().expect("valid price step");
    let current = measure(
        50,
        &(current_book, current_orders),
        |(book, orders)| (book.clone(), orders.clone()),
        |(mut book, orders)| {
            let mut amended = 0usize;
            for order in &orders {
                let new_price = order.price.checked_sub(price_step).expect("price fits");
                if book.amend(order, Some(new_price), None).is_ok() {
                    amended += 1;
                }
            }
            assert_eq!(orders.len(), amended);
            amended
        },
    );
    let prototype = measure(
        50,
        &(prototype_book, prototype_entries),
        |(book, entries)| (book.clone(), entries.clone()),
        |(mut book, entries)| {
            let mut amended = 0usize;
            for &(sequence, price) in &entries {
                let new_price = price.checked_sub(price_step).expect("price fits");
                if book.amend(sequence, Some(new_price), None).is_some() {
                    amended += 1;
                }
            }
            assert_eq!(entries.len(), amended);
            amended
        },
    );
    report("amend price 1k (re-queue)", current, prototype, "amends");

    let (current_book, current_orders) = current_amend_book(100, 50);
    let (prototype_book, prototype_entries) = prototype_amend_book(100, 50);
    let current = measure(
        20,
        &(current_book, current_orders),
        |(book, orders)| (book.clone(), orders.clone()),
        |(mut book, orders)| {
            let mut amended = 0usize;
            for order in &orders {
                let new_price = order.price.checked_sub(price_step).expect("price fits");
                if book.amend(order, Some(new_price), None).is_ok() {
                    amended += 1;
                }
            }
            assert_eq!(orders.len(), amended);
            amended
        },
    );
    let prototype = measure(
        20,
        &(prototype_book, prototype_entries),
        |(book, entries)| (book.clone(), entries.clone()),
        |(mut book, entries)| {
            let mut amended = 0usize;
            for &(sequence, price) in &entries {
                let new_price = price.checked_sub(price_step).expect("price fits");
                if book.amend(sequence, Some(new_price), None).is_some() {
                    amended += 1;
                }
            }
            assert_eq!(entries.len(), amended);
            amended
        },
    );
    report(
        "amend price 5k dense (re-queue)",
        current,
        prototype,
        "amends",
    );

    let (current_book, _, _) = current_bbo_book(1_000);
    let (prototype_book, _, _) = prototype_bbo_book(1_000);
    let current = measure(
        1,
        &current_book,
        |book| book.clone(),
        |book| {
            let mut levels = 0usize;
            for _ in 0..100_000 {
                let depth = book.depth(OrderSide::Buy, 10);
                assert_eq!(10, depth.len());
                levels += depth.len();
            }
            levels
        },
    );
    let prototype = measure(
        1,
        &prototype_book,
        |book| book.clone(),
        |book| {
            let mut levels = 0usize;
            for _ in 0..100_000 {
                let depth = book.depth(OrderSide::Buy, 10);
                assert_eq!(10, depth.len());
                levels += depth.len();
            }
            levels
        },
    );
    report(
        "depth 10 levels (100k queries)",
        current,
        prototype,
        "levels",
    );

    let current = measure(
        1,
        &current_book,
        |book| book.clone(),
        |book| {
            for _ in 0..1_000_000 {
                let _ = black_box(book.spread());
            }
            1_000_000
        },
    );
    let prototype = measure(
        1,
        &prototype_book,
        |book| book.clone(),
        |book| {
            for _ in 0..1_000_000 {
                let _ = black_box(book.spread());
            }
            1_000_000
        },
    );
    report(
        "BBO query 1k levels (1M calls)",
        current,
        prototype,
        "queries",
    );

    let (current_book, current_bids, current_asks) = current_bbo_book(1_000);
    let (prototype_book, prototype_bids, prototype_asks) = prototype_bbo_book(1_000);
    let current = measure(
        50,
        &(current_book, current_bids, current_asks),
        |(book, bids, asks)| (book.clone(), bids.clone(), asks.clone()),
        |(mut book, bids, asks)| {
            let mut advances = 0usize;
            for (bid, ask) in bids.iter().zip(asks.iter()) {
                assert!(book.cancel(bid).is_some());
                assert!(book.cancel(ask).is_some());
                let _ = black_box(book.best_bid());
                let _ = black_box(book.best_ask());
                let _ = black_box(book.spread());
                advances += 2;
            }
            assert!(book.best_bid().is_none());
            assert!(book.best_ask().is_none());
            advances
        },
    );
    let prototype = measure(
        50,
        &(prototype_book, prototype_bids, prototype_asks),
        |(book, bids, asks)| (book.clone(), bids.clone(), asks.clone()),
        |(mut book, bids, asks)| {
            let mut advances = 0usize;
            for (&bid, &ask) in bids.iter().zip(asks.iter()) {
                assert!(book.cancel(bid).is_some());
                assert!(book.cancel(ask).is_some());
                let _ = black_box(book.best_bid());
                let _ = black_box(book.best_ask());
                let _ = black_box(book.spread());
                advances += 2;
            }
            assert!(book.best_bid().is_none());
            assert!(book.best_ask().is_none());
            advances
        },
    );
    report(
        "BBO advance 1k levels x 2 sides",
        current,
        prototype,
        "advances",
    );
}
