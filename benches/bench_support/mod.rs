//! Shared timing helpers for the no-framework benchmarks.
//!
//! The book benches time whole rounds by hand: `Instant` around the operation
//! under test, with the state cloned outside the timer. This module adds the
//! measurement discipline recorded in `docs/PERFORMANCE.md`:
//!
//! * per-round samples instead of only a best-of-5 mean, so p50/p99 are
//!   visible - best-of-5 alone hides the tails a regression gate needs;
//! * an explicit cold-cache pass: a 64 MiB read-modify-write sweep between the
//!   clone and the timed section evicts the freshly cloned book, so the cold
//!   rows measure cache-miss behaviour instead of best-case residency;
//! * setup and teardown outside the timer: the clone runs before
//!   `Instant::now()` and the mutated state is dropped after the duration is
//!   recorded;
//! * [`best_of_repeats`] keeps the historical best-of-5 mean-of-rounds metric,
//!   so new tables stay comparable with the recorded baselines.
//!
//! Warm rows: the clone runs immediately before the timed op, so the cloned
//! book is cache-resident. Cold rows: the eviction sweep runs between the
//! clone and the timed op. Warm and cold rounds are interleaved in one time
//! window so clock ramp-up and machine drift do not bias one pass. Every
//! duration is one whole round (one sweep, one batch of cancels, ...), not a
//! single operation.

use std::hint::black_box;
use std::time::Duration;
use std::time::Instant;

/// Warm repetitions; the minimum of the five mean-of-`rounds` values is the
/// historical "best-of-5" number.
pub const REPEATS: usize = 5;
/// Rounds measured after a cache eviction. 40 keeps every case's cold pass
/// short while still giving a usable p50; p99 at n=40 is the worst round.
pub const COLD_ROUNDS: usize = 40;
/// Size of the read-modify-write sweep used to evict the caches. Larger than
/// the system cache on current Apple-silicon machines.
const EVICT_BYTES: usize = 64 << 20;

/// Per-round timing samples for one case.
pub struct Samples {
    /// Warm rounds: clone immediately before the timed op.
    pub warm: Vec<Duration>,
    /// Cold rounds: eviction sweep between clone and timed op.
    pub cold: Vec<Duration>,
    /// Units of work per round (fills, cancels, ...), constant across rounds.
    pub units_per_round: usize,
}

/// Timing configuration for [`run`].
#[derive(Clone, Copy)]
pub struct Config {
    pub repeats: usize,
    pub cold_rounds: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repeats: REPEATS,
            cold_rounds: COLD_ROUNDS,
        }
    }
}

fn evict_caches(scratch: &mut [u8]) {
    // One read-modify-write per cache line. A write allocates the line, so the
    // sweep evicts whatever was resident, and the dependency on the previous
    // byte value keeps it from being optimised away.
    for line in scratch.chunks_mut(64) {
        line[0] = line[0].wrapping_add(1);
    }
    black_box(scratch.as_ptr());
}

/// Runs `op` on a fresh clone of `base` per round and times only the op.
///
/// Setup and teardown are outside the timed window: the clone runs before
/// `Instant::now()`, and the mutated state is dropped after the duration is
/// recorded (the external optimization notes' `iter_custom` lesson - timing a
/// book's drop inflated every case there). The warm pass collects
/// `config.repeats * rounds` per-round durations; the cold pass collects
/// `config.cold_rounds` more, interleaved with the warm rounds, and asserts
/// that both passes did the same work per round.
pub fn run<T>(
    config: Config,
    rounds: u32,
    base: &T,
    mut clone: impl FnMut(&T) -> T,
    mut op: impl FnMut(&mut T) -> usize,
) -> Samples {
    assert!(rounds > 0, "a case needs at least one round");
    assert!(
        config.cold_rounds > 0,
        "a case needs at least one cold round"
    );
    // One untimed repeat primes the allocator, page tables and CPU frequency,
    // so the collected samples do not carry one-off warm-up cost or a low
    // clock during the first repeat.
    for _ in 0..rounds {
        let mut state = clone(base);
        black_box(op(&mut state));
    }

    let warm_total = config.repeats * rounds as usize;
    // Warm and cold rounds are interleaved in one time window. Running all
    // warm rounds first systematically biased the warm pass (the machine had
    // not settled and the cold pass inherited a boosted clock), which showed
    // up as cold p50 beating warm p50.
    let stride = (warm_total / config.cold_rounds.max(1)).max(1);
    let mut scratch = vec![0u8; EVICT_BYTES];
    let mut warm = Vec::with_capacity(warm_total);
    let mut cold = Vec::with_capacity(config.cold_rounds);
    let mut warm_units = 0usize;
    let mut cold_units = 0usize;

    for round in 0..warm_total {
        // Warm: the clone immediately before the timed op leaves the book
        // cache-resident.
        let mut state = clone(base);
        let start = Instant::now();
        warm_units += op(&mut state);
        warm.push(start.elapsed());
        drop(state);

        // Cold: clone, evict the caches, then time the same operation.
        if round % stride == 0 && cold.len() < config.cold_rounds {
            let mut state = clone(base);
            evict_caches(&mut scratch);
            let start = Instant::now();
            cold_units += op(&mut state);
            cold.push(start.elapsed());
            drop(state);
        }
    }
    while cold.len() < config.cold_rounds {
        let mut state = clone(base);
        evict_caches(&mut scratch);
        let start = Instant::now();
        cold_units += op(&mut state);
        cold.push(start.elapsed());
        drop(state);
    }

    let units_per_round = warm_units / warm_total;
    assert_eq!(
        units_per_round,
        cold_units / config.cold_rounds,
        "cold rounds must do the same work as warm rounds"
    );

    Samples {
        warm,
        cold,
        units_per_round,
    }
}

/// Nearest-rank percentile: sorts a copy and returns the sample at
/// `ceil(rank * n)`, so p99 of 40 samples is the worst round.
pub fn percentile(samples: &[Duration], rank: f64) -> Duration {
    assert!(!samples.is_empty(), "percentile of no samples");
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((rank * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[index - 1]
}

/// The historical metric: the best mean-of-`rounds` across the warm repeats.
pub fn best_of_repeats(samples: &[Duration], rounds: u32) -> Duration {
    samples
        .chunks(rounds as usize)
        .map(|chunk| chunk.iter().sum::<Duration>() / rounds)
        .min()
        .unwrap_or(Duration::MAX)
}

impl Samples {
    /// Best mean-of-`rounds` across the warm repeats (the old "best-of-5").
    pub fn warm_best(&self, rounds: u32) -> Duration {
        best_of_repeats(&self.warm, rounds)
    }

    /// Warm p50 over all per-round samples.
    pub fn warm_p50(&self) -> Duration {
        percentile(&self.warm, 0.50)
    }

    /// Warm p99 over all per-round samples.
    pub fn warm_p99(&self) -> Duration {
        percentile(&self.warm, 0.99)
    }

    /// Cold p50 over the eviction-pass samples.
    pub fn cold_p50(&self) -> Duration {
        percentile(&self.cold, 0.50)
    }

    /// Cold p99 over the eviction-pass samples.
    pub fn cold_p99(&self) -> Duration {
        percentile(&self.cold, 0.99)
    }

    /// Units per second implied by `per_round`.
    pub fn units_per_second(&self, per_round: Duration) -> f64 {
        self.units_per_round as f64 / per_round.as_secs_f64()
    }
}
