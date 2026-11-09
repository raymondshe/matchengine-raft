//! Realistic-mix replay benchmark.
//!
//! Run with `cargo bench --bench replay_mix`. The fixture is a deterministic
//! 100,000-operation stream: limit adds, icebergs, parked stops, cancels,
//! amendments, and market takers, with maker/taker fees on every order. It
//! exercises `OrderBook` directly (no HTTP, Raft, WAL, snapshots or feed) and
//! reports whole-replay percentiles plus per-operation percentiles by kind.
//!
//! `--check` enforces the loose whole-replay warm p50/p99 CI bounds from
//! `docs/PERFORMANCE.md`; cold passes stay informational.

#[path = "bench_support/mod.rs"]
mod bench_support;

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchResult;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::Outcome;

const OPS: usize = 100_000;
const MAKER_FEE: f64 = 0.0002;
const TAKER_FEE: f64 = 0.0005;
/// Loose CI bounds for the whole warm replay. The recorded host baseline is
/// ~107 ms p50 / ~109 ms p99, so these only catch an order-of-magnitude
/// regression on a noisy shared runner.
const CHECK_P50: Duration = Duration::from_secs(1);
const CHECK_P99: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
enum Op {
    Add {
        side: OrderSide,
        price: Fixed,
        volume: Fixed,
        iceberg: bool,
    },
    Stop {
        side: OrderSide,
        offset: Fixed,
        volume: Fixed,
    },
    Cancel,
    Amend,
    Taker {
        side: OrderSide,
        volume: Fixed,
    },
}

#[derive(Clone, Debug)]
struct Fixture {
    ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Add = 0,
    Iceberg = 1,
    Stop = 2,
    Cancel = 3,
    Amend = 4,
    Taker = 5,
}

const KINDS: [Kind; 6] = [
    Kind::Add,
    Kind::Iceberg,
    Kind::Stop,
    Kind::Cancel,
    Kind::Amend,
    Kind::Taker,
];

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Add => "limit add",
            Kind::Iceberg => "iceberg add",
            Kind::Stop => "stop add",
            Kind::Cancel => "cancel",
            Kind::Amend => "amend",
            Kind::Taker => "taker",
        }
    }
}

#[derive(Default)]
struct Timings {
    all: Vec<Duration>,
    by_kind: [Vec<Duration>; 6],
}

#[derive(Default)]
struct Stats {
    limit_rested: usize,
    iceberg_rested: usize,
    stops_parked: usize,
    rejected: usize,
    cancelled: usize,
    amended: usize,
    taker_fills: usize,
    stop_triggers: usize,
}

impl Timings {
    fn record(&mut self, kind: Kind, elapsed: Duration) {
        self.all.push(elapsed);
        self.by_kind[kind as usize].push(elapsed);
    }
}

/// Small deterministic PRNG; the benchmark fixture must be identical across
/// runs and machines.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn price(base: f64, ticks: u64) -> Fixed {
    Fixed::from(base + ticks as f64 * 0.01)
}

fn add_op(rng: &mut Lcg) -> Op {
    let side = if rng.below(2) == 0 {
        OrderSide::Buy
    } else {
        OrderSide::Sell
    };
    let touch = rng.below(2) == 0;
    let price = match (side, touch) {
        (OrderSide::Buy, true) => price(99.00, rng.below(100)),
        (OrderSide::Sell, true) => price(100.01, rng.below(100)),
        (OrderSide::Buy, false) => price(80.00, rng.below(1_500)),
        (OrderSide::Sell, false) => price(105.01, rng.below(1_500)),
    };
    let volume = Fixed::from(1.0 + rng.below(100) as f64 * 0.1);
    Op::Add {
        side,
        price,
        volume,
        iceberg: false,
    }
}

fn fixture() -> Fixture {
    let mut rng = Lcg::new(0xA11C_E5E5_D00D_1234);
    let mut ops = Vec::with_capacity(OPS);

    // Seed both touch and background liquidity before the first taker.
    for _ in 0..1_000 {
        ops.push(add_op(&mut rng));
    }
    // Establish a non-zero last trade price so stop offsets are valid.
    ops.push(Op::Taker {
        side: OrderSide::Buy,
        volume: Fixed::from(0.1),
    });

    while ops.len() < OPS {
        let bucket = rng.below(100);
        let op = match bucket {
            0..=54 => add_op(&mut rng),
            55..=64 => {
                let mut op = add_op(&mut rng);
                if let Op::Add { iceberg, .. } = &mut op {
                    *iceberg = true;
                }
                op
            }
            65..=69 => {
                let side = if rng.below(2) == 0 {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                };
                Op::Stop {
                    side,
                    offset: Fixed::from(0.5 + rng.below(150) as f64 * 0.01),
                    volume: Fixed::from(1.0 + rng.below(50) as f64 * 0.1),
                }
            }
            70..=79 => Op::Cancel,
            80..=89 => Op::Amend,
            _ => Op::Taker {
                side: if rng.below(2) == 0 {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                },
                volume: Fixed::from(0.1 + rng.below(10) as f64 * 0.1),
            },
        };
        ops.push(op);
    }

    Fixture { ops }
}

fn with_fees(order: &mut Order) {
    order.maker_fee_rate = Fixed::from(MAKER_FEE);
    order.taker_fee_rate = Fixed::from(TAKER_FEE);
}

fn apply_fill(orders: &mut HashMap<u64, Order>, fill: &MatchResult) {
    let sequence = fill.maker.sequence;
    let Some(order) = orders.get_mut(&sequence) else {
        return;
    };
    if fill.matches < fill.maker.volume {
        // Partially filled makers are shrunk in place and keep their sequence.
        order.volume = fill.maker.volume - fill.matches;
        return;
    }
    if fill.maker.hidden_volume > Fixed::ZERO {
        // An iceberg refills from its hidden reserve instead of disappearing.
        let draw = if fill.maker.display_volume > Fixed::ZERO {
            fill.maker.display_volume.min(fill.maker.hidden_volume)
        } else {
            fill.maker.hidden_volume
        };
        order.volume = draw;
        order.hidden_volume = fill.maker.hidden_volume - draw;
    } else {
        orders.remove(&sequence);
    }
}

fn remove_filled(orders: &mut HashMap<u64, Order>, outcome: &Outcome) {
    for fill in &outcome.fills {
        apply_fill(orders, fill);
    }
    for trigger in &outcome.triggers {
        orders.remove(&trigger.stop.sequence);
        for fill in &trigger.fills {
            apply_fill(orders, fill);
        }
    }
}

fn pick_key(keys: &mut Vec<u64>, cursor: usize) -> Option<u64> {
    if keys.is_empty() {
        None
    } else {
        let index = cursor % keys.len();
        Some(keys.swap_remove(index))
    }
}

fn pick_amendable(
    keys: &mut Vec<u64>,
    orders: &HashMap<u64, Order>,
    cursor: usize,
) -> Option<Order> {
    let mut attempts = 0;
    while !keys.is_empty() && attempts < 32 {
        let index = (cursor + attempts) % keys.len();
        let key = keys.swap_remove(index);
        if let Some(order) = orders.get(&key) {
            if order.display_volume == Fixed::ZERO && !order.is_stop() {
                return Some(*order);
            }
            keys.push(key);
        }
        attempts += 1;
    }
    None
}

fn replay(
    fixture: &Fixture,
    mut timings: Option<&mut Timings>,
    mut stats: Option<&mut Stats>,
) -> usize {
    let mut book = OrderBook::new();
    let mut orders: HashMap<u64, Order> = HashMap::new();
    let mut keys: Vec<u64> = Vec::new();
    let mut next_id = 1u64;

    for (cursor, op) in fixture.ops.iter().enumerate() {
        let start = Instant::now();
        let kind = match *op {
            Op::Add {
                side,
                price,
                volume,
                iceberg,
            } => {
                let mut order = Order::new(side, price, volume, 0);
                order.id = next_id;
                next_id += 1;
                with_fees(&mut order);
                if iceberg {
                    order.display_volume = volume * Fixed::from(0.1);
                }
                let outcome = book.execute_order(&mut order);
                if outcome.resting.is_some() {
                    orders.insert(order.sequence, order);
                    keys.push(order.sequence);
                    if let Some(stats) = &mut stats {
                        if iceberg {
                            stats.iceberg_rested += 1;
                        } else {
                            stats.limit_rested += 1;
                        }
                    }
                } else if let Some(stats) = &mut stats
                    && outcome.rejected.is_some()
                {
                    stats.rejected += 1;
                }
                if iceberg { Kind::Iceberg } else { Kind::Add }
            }
            Op::Stop {
                side,
                offset,
                volume,
            } => {
                let last = book.last_trade_price.unwrap_or_else(|| Fixed::from(100.0));
                let stop_price = match side {
                    OrderSide::Buy => last + offset,
                    OrderSide::Sell => last - offset,
                };
                let limit_price = match side {
                    OrderSide::Buy => stop_price + Fixed::from(1.0),
                    OrderSide::Sell => stop_price - Fixed::from(1.0),
                };
                let mut order = Order::new(side, limit_price, volume, 0);
                order.id = next_id;
                next_id += 1;
                order.stop_price = stop_price;
                with_fees(&mut order);
                let outcome = book.execute_order(&mut order);
                if outcome.parked {
                    orders.insert(order.sequence, order);
                    keys.push(order.sequence);
                    if let Some(stats) = &mut stats {
                        stats.stops_parked += 1;
                    }
                } else if let Some(stats) = &mut stats
                    && outcome.rejected.is_some()
                {
                    stats.rejected += 1;
                }
                Kind::Stop
            }
            Op::Cancel => {
                if let Some(key) = pick_key(&mut keys, cursor)
                    && let Some(order) = orders.remove(&key)
                    && book.cancel(&order).is_some()
                    && let Some(stats) = &mut stats
                {
                    stats.cancelled += 1;
                }
                Kind::Cancel
            }
            Op::Amend => {
                if let Some(order) = pick_amendable(&mut keys, &orders, cursor) {
                    let new_volume = order.volume * Fixed::from(0.9);
                    if let Ok(amendment) = book.amend(&order, None, Some(new_volume)) {
                        orders.remove(&order.sequence);
                        orders.insert(amendment.order.sequence, amendment.order);
                        keys.push(amendment.order.sequence);
                        if let Some(stats) = &mut stats {
                            stats.amended += 1;
                        }
                    } else {
                        keys.push(order.sequence);
                    }
                }
                Kind::Amend
            }
            Op::Taker { side, volume } => {
                let mut order = Order::new(side, 0.0, volume, 0);
                order.id = next_id;
                next_id += 1;
                order.live = OrderType::Market;
                with_fees(&mut order);
                let outcome = book.execute_order(&mut order);
                remove_filled(&mut orders, &outcome);
                if let Some(stats) = &mut stats {
                    stats.taker_fills += outcome.fills.len();
                    stats.stop_triggers += outcome.triggers.len();
                }
                Kind::Taker
            }
        };
        if let Some(timings) = &mut timings {
            timings.record(kind, start.elapsed());
        }
    }
    black_box(&book);
    black_box(&orders);
    fixture.ops.len()
}

fn count_kind(fixture: &Fixture, kind: Kind) -> usize {
    fixture
        .ops
        .iter()
        .filter(|op| {
            matches!(
                (op, kind),
                (Op::Add { iceberg: false, .. }, Kind::Add)
                    | (Op::Add { iceberg: true, .. }, Kind::Iceberg)
                    | (Op::Stop { .. }, Kind::Stop)
                    | (Op::Cancel, Kind::Cancel)
                    | (Op::Amend, Kind::Amend)
                    | (Op::Taker { .. }, Kind::Taker)
            )
        })
        .count()
}

fn print_percentiles(label: &str, samples: &[Duration]) {
    if samples.is_empty() {
        println!("{label:14} n=0");
        return;
    }
    let p50 = bench_support::percentile(samples, 0.50);
    let p99 = bench_support::percentile(samples, 0.99);
    let max = *samples.iter().max().expect("non-empty");
    println!(
        "{label:14} n={:>7} p50 {p50:>10?} p99 {p99:>10?} max {max:>10?}",
        samples.len()
    );
}

fn main() {
    let check = std::env::args().any(|arg| arg == "--check");
    let fixture = fixture();
    println!("realistic-mix replay benchmark (100k operations, OrderBook only)");
    println!(
        "mix: limit add {}, iceberg {}, stop {}, cancel {}, amend {}, taker {}",
        count_kind(&fixture, Kind::Add),
        count_kind(&fixture, Kind::Iceberg),
        count_kind(&fixture, Kind::Stop),
        count_kind(&fixture, Kind::Cancel),
        count_kind(&fixture, Kind::Amend),
        count_kind(&fixture, Kind::Taker),
    );
    println!(
        "fees: maker {MAKER_FEE}, taker {TAKER_FEE}; 20 warm + 20 interleaved cold rounds; warm = clone immediately before replay, cold = 64 MiB eviction first"
    );
    if check {
        println!("check mode: whole-replay warm p50/p99 CI bounds are enforced\n");
    } else {
        println!();
    }

    let config = bench_support::Config {
        repeats: 20,
        cold_rounds: 20,
    };
    let samples = bench_support::run(
        config,
        1,
        &fixture,
        |fixture| fixture.clone(),
        |fixture| black_box(replay(fixture, None, None)),
    );
    let best = samples.warm_best(1);
    let p50 = samples.warm_p50();
    let p99 = samples.warm_p99();
    println!(
        "whole replay  warm best {best:>10?} p50 {p50:>10?} p99 {p99:>10?} {:>10.0} ops/s",
        samples.units_per_second(best),
    );
    println!(
        "{:14} cold p50 {:>10?} p99 {:>10?} n={}",
        "",
        samples.cold_p50(),
        samples.cold_p99(),
        samples.cold.len(),
    );

    if check && (p50 > CHECK_P50 || p99 > CHECK_P99) {
        eprintln!(
            "CHECK FAIL whole replay: warm p50 {p50:?}/{p99:?} exceeds {CHECK_P50:?}/{CHECK_P99:?}"
        );
        std::process::exit(1);
    }

    let mut stats = Stats::default();
    replay(&fixture, None, Some(&mut stats));
    println!(
        "outcomes: limit rested {}, iceberg rested {}, stops parked {}, rejected {}, cancelled {}, amended {}, taker fills {}, stop triggers {}",
        stats.limit_rested,
        stats.iceberg_rested,
        stats.stops_parked,
        stats.rejected,
        stats.cancelled,
        stats.amended,
        stats.taker_fills,
        stats.stop_triggers,
    );

    let mut timings = Timings::default();
    replay(&fixture, Some(&mut timings), None);
    println!("\nper-operation latency:");
    print_percentiles("all", &timings.all);
    for kind in KINDS {
        print_percentiles(kind.label(), &timings.by_kind[kind as usize]);
    }

    if check {
        println!("\nreplay-mix latency check passed");
    }
}
