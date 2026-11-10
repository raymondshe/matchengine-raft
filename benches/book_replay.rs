//! Book-only replay benchmarks: deep sweeps, cancel-at-depth and BBO advancement.
//!
//! Run with `cargo bench --bench book_replay`. This exercises `OrderBook` directly:
//! no HTTP, Raft, WAL, snapshots or market-data feed. Each case builds a pristine
//! book once, clones it outside the timer and measures only the operation under
//! test. Every case reports the historical best-of-5 mean-of-rounds plus warm
//! p50/p99 over all per-round samples, and a cold pass that evicts the caches
//! (64 MiB read-modify-write sweep) between the clone and the timed op, so warm
//! and cold behaviour are reported separately. See `benches/bench_support`.
//!
//! `--check` enforces the loose warm p50/p99 CI bounds from
//! `docs/PERFORMANCE.md`; cold passes stay informational. `--matrix` instead
//! runs the opt-in occupancy scenario matrix (static, normal, wide-swing and
//! flash-crash), reporting average/peak resting orders and levels plus the
//! worst-case median-of-10 throughput with variance.

#[path = "bench_support/mod.rs"]
mod bench_support;
#[path = "bench_support/scenario.rs"]
mod scenario;

use std::hint::black_box;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;

/// Loose CI bounds for the warm p50/p99. They are deliberately at least an
/// order of magnitude above the recorded host baseline so a shared CI runner
/// does not fail on scheduling noise, while a real algorithmic regression does.
#[derive(Clone, Copy)]
struct Bounds {
    p50: Duration,
    p99: Duration,
}

const CHECK_SWEEP_1K: Bounds = Bounds {
    p50: Duration::from_millis(1),
    p99: Duration::from_millis(5),
};
const CHECK_SWEEP_20K: Bounds = Bounds {
    p50: Duration::from_millis(20),
    p99: Duration::from_millis(100),
};
const CHECK_CANCEL: Bounds = Bounds {
    p50: Duration::from_millis(5),
    p99: Duration::from_millis(20),
};
const CHECK_BBO_ADVANCE: Bounds = Bounds {
    p50: Duration::from_millis(2),
    p99: Duration::from_millis(10),
};
const CHECK_BBO_QUERY: Bounds = Bounds {
    p50: Duration::from_millis(50),
    p99: Duration::from_millis(100),
};

static CHECK_FAILED: AtomicBool = AtomicBool::new(false);

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
    check: Option<Bounds>,
) {
    let samples = bench_support::run(bench_support::Config::default(), rounds, base, clone, op);
    let best = samples.warm_best(rounds);
    let rate = samples.units_per_second(best);
    let p50 = samples.warm_p50();
    let p99 = samples.warm_p99();
    println!(
        "{name:38} warm  best {best:>10?} p50 {p50:>10?} p99 {p99:>10?} n={:>4} {rate:>12.0} {unit}/s",
        samples.warm.len(),
    );
    println!(
        "{:38} cold  p50 {:>10?} p99 {:>10?} n={}",
        "",
        samples.cold_p50(),
        samples.cold_p99(),
        samples.cold.len(),
    );

    if let Some(bounds) = check
        && (p50 > bounds.p50 || p99 > bounds.p99)
    {
        eprintln!(
            "CHECK FAIL {name}: warm p50 {p50:?}/{p99:?} exceeds {:?}/{:?}",
            bounds.p50, bounds.p99
        );
        CHECK_FAILED.store(true, Ordering::Relaxed);
    }
}

fn main() {
    let matrix = std::env::args().any(|arg| arg == "--matrix");
    if matrix {
        scenario::run();
        return;
    }
    let check = std::env::args().any(|arg| arg == "--check");
    println!("book-only replay benchmark (no HTTP, Raft, WAL or feed)");
    println!("warm = clone immediately before the timed op; cold = 64 MiB eviction sweep first");
    println!("best = best mean-of-rounds over 5 repeats; p50/p99 = nearest-rank per round");
    if check {
        println!("check mode: warm p50/p99 CI bounds are enforced\n");
    } else {
        println!();
    }

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
        check.then_some(CHECK_SWEEP_1K),
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
        check.then_some(CHECK_SWEEP_20K),
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
        check.then_some(CHECK_CANCEL),
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
        check.then_some(CHECK_BBO_ADVANCE),
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
        check.then_some(CHECK_BBO_QUERY),
    );

    if check {
        if CHECK_FAILED.load(Ordering::Relaxed) {
            std::process::exit(1);
        }
        println!("\nbook replay latency check passed");
    }
}
