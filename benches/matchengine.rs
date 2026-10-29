//! Micro-benchmarks for the order book (`OrderBook::place_order` / `cancel`).
//!
//! Run with `cargo bench --bench matchengine`. No framework: each scenario prints
//! the best median round time out of five repetitions, which is stable enough to
//! compare optimizations on this machine-class.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;

/// A resting limit order helper; `id` stays 0 like `Order::new` does.
fn resting(side: OrderSide, price: f64, volume: f64) -> Order {
    Order::new(side, price, volume, 0)
}

/// Runs `f` `rounds` times five times and prints the fastest per-round time.
fn bench(name: &str, rounds: u32, mut f: impl FnMut()) {
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..rounds {
            f();
        }
        let elapsed = start.elapsed() / rounds;
        if elapsed < best {
            best = elapsed;
        }
    }
    println!("{name:34} best-of-5 per round {best:>10?}");
}

/// Builds a book of `levels * per_level` resting asks above 100.0 and a taker
/// that crosses all of them, so a round is a full sweep.
fn sweep_book(levels: u32, per_level: u32) -> (OrderBook, Order) {
    let mut ob = OrderBook::new();
    for i in 0..levels * per_level {
        let price = 100.0 + f64::from(i / per_level) * 0.01;
        let mut ask = resting(OrderSide::Sell, price, 1.0);
        ask.id = u64::from(i);
        ob.place_order(&mut ask);
    }
    let total = levels * per_level;
    let taker = resting(
        OrderSide::Buy,
        100.0 + f64::from(levels) + 1.0,
        f64::from(total),
    );
    (ob, taker)
}

fn main() {
    // Wide book: many price levels, one order each — exercises tree descents per level.
    let (ob_wide, taker_wide) = sweep_book(100, 10);
    bench("sweep 1k asks / 100 levels", 100, || {
        let mut ob = ob_wide.clone();
        let mut taker = taker_wide;
        assert_eq!(1000, ob.place_order(&mut taker).len());
        assert!(ob.asks.is_empty());
    });

    // Deep book: one price level, many orders — worst case for per-order tree walks.
    let (ob_deep, taker_deep) = sweep_book(1, 20_000);
    bench("sweep 20k asks / 1 level", 20, || {
        let mut ob = ob_deep.clone();
        let mut taker = taker_deep;
        assert_eq!(20_000, ob.place_order(&mut taker).len());
        assert!(ob.asks.is_empty());
    });

    // Resting: no crossing, pure insert path.
    bench("rest 1k bids", 100, || {
        let mut ob = OrderBook::new();
        for i in 0..1000u32 {
            let price = 100.0 - f64::from(i / 10) * 0.01;
            let mut bid = resting(OrderSide::Buy, price, 1.0);
            bid.id = u64::from(i);
            ob.place_order(&mut bid);
        }
        assert_eq!(1000, ob.bids.len());
    });

    // Resting with the market-data tap enabled: the same insert path plus one
    // journal append per order (the matching-thread cost of the feed).
    bench("rest 1k bids + journal", 100, || {
        let mut ob = OrderBook::new();
        ob.enable_depth_journal();
        for i in 0..1000u32 {
            let price = 100.0 - f64::from(i / 10) * 0.01;
            let mut bid = resting(OrderSide::Buy, price, 1.0);
            bid.id = u64::from(i);
            ob.place_order(&mut bid);
        }
        assert_eq!(1000, ob.take_depth_journal().len());
    });

    // Partial fills: many takers nibble at one huge maker.
    bench("partial fills 1k", 100, || {
        let mut ob = OrderBook::new();
        let mut maker = resting(OrderSide::Sell, 100.0, f64::from(u16::MAX));
        ob.place_order(&mut maker);
        for _ in 0..1000u32 {
            let mut taker = resting(OrderSide::Buy, 100.0, 1.0);
            assert_eq!(1, ob.place_order(&mut taker).len());
        }
    });

    // IOC remainders: each taker fills one unit and cancels the other.
    let mut ioc_book = OrderBook::new();
    bench("ioc cancels 1k", 100, || {
        for _ in 0..1000u32 {
            let mut maker = resting(OrderSide::Sell, 100.0, 1.0);
            ioc_book.place_order(&mut maker);
            let mut taker = resting(OrderSide::Buy, 100.0, 2.0);
            taker.tif = matchengine_raft::order_book::TimeInForce::Ioc;
            let outcome = ioc_book.execute_order(&mut taker);
            assert_eq!(1, outcome.fills.len());
            assert_eq!(Some(1.0), outcome.cancelled.map(|v| v.to_f64()));
        }
        assert!(ioc_book.asks.is_empty());
    });

    // Iceberg replenishment: one hidden reserve refilled 1k times.
    bench("iceberg replenish 1k", 100, || {
        let mut ob = OrderBook::new();
        let mut iceberg = resting(OrderSide::Sell, 100.0, 1000.0);
        iceberg.display_volume = 1.0.into();
        ob.place_order(&mut iceberg);
        for _ in 0..1000u32 {
            let mut taker = resting(OrderSide::Buy, 100.0, 1.0);
            assert_eq!(1, ob.place_order(&mut taker).len());
        }
        assert!(ob.asks.is_empty());
    });

    // Amends: same-price reductions keep the sequence and update in place.
    bench("amend 1k", 100, || {
        let mut ob = OrderBook::new();
        let mut orders = Vec::with_capacity(1000);
        for i in 0..1000u32 {
            let mut bid = resting(OrderSide::Buy, 100.0 - f64::from(i / 10) * 0.01, 10.0);
            bid.id = u64::from(i);
            ob.place_order(&mut bid);
            orders.push(bid);
        }
        for order in &orders {
            let amended = ob.amend(order, None, Some(5.0.into())).expect("amendable");
            assert_eq!(order.sequence, amended.order.sequence);
        }
        assert_eq!(1000, ob.bids.len());
    });

    // Cancels: exact-key removals against a resting book.
    bench("cancel 1k", 100, || {
        let mut ob = OrderBook::new();
        let mut orders = Vec::with_capacity(1000);
        for i in 0..1000u32 {
            let mut bid = resting(OrderSide::Buy, 100.0 - f64::from(i / 10) * 0.01, 1.0);
            bid.id = u64::from(i);
            ob.place_order(&mut bid);
            orders.push(bid);
        }
        for order in &orders {
            assert!(ob.cancel(order).is_some());
        }
        assert!(ob.bids.is_empty());
    });
}
