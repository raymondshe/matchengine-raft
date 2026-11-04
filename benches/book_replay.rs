//! Book-only replay benchmarks: deep sweeps, cancel-at-depth and BBO advancement.
//!
//! Run with `cargo bench --bench book_replay`. This exercises `OrderBook` directly:
//! no HTTP, Raft, WAL, snapshots or market-data feed. Each case builds a pristine
//! book once, clones it outside the timer and measures only the operation under
//! test. Every case reports the historical best-of-5 mean-of-rounds plus warm
//! p50/p99 over all per-round samples, and a cold pass that evicts the caches
//! (64 MiB read-modify-write sweep) between the clone and the timed op, so warm
//! and cold behaviour are reported separately. See `benches/bench_support`.

#[path = "bench_support/mod.rs"]
mod bench_support;

use std::hint::black_box;

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

/// Clones `base` outside the timer, runs `op` and reports the best-of-5
/// mean-of-rounds, warm p50/p99 over all per-round samples, and a cold pass
/// with the caches evicted between clone and timed op.
fn bench_clone<T>(
    name: &str,
    rounds: u32,
    base: &T,
    clone: impl FnMut(&T) -> T,
    op: impl FnMut(&mut T) -> usize,
    unit: &str,
) {
    let samples = bench_support::run(bench_support::Config::default(), rounds, base, clone, op);
    let best = samples.warm_best(rounds);
    let rate = samples.units_per_second(best);
    println!(
        "{name:38} warm  best {best:>10?} p50 {:>10?} p99 {:>10?} n={:>4} {rate:>12.0} {unit}/s",
        samples.warm_p50(),
        samples.warm_p99(),
        samples.warm.len(),
    );
    println!(
        "{:38} cold  p50 {:>10?} p99 {:>10?} n={}",
        "",
        samples.cold_p50(),
        samples.cold_p99(),
        samples.cold.len(),
    );
}

fn main() {
    println!("book-only replay benchmark (no HTTP, Raft, WAL or feed)");
    println!("warm = clone immediately before the timed op; cold = 64 MiB eviction sweep first");
    println!("best = best mean-of-rounds over 5 repeats; p50/p99 = nearest-rank per round\n");

    let (wide, wide_taker) = sweep_book(100, 10);
    bench_clone(
        "deep sweep 1k asks / 100 levels",
        100,
        &(wide, wide_taker),
        |(book, taker)| (book.clone(), *taker),
        |(book, taker)| {
            let fills = book.place_order(taker).len();
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
        |(book, taker)| {
            let fills = book.place_order(taker).len();
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
        |(book, orders)| {
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
        |(book, bids, asks)| {
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
        "BBO query 1k levels (1M calls/round)",
        20,
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
