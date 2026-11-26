//! Apply-path allocation benchmark.
//!
//! Run with `cargo bench --bench apply_path`. This drives
//! `StateMachineStore::apply` directly (no HTTP, Raft, WAL or feed) and reports
//! heap allocations and requested bytes per applied entry. The `blank` and
//! `batch blank` baselines isolate the harness and response allocations from the
//! event path.
//!
//! The global allocator counts calls, not live bytes: `allocs/entry` is heap
//! churn, and `bytes/entry` is the sum of requested sizes (including reallocs),
//! not a memory high-water mark.
//!
//! `--check` turns the measured 2026-11-09 totals into CI ceilings: allocation
//! calls may not increase, and requested bytes get a small allowance. The
//! `bench-regression` CI job runs this mode.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::stream;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::market_data::MarketDataTap;
use matchengine_raft::match_result::FileLogSink;
use matchengine_raft::match_result::NoopSink;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::PegReference;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::state_machine::StateMachineStore;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::LogId;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryOf;
use openraft::alias::EntryPayloadOf;
use openraft::alias::LogIdOf;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;
use openraft::vote::RaftLeaderId;

type C = MatchEngineTypeConfig;

const ENTRY_WINDOW: usize = 4_096;
const MEASURED_ENTRIES: usize = 50_000;
const BATCH_SIZE: usize = 128;
const MEASURED_BATCHES: usize = 1_000;

/// `--peg` workload: a deep book of plain bids plus optional pegged bids at
/// the front of the ladder. `StateMachineStore::apply` calls `reprice_pegged`
/// after every Place, so this isolates the peg discovery scan (F1a) and the
/// per-peg non-pegged BBO walk (F1c) without HTTP, Raft or events on the wire.
const PEG_SETUP: usize = 5_000;
const PEG_ENTRIES: usize = 5_000;
const PEG_COUNT: usize = 32;

/// `--trail` workload: parked trailing stops plus repeated prints. Each print
/// updates every trailing stop; the allocation counter detects the temporary
/// snapshot vector that `observe_trade` used to allocate per print.
const TRAIL_COUNT: usize = 8;
const TRAIL_ENTRIES: usize = 5_000;

/// The blank row is a harness baseline, not an engine workload. Some Linux CI
/// runners still perform one small lazy initialisation on the first measured
/// apply; allow a fixed handful of calls/bytes so that noise cannot fail CI
/// while a real per-entry regression (thousands of calls) still does.
const BLANK_MAX_CALLS: usize = 4;
const BLANK_MAX_BYTES: usize = 128;

/// Allocation ceilings for `--check`. Tightened again on 2026-11-26 after
/// responses stopped being built for follower/replay entries and batch
/// publishes were coalesced: measured totals are 8,335 / 0 / 100,000 / 1,000 /
/// 27,336. The ceilings keep headroom for allocator differences across hosts.
const REST_MAX_CALLS: usize = 10_000;
const CANCEL_MAX_CALLS: usize = 2_000;
const FILL_MAX_CALLS: usize = 102_000;
const BATCH_BLANK_MAX_CALLS: usize = 2_000;
const BATCH_REST_MAX_CALLS: usize = 32_000;

/// Requested bytes get a small allowance because capacity choices can change
/// without increasing the allocation count. The batch rows include the larger
/// inline response header; the hot single-request rows are much smaller now.
const REST_MAX_BYTES: usize = 30_000_000;
const CANCEL_MAX_BYTES: usize = 1_000_000;
const FILL_MAX_BYTES: usize = 120_000_000;
const BATCH_BLANK_MAX_BYTES: usize = 80_000_000;
const BATCH_REST_MAX_BYTES: usize = 250_000_000;

static CHECK_MODE: AtomicBool = AtomicBool::new(false);
static CHECK_FAILED: AtomicBool = AtomicBool::new(false);

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct AllocSnapshot {
    calls: usize,
    bytes: usize,
}

fn reset_allocations() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
}

fn allocation_snapshot() -> AllocSnapshot {
    AllocSnapshot {
        calls: ALLOCATIONS.load(Ordering::Relaxed),
        bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

fn tempdir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-apply-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn log_id(term: u64, index: u64) -> LogIdOf<C> {
    LogId::new(CommittedLeaderIdOf::<C>::new(term, 1), index)
}

fn entry(index: u64, payload: EntryPayloadOf<C>) -> EntryOf<C> {
    Entry {
        log_id: log_id(1, index),
        payload,
    }
}

fn open_store(label: &str) -> anyhow::Result<(StateMachineStore<C>, PathBuf)> {
    let dir = tempdir(label);
    let store = StateMachineStore::<C>::open_with_sinks_and_retention(
        &dir,
        Arc::new(NoopSink),
        None,
        ENTRY_WINDOW,
    )?;
    Ok((store, dir))
}

async fn apply_entry(store: &mut StateMachineStore<C>, index: u64, payload: EntryPayloadOf<C>) {
    let item: Result<EntryResponder<C>, std::io::Error> = Ok((entry(index, payload), None));
    let entries = stream::iter(std::iter::once(item));
    store.apply(entries).await.unwrap();
}

async fn apply_rests(store: &mut StateMachineStore<C>, first: u64, count: usize) {
    for offset in 0..count {
        apply_entry(
            store,
            first + offset as u64,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))),
        )
        .await;
    }
}

fn report(
    label: &str,
    entries: usize,
    elapsed: Duration,
    alloc: AllocSnapshot,
    max_calls: usize,
    max_bytes: usize,
) {
    let per_entry = alloc.calls as f64 / entries as f64;
    let bytes_per_entry = alloc.bytes as f64 / entries as f64;
    let micros = elapsed.as_secs_f64() * 1_000_000.0 / entries as f64;
    println!(
        "{label:24} entries={entries:>7} {micros:>8.3} us/entry  {per_entry:>7.2} allocs/entry  {bytes_per_entry:>9.1} bytes/entry  calls={} bytes={}",
        alloc.calls, alloc.bytes
    );

    if CHECK_MODE.load(Ordering::Relaxed) && (alloc.calls > max_calls || alloc.bytes > max_bytes) {
        eprintln!(
            "CHECK FAIL {label}: calls {}/{} bytes {}/{}",
            alloc.calls, max_calls, alloc.bytes, max_bytes
        );
        CHECK_FAILED.store(true, Ordering::Relaxed);
    }
}

async fn blank_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("blank")?;
    // Warm up runtime/lock/timer initialization before the measured window.
    // Linux builds have shown a one-time 3-allocation baseline on the first
    // apply after the counter reset; a single warm-up entry was not enough.
    for index in 1..=64 {
        apply_entry(&mut store, index, EntryPayload::Blank).await;
    }

    reset_allocations();
    // Some Linux builds still initialise lazily on the first apply *after* the
    // counter reset; absorb that in a second, uncounted warm-up.
    for index in 65..=72 {
        apply_entry(&mut store, index, EntryPayload::Blank).await;
    }
    reset_allocations();
    let started = Instant::now();
    for index in 73..73 + MEASURED_ENTRIES as u64 {
        apply_entry(&mut store, index, EntryPayload::Blank).await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "blank",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        BLANK_MAX_CALLS,
        BLANK_MAX_BYTES,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn rest_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("rest")?;
    apply_rests(&mut store, 1, ENTRY_WINDOW).await;
    let first = ENTRY_WINDOW as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for index in first..first + MEASURED_ENTRIES as u64 {
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))),
        )
        .await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "rest (1 event)",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        REST_MAX_CALLS,
        REST_MAX_BYTES,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn cancel_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("cancel")?;
    let setup = ENTRY_WINDOW + MEASURED_ENTRIES;
    apply_rests(&mut store, 1, setup).await;
    let first = setup as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for (offset, index) in (0..MEASURED_ENTRIES).zip(first..) {
        let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
        order.sequence = offset as u64;
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::Cancel { order }),
        )
        .await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "cancel (1 event)",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        CANCEL_MAX_CALLS,
        CANCEL_MAX_BYTES,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn fill_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("fill")?;
    let setup = ENTRY_WINDOW + MEASURED_ENTRIES;
    for offset in 0..setup {
        apply_entry(
            &mut store,
            1 + offset as u64,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Sell,
                101.0,
                1.0,
                0,
            ))),
        )
        .await;
    }
    let first = setup as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for index in first..first + MEASURED_ENTRIES as u64 {
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                101.0,
                1.0,
                0,
            ))),
        )
        .await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "fill (1 event)",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        FILL_MAX_CALLS,
        FILL_MAX_BYTES,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn batch_requests(size: usize, rest: bool) -> Vec<MatchEngineRequest> {
    (0..size)
        .map(|_| {
            if rest {
                MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0))
            } else {
                MatchEngineRequest::PurgeExpired {
                    now_ms: 0,
                    day_close_ms: None,
                }
            }
        })
        .collect()
}

async fn batch_workload(
    label: &str,
    rest: bool,
    max_calls: usize,
    max_bytes: usize,
) -> anyhow::Result<()> {
    let (mut store, dir) = open_store(label)?;
    if rest {
        apply_rests(&mut store, 1, ENTRY_WINDOW).await;
    }
    let first = if rest { ENTRY_WINDOW as u64 + 1 } else { 1 };
    let template = batch_requests(BATCH_SIZE, rest);

    reset_allocations();
    let started = Instant::now();
    for index in first..first + MEASURED_BATCHES as u64 {
        let batch = MatchEngineRequest::Batch {
            requests: template.clone(),
        };
        apply_entry(&mut store, index, EntryPayload::Normal(batch)).await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        label,
        MEASURED_BATCHES * BATCH_SIZE,
        elapsed,
        alloc,
        max_calls,
        max_bytes,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn peg_workload(label: &str, pegs: usize) -> anyhow::Result<()> {
    let (mut store, dir) = open_store(label)?;
    let mut index = 1u64;

    // Plain bids at 100, then an ask far enough away that peg targets at 101
    // rest without crossing. Pegged bids therefore sit in front of the plain
    // book in price order, which is the worst case for `best_non_pegged_*`.
    apply_rests(&mut store, index, PEG_SETUP).await;
    index += PEG_SETUP as u64;
    apply_entry(
        &mut store,
        index,
        EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Sell,
            200.0,
            1.0,
            0,
        ))),
    )
    .await;
    index += 1;
    for _ in 0..pegs {
        let mut order = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
        order.peg = Some(PegReference::Primary);
        order.peg_offset = Fixed::from(1);
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(order)),
        )
        .await;
        index += 1;
    }

    {
        let inner = store.inner().lock().await;
        let pegged = inner
            .orderbook
            .bids
            .values()
            .chain(inner.orderbook.asks.values())
            .filter(|order| order.is_pegged())
            .count();
        assert_eq!(pegs, pegged, "peg fixture must rest every pegged order");
    }

    // Each measured Place rests at 99 and triggers the post-apply peg pass.
    reset_allocations();
    let started = Instant::now();
    for _ in 0..PEG_ENTRIES {
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                99.0,
                1.0,
                0,
            ))),
        )
        .await;
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(label, PEG_ENTRIES, elapsed, alloc, usize::MAX, usize::MAX);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn trail_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("trail")?;
    let mut index = 1u64;

    // Establish a last trade price so the trailing stops park with a trigger.
    apply_entry(
        &mut store,
        index,
        EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Sell,
            100.0,
            1.0,
            0,
        ))),
    )
    .await;
    index += 1;
    apply_entry(
        &mut store,
        index,
        EntryPayload::Normal(MatchEngineRequest::place({
            let mut order = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
            order.live = OrderType::Market;
            order
        })),
    )
    .await;
    index += 1;

    for _ in 0..TRAIL_COUNT {
        let mut order = Order::new(OrderSide::Sell, 0.0, 1.0, 0);
        order.live = OrderType::Market;
        order.trail_offset = Fixed::from(1);
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(order)),
        )
        .await;
        index += 1;
    }
    // One ask per measured print; trailing triggers stay below 101 as the
    // reference rises, so the stops remain parked.
    apply_entry(
        &mut store,
        index,
        EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Sell,
            101.0,
            TRAIL_ENTRIES as f64,
            0,
        ))),
    )
    .await;
    index += 1;

    reset_allocations();
    let started = Instant::now();
    for _ in 0..TRAIL_ENTRIES {
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place({
                let mut order = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
                order.live = OrderType::Market;
                order
            })),
        )
        .await;
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "trailing print (8 stops)",
        TRAIL_ENTRIES,
        elapsed,
        alloc,
        usize::MAX,
        usize::MAX,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn ids_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("ids")?;
    // Build payloads before the measured window so only apply-time clones count.
    let payloads: Vec<_> = (0..MEASURED_ENTRIES)
        .map(|i| {
            EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                format!("bench-{i}"),
            ))
        })
        .collect();

    reset_allocations();
    let started = Instant::now();
    for (offset, payload) in payloads.into_iter().enumerate() {
        apply_entry(&mut store, 1 + offset as u64, payload).await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "rest with client_order_id",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        usize::MAX,
        usize::MAX,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn batch_peg_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("batch-peg")?;
    let mut index = 1u64;
    apply_rests(&mut store, index, PEG_SETUP).await;
    index += PEG_SETUP as u64;
    apply_entry(
        &mut store,
        index,
        EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Sell,
            200.0,
            1.0,
            0,
        ))),
    )
    .await;
    index += 1;
    for _ in 0..PEG_COUNT {
        let mut order = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
        order.peg = Some(PegReference::Primary);
        order.peg_offset = Fixed::from(1);
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(order)),
        )
        .await;
        index += 1;
    }

    let template = batch_requests(BATCH_SIZE, true);
    reset_allocations();
    let started = Instant::now();
    for _ in 0..MEASURED_BATCHES {
        let batch = MatchEngineRequest::Batch {
            requests: template.clone(),
        };
        apply_entry(&mut store, index, EntryPayload::Normal(batch)).await;
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "batch rest with 32 pegs",
        MEASURED_BATCHES * BATCH_SIZE,
        elapsed,
        alloc,
        usize::MAX,
        usize::MAX,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn sink_workload() -> anyhow::Result<()> {
    let dir = tempdir("sink");
    let sink = Arc::new(FileLogSink::open(dir.join("events.jsonl"))?);
    let mut store =
        StateMachineStore::<C>::open_with_sinks_and_retention(&dir, sink, None, ENTRY_WINDOW)?;
    apply_rests(&mut store, 1, ENTRY_WINDOW).await;
    let first = ENTRY_WINDOW as u64 + 1;
    let template = batch_requests(BATCH_SIZE, true);

    reset_allocations();
    let started = Instant::now();
    for index in first..first + MEASURED_BATCHES as u64 {
        let batch = MatchEngineRequest::Batch {
            requests: template.clone(),
        };
        apply_entry(&mut store, index, EntryPayload::Normal(batch)).await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "batch rest + file sink",
        MEASURED_BATCHES * BATCH_SIZE,
        elapsed,
        alloc,
        usize::MAX,
        usize::MAX,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn tap_workload() -> anyhow::Result<()> {
    let dir = tempdir("tap");
    let (tap, _channels) = MarketDataTap::channel(8_192);
    let mut store = StateMachineStore::<C>::open_with_sinks_and_retention(
        &dir,
        Arc::new(NoopSink),
        Some(tap),
        ENTRY_WINDOW,
    )?;

    reset_allocations();
    let started = Instant::now();
    for index in 1..=MEASURED_ENTRIES as u64 {
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))),
        )
        .await;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report(
        "rest with market-data tap",
        MEASURED_ENTRIES,
        elapsed,
        alloc,
        usize::MAX,
        usize::MAX,
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--peg") => {
            peg_workload("peg scan (0 pegs)", 0).await?;
            peg_workload("peg scan (32 pegs)", PEG_COUNT).await?;
            return Ok(());
        }
        Some("--trail") => {
            trail_workload().await?;
            return Ok(());
        }
        Some("--sink") => {
            sink_workload().await?;
            return Ok(());
        }
        Some("--tap") => {
            tap_workload().await?;
            return Ok(());
        }
        Some("--ids") => {
            ids_workload().await?;
            return Ok(());
        }
        Some("--batch-peg") => {
            batch_peg_workload().await?;
            return Ok(());
        }
        _ => {}
    }

    let check = std::env::args().any(|arg| arg == "--check");
    CHECK_MODE.store(check, Ordering::Relaxed);

    println!("apply-path allocation benchmark (direct StateMachineStore::apply)");
    println!("counts are heap allocations, not live bytes; bytes include realloc churn");
    if check {
        println!("check mode: allocation ceilings are enforced\n");
    } else {
        println!();
    }

    blank_workload().await?;
    rest_workload().await?;
    cancel_workload().await?;
    fill_workload().await?;
    batch_workload(
        "batch blank",
        false,
        BATCH_BLANK_MAX_CALLS,
        BATCH_BLANK_MAX_BYTES,
    )
    .await?;
    batch_workload(
        "batch rest (1 event each)",
        true,
        BATCH_REST_MAX_CALLS,
        BATCH_REST_MAX_BYTES,
    )
    .await?;

    if check {
        if CHECK_FAILED.load(Ordering::Relaxed) {
            anyhow::bail!("apply-path allocation regression");
        }
        println!("\nallocation regression check passed");
    }

    Ok(())
}
