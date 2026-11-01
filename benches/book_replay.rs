//! Book-only replay benchmarks: deep sweeps, cancel-at-depth and BBO advancement.
//!
//! Run with `cargo bench --bench book_replay`. This exercises `OrderBook` directly:
//! no HTTP, Raft, WAL, snapshots or market-data feed. Each case builds a pristine
//! book once, clones it outside the timer and measures only the operation under
//! test, best-of-5 per round.

use std::hint::black_box;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;

fn resting(side: OrderSide, price: f64, volume: f64) -> Order {
    Order::new(side, price, volume, 0)
}

/// Builds `levels * per_level` asks above 100.0 and a taker that crosses all of
/// them.
fn sweep_book(levels: u32, per_level: u32) -> (OrderBook, Order) {
    let mut book = OrderBook::new();
    for i in 0..levels * per_level {
        let price = 100.0 + f64::from(i / per_level) * 0.01;
        let mut ask = resting(OrderSide::Sell, price, 1.0);
        ask.id = u64::from(i);
        book.place_order(&mut ask);
    }
    let total = f64::from(levels * per_level);
    let taker = resting(OrderSide::Buy, 100.0 + f64::from(levels) + 1.0, total);
    (book, taker)
}

/// Builds a bid book and returns the orders at the deeper half of the price
/// range. The returned orders are the cancel workload.
fn cancel_book(levels: u32, per_level: u32) -> (OrderBook, Vec<Order>) {
    let mut book = OrderBook::new();
    let mut deep = Vec::new();
    for level in 0..levels {
        for offset in 0..per_level {
            let price = 100.0 - f64::from(level) * 0.01;
            let mut bid = resting(OrderSide::Buy, price, 1.0);
            bid.id = u64::from(level * per_level + offset);
            book.place_order(&mut bid);
            if level >= levels / 2 {
                deep.push(bid);
            }
        }
    }
    (book, deep)
}

/// Builds one order per price level on both sides. Cancelling the best bid and
/// ask advances the BBO to the next level.
fn bbo_book(levels: u32) -> (OrderBook, Vec<Order>, Vec<Order>) {
    let mut book = OrderBook::new();
    let mut bids = Vec::with_capacity(levels as usize);
    let mut asks = Vec::with_capacity(levels as usize);
    for level in 0..levels {
        let mut bid = resting(OrderSide::Buy, 100.0 - f64::from(level) * 0.01, 1.0);
        bid.id = u64::from(level);
        book.place_order(&mut bid);
        bids.push(bid);

        let mut ask = resting(OrderSide::Sell, 101.0 + f64::from(level) * 0.01, 1.0);
        ask.id = u64::from(levels + level);
        book.place_order(&mut ask);
        asks.push(ask);
    }
    (book, bids, asks)
}

/// Clones `base` outside the timer, runs `op` and reports the best per-round
/// average across five repetitions.
fn bench_clone<T>(
    name: &str,
    rounds: u32,
    base: &T,
    mut clone: impl FnMut(&T) -> T,
    mut op: impl FnMut(T) -> usize,
    unit: &str,
) {
    let mut best = Duration::MAX;
    let mut best_units = 0usize;

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

    let throughput = best_units as f64 / best.as_secs_f64();
    println!("{name:44} best-of-5 {best:>10?}  {throughput:>12.0} {unit}/s");
}

fn main() {
    println!("book-only replay benchmark (no HTTP, Raft, WAL or feed)");

    let (wide, wide_taker) = sweep_book(100, 10);
    bench_clone(
        "deep sweep 1k asks / 100 levels",
        100,
        &(wide, wide_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let fills = book.place_order(&mut taker).len();
            assert_eq!(1_000, fills);
            assert!(book.asks.is_empty());
            fills
        },
        "fills",
    );

    let (deep, deep_taker) = sweep_book(1, 20_000);
    bench_clone(
        "deep sweep 20k asks / 1 level",
        20,
        &(deep, deep_taker),
        |(book, taker)| (book.clone(), *taker),
        |(mut book, mut taker)| {
            let fills = book.place_order(&mut taker).len();
            assert_eq!(20_000, fills);
            assert!(book.asks.is_empty());
            fills
        },
        "fills",
    );

    let (cancelled_book, deep_orders) = cancel_book(100, 100);
    bench_clone(
        "cancel 5k at depth / 50 levels",
        50,
        &(cancelled_book, deep_orders),
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
        "cancels",
    );

    let (advancing_book, bids, asks) = bbo_book(1_000);
    bench_clone(
        "BBO advance 1k levels x 2 sides",
        50,
        &(advancing_book, bids, asks),
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
        "advances",
    );

    let (static_book, _, _) = bbo_book(1_000);
    bench_clone(
        "BBO query 1k levels (1M calls)",
        1,
        &static_book,
        |book| book.clone(),
        |book| {
            for _ in 0..1_000_000 {
                let _ = black_box(book.spread());
            }
            1_000_000
        },
        "queries",
    );
}
