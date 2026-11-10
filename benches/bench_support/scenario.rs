//! Occupancy scenario matrix for the book-only benchmark.
//!
//! The FlashOne survey found that a matcher's price-ladder cost is driven by
//! the standing book, not only by message dispatch: its seed-23 averages range
//! from ~20,900 orders / 60 levels in `static` to ~240 orders / 210 levels in
//! `flash-crash`. This module builds a book with each shape and replays a
//! deterministic, steady-state mix over it, so the same `OrderBook` is measured
//! under four occupancy/price-span regimes.
//!
//! The initial shapes are calibrated to the survey's average standing book.
//! The mix is a book-only stress mix, not the survey's 2M-message flow:
//! 45% add, 43% cancel, 10% amend and 2% taker keeps the order count roughly
//! constant while exercising matching, cancel, amend and price-level removal.
//! Each scenario runs 10 whole-replay trials; the table reports the median,
//! population standard deviation and coefficient of variation, then names the
//! worst-case scenario. All prices are integer ticks; there are no floats in
//! the generated book.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchResult;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::Outcome;

use super::bench_support;

/// One tick is $0.005, matching the survey; one cent is two ticks.
const TICK_RAW: i128 = 5_000_000_000_000_000;
/// The synthetic book is centred on $100.00.
const MID_TICKS: i64 = 20_000;
/// Whole-replay operations per trial.
const OPS: usize = 50_000;
/// Ten trials as required by the measurement envelope.
const TRIALS: usize = 10;
/// Occupancy is sampled every this many operations, plus the first and last.
const SAMPLE_EVERY: usize = 64;

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Static,
    Normal,
    WideSwing,
    FlashCrash,
}

impl Scenario {
    const ALL: [Self; 4] = [
        Self::Static,
        Self::Normal,
        Self::WideSwing,
        Self::FlashCrash,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Normal => "normal",
            Self::WideSwing => "wide-swing",
            Self::FlashCrash => "flash-crash",
        }
    }

    /// Average standing orders from the survey's canonical seed.
    fn orders(self) -> usize {
        match self {
            Self::Static => 20_900,
            Self::Normal => 2_700,
            Self::WideSwing => 1_300,
            Self::FlashCrash => 240,
        }
    }

    /// Average distinct price levels from the survey's canonical seed.
    fn levels(self) -> usize {
        match self {
            Self::Static => 60,
            Self::Normal => 290,
            Self::WideSwing => 830,
            Self::FlashCrash => 210,
        }
    }

    /// Tick spacing between adjacent levels. The static book is dense; the
    /// volatile books span a wider price range with one or two orders per level.
    fn tick_step(self) -> i64 {
        match self {
            Self::Static => 1,
            Self::Normal => 2,
            Self::WideSwing => 10,
            Self::FlashCrash => 5,
        }
    }

    fn seed(self) -> u64 {
        match self {
            Self::Static => 0x5A17_C0DE_0001,
            Self::Normal => 0x5A17_C0DE_0002,
            Self::WideSwing => 0x5A17_C0DE_0003,
            Self::FlashCrash => 0x5A17_C0DE_0004,
        }
    }
}

fn price_at(ticks: i64) -> Fixed {
    Fixed::from_raw(i128::from(ticks) * TICK_RAW)
}

fn price_for(side: OrderSide, level: i64, tick_step: i64) -> Fixed {
    let offset = level * tick_step;
    match side {
        OrderSide::Buy => price_at(MID_TICKS - offset),
        OrderSide::Sell => price_at(MID_TICKS + offset),
    }
}

/// The pristine book and its resting orders, built once per scenario and
/// cloned by the benchmark harness outside the timer.
struct BookShape {
    book: OrderBook,
    orders: Vec<Order>,
}

fn build_shape(scenario: Scenario) -> BookShape {
    let mut book = OrderBook::new();
    let side_levels = (scenario.levels() / 2).max(1);
    let per_side = scenario.orders() / 2;
    let base = per_side / side_levels;
    let remainder = per_side % side_levels;
    let tick_step = scenario.tick_step();
    let mut orders = Vec::with_capacity(scenario.orders());
    let mut id = 1u64;

    for side in [OrderSide::Buy, OrderSide::Sell] {
        for level in 0..side_levels {
            let count = base + usize::from(level < remainder);
            let price = price_for(side, level as i64 + 1, tick_step);
            for _ in 0..count {
                let mut order = Order::new(side, price, Fixed::from_units(1), 0);
                order.id = id;
                id += 1;
                let outcome = book.execute_order(&mut order);
                assert_eq!(None, outcome.rejected, "the seeded book must be valid");
                assert_eq!(
                    Some(Fixed::from_units(1)),
                    outcome.resting,
                    "the seeded book must rest every order"
                );
                orders.push(order);
            }
        }
    }

    BookShape { book, orders }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Add { side: OrderSide, price: Fixed },
    Cancel,
    Amend,
    Taker { side: OrderSide, volume: Fixed },
}

#[derive(Clone, Debug)]
struct Fixture {
    ops: Vec<Op>,
}

/// Small deterministic PRNG. The fixture must be identical on every host.
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

fn make_fixture(scenario: Scenario) -> Fixture {
    let mut rng = Lcg::new(scenario.seed());
    let side_levels = (scenario.levels() / 2).max(1) as u64;
    let tick_step = scenario.tick_step();
    let mut ops = Vec::with_capacity(OPS);

    for _ in 0..OPS {
        match rng.below(100) {
            0..=44 => {
                let side = if rng.below(2) == 0 {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                };
                let level = rng.below(side_levels) as i64 + 1;
                ops.push(Op::Add {
                    side,
                    price: price_for(side, level, tick_step),
                });
            }
            45..=87 => ops.push(Op::Cancel),
            88..=97 => ops.push(Op::Amend),
            _ => ops.push(Op::Taker {
                side: if rng.below(2) == 0 {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                },
                volume: Fixed::from_units(1),
            }),
        }
    }

    Fixture { ops }
}

/// Book state that is cloned outside the timer for every trial.
#[derive(Clone)]
struct ReplayState {
    book: OrderBook,
    orders: HashMap<u64, Order>,
    keys: Vec<u64>,
}

impl ReplayState {
    fn new(shape: &BookShape) -> Self {
        let mut orders = HashMap::with_capacity(shape.orders.len());
        let mut keys = Vec::with_capacity(shape.orders.len());
        for order in &shape.orders {
            orders.insert(order.sequence, *order);
            keys.push(order.sequence);
        }
        Self {
            book: shape.book.clone(),
            orders,
            keys,
        }
    }
}

#[derive(Default)]
struct Stats {
    add_rested: usize,
    rejected: usize,
    cancelled: usize,
    amended: usize,
    takers: usize,
    fills: usize,
}

#[derive(Default)]
struct Occupancy {
    samples: u64,
    orders_sum: u128,
    levels_sum: u128,
    initial_orders: usize,
    initial_levels: usize,
    peak_orders: usize,
    peak_levels: usize,
}

impl Occupancy {
    fn observe(&mut self, book: &OrderBook) {
        let orders = book.bids.len() + book.asks.len();
        let levels = count_levels(book.bids.values()) + count_levels(book.asks.values());
        if self.samples == 0 {
            self.initial_orders = orders;
            self.initial_levels = levels;
        }
        self.samples += 1;
        self.orders_sum += orders as u128;
        self.levels_sum += levels as u128;
        self.peak_orders = self.peak_orders.max(orders);
        self.peak_levels = self.peak_levels.max(levels);
    }

    fn average_orders(&self) -> f64 {
        self.orders_sum as f64 / self.samples as f64
    }

    fn average_levels(&self) -> f64 {
        self.levels_sum as f64 / self.samples as f64
    }
}

fn count_levels<'a>(orders: impl Iterator<Item = &'a Order>) -> usize {
    let mut levels = 0usize;
    let mut previous: Option<Fixed> = None;
    for order in orders {
        if previous != Some(order.price) {
            levels += 1;
            previous = Some(order.price);
        }
    }
    levels
}

fn pick_key(keys: &mut Vec<u64>, cursor: usize) -> Option<u64> {
    if keys.is_empty() {
        None
    } else {
        Some(keys.swap_remove(cursor % keys.len()))
    }
}

fn pick_amendable(
    keys: &mut Vec<u64>,
    orders: &HashMap<u64, Order>,
    cursor: usize,
) -> Option<Order> {
    for attempt in 0..32 {
        if keys.is_empty() {
            return None;
        }
        let index = (cursor + attempt) % keys.len();
        let key = keys.swap_remove(index);
        if let Some(order) = orders.get(&key) {
            return Some(*order);
        }
    }
    None
}

fn remove_filled(orders: &mut HashMap<u64, Order>, fills: &[MatchResult]) {
    for fill in fills {
        let sequence = fill.maker.sequence;
        if fill.matches >= fill.maker.volume {
            orders.remove(&sequence);
        } else if let Some(order) = orders.get_mut(&sequence) {
            order.volume = fill.maker.volume - fill.matches;
        }
    }
}

fn replay(
    state: &mut ReplayState,
    fixture: &Fixture,
    mut occupancy: Option<&mut Occupancy>,
    mut stats: Option<&mut Stats>,
) -> usize {
    if let Some(occupancy) = &mut occupancy {
        occupancy.observe(&state.book);
    }

    for (cursor, op) in fixture.ops.iter().enumerate() {
        match *op {
            Op::Add { side, price } => {
                let mut order = Order::new(side, price, Fixed::from_units(1), 0);
                let outcome = state.book.execute_order(&mut order);
                remove_filled(&mut state.orders, &outcome.fills);
                if let Some(stats) = &mut stats {
                    stats.fills += outcome.fills.len();
                    if outcome.rejected.is_some() {
                        stats.rejected += 1;
                    }
                }
                if outcome.resting.is_some() {
                    state.orders.insert(order.sequence, order);
                    state.keys.push(order.sequence);
                    if let Some(stats) = &mut stats {
                        stats.add_rested += 1;
                    }
                }
            }
            Op::Cancel => {
                if let Some(key) = pick_key(&mut state.keys, cursor)
                    && let Some(order) = state.orders.remove(&key)
                    && state.book.cancel(&order).is_some()
                    && let Some(stats) = &mut stats
                {
                    stats.cancelled += 1;
                }
            }
            Op::Amend => {
                if let Some(order) = pick_amendable(&mut state.keys, &state.orders, cursor) {
                    let new_volume = if order.volume > Fixed::from_units(1) {
                        order.volume - Fixed::from_units(1)
                    } else {
                        order.volume + Fixed::from_units(1)
                    };
                    match state.book.amend(&order, None, Some(new_volume)) {
                        Ok(amendment) => {
                            state.orders.remove(&order.sequence);
                            state
                                .orders
                                .insert(amendment.order.sequence, amendment.order);
                            state.keys.push(amendment.order.sequence);
                            if let Some(stats) = &mut stats {
                                stats.amended += 1;
                            }
                        }
                        Err(_) => state.keys.push(order.sequence),
                    }
                }
            }
            Op::Taker { side, volume } => {
                let mut order = Order::new(side, 0.0, volume, 0);
                order.live = OrderType::Market;
                let outcome: Outcome = state.book.execute_order(&mut order);
                remove_filled(&mut state.orders, &outcome.fills);
                if let Some(stats) = &mut stats {
                    stats.takers += 1;
                    stats.fills += outcome.fills.len();
                }
            }
        }

        if (cursor + 1) % SAMPLE_EVERY == 0
            && let Some(occupancy) = &mut occupancy
        {
            occupancy.observe(&state.book);
        }
    }

    if let Some(occupancy) = &mut occupancy {
        occupancy.observe(&state.book);
    }
    black_box(&state.book);
    fixture.ops.len()
}

fn stddev(samples: &[Duration]) -> Duration {
    if samples.len() < 2 {
        return Duration::ZERO;
    }
    let mean = samples
        .iter()
        .map(|sample| sample.as_secs_f64())
        .sum::<f64>()
        / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample.as_secs_f64() - mean;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    Duration::from_secs_f64(variance.sqrt())
}

struct Row {
    scenario: Scenario,
    occupancy: Occupancy,
    stats: Stats,
    median: Duration,
    deviation: Duration,
    ops_per_second: f64,
}

pub fn run() {
    println!("book occupancy scenario matrix (OrderBook only; no HTTP, Raft, WAL or feed)");
    println!(
        "initial shapes: FlashOne seed-23 averages (static 20.9k/60, normal 2.7k/290, wide 1.3k/830, flash 240/210 orders/levels)"
    );
    println!(
        "workload: {OPS} ops/trial, 45% add / 43% cancel / 10% amend / 2% taker, one tick = $0.005; {TRIALS} whole-replay trials"
    );
    println!(
        "occupancy sampled every {SAMPLE_EVERY} ops; timing excludes clone, occupancy sampling and fixture generation"
    );
    println!();

    let mut rows = Vec::with_capacity(Scenario::ALL.len());
    for scenario in Scenario::ALL {
        let shape = build_shape(scenario);
        let fixture = make_fixture(scenario);
        let op_count = fixture.ops.len();

        let mut occupancy = Occupancy::default();
        let mut stats = Stats::default();
        let mut state = ReplayState::new(&shape);
        replay(&mut state, &fixture, Some(&mut occupancy), Some(&mut stats));

        let base = (ReplayState::new(&shape), fixture);
        let samples = bench_support::run(
            bench_support::Config {
                repeats: TRIALS,
                cold_rounds: 1,
            },
            1,
            &base,
            |state| (state.0.clone(), state.1.clone()),
            |state| replay(&mut state.0, &state.1, None, None),
        );
        let median = bench_support::percentile(&samples.warm, 0.50);
        let deviation = stddev(&samples.warm);
        let ops_per_second = op_count as f64 / median.as_secs_f64();

        println!(
            "{:<12} init {:>6}/{:>4}  avg {:>8.0}/{:>6.1}  peak {:>6}/{:>4}  fills {:>6}",
            scenario.name(),
            occupancy.initial_orders,
            occupancy.initial_levels,
            occupancy.average_orders(),
            occupancy.average_levels(),
            occupancy.peak_orders,
            occupancy.peak_levels,
            stats.fills,
        );
        rows.push(Row {
            scenario,
            occupancy,
            stats,
            median,
            deviation,
            ops_per_second,
        });
    }

    println!();
    println!(
        "{:<12} {:>10} {:>9} {:>8} {:>8} {:>9} {:>9}",
        "scenario", "adds", "cancels", "amends", "takers", "fills", "rejected"
    );
    for row in &rows {
        println!(
            "{:<12} {:>10} {:>9} {:>8} {:>8} {:>9} {:>9}",
            row.scenario.name(),
            row.stats.add_rested,
            row.stats.cancelled,
            row.stats.amended,
            row.stats.takers,
            row.stats.fills,
            row.stats.rejected,
        );
    }

    println!();
    println!(
        "{:<12} {:>12} {:>12} {:>8} {:>14}",
        "scenario", "median", "stddev", "CV%", "median M ops/s"
    );
    for row in &rows {
        let cv = row.deviation.as_secs_f64() / row.median.as_secs_f64() * 100.0;
        println!(
            "{:<12} {:>12?} {:>12?} {:>8.1} {:>14.3}",
            row.scenario.name(),
            row.median,
            row.deviation,
            cv,
            row.ops_per_second / 1_000_000.0,
        );
    }

    let worst = rows
        .iter()
        .min_by(|a, b| a.ops_per_second.total_cmp(&b.ops_per_second))
        .expect("at least one scenario");
    let cv = worst.deviation.as_secs_f64() / worst.median.as_secs_f64() * 100.0;
    println!();
    println!(
        "worst-case median throughput: {} {:.3} M ops/s (median {:?}, stddev {:?}, CV {:.1}%)",
        worst.scenario.name(),
        worst.ops_per_second / 1_000_000.0,
        worst.median,
        worst.deviation,
        cv,
    );
    println!(
        "worst-case book: avg {:.0} orders / {:.1} levels, peak {}/{}; {} adds rested, {} fills",
        worst.occupancy.average_orders(),
        worst.occupancy.average_levels(),
        worst.occupancy.peak_orders,
        worst.occupancy.peak_levels,
        worst.stats.add_rested,
        worst.stats.fills,
    );
    println!(
        "This is an occupancy envelope, not the external 2M-message flow: it isolates the price-ladder cost at four standing-book shapes."
    );
}
