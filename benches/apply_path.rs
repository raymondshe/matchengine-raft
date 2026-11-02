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

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::stream;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::match_result::NoopSink;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
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

fn report(label: &str, entries: usize, elapsed: Duration, alloc: AllocSnapshot) {
    let per_entry = alloc.calls as f64 / entries as f64;
    let bytes_per_entry = alloc.bytes as f64 / entries as f64;
    let micros = elapsed.as_secs_f64() * 1_000_000.0 / entries as f64;
    println!(
        "{label:24} entries={entries:>7} {micros:>8.3} us/entry  {per_entry:>7.2} allocs/entry  {bytes_per_entry:>9.1} bytes/entry"
    );
}

async fn blank_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("blank")?;
    apply_entry(&mut store, 1, EntryPayload::Blank).await;
    let mut index = 2u64;

    reset_allocations();
    let started = Instant::now();
    for _ in 0..MEASURED_ENTRIES {
        apply_entry(&mut store, index, EntryPayload::Blank).await;
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report("blank", MEASURED_ENTRIES, elapsed, alloc);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn rest_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("rest")?;
    apply_rests(&mut store, 1, ENTRY_WINDOW).await;
    let mut index = ENTRY_WINDOW as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for _ in 0..MEASURED_ENTRIES {
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
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report("rest (1 event)", MEASURED_ENTRIES, elapsed, alloc);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn cancel_workload() -> anyhow::Result<()> {
    let (mut store, dir) = open_store("cancel")?;
    let setup = ENTRY_WINDOW + MEASURED_ENTRIES;
    apply_rests(&mut store, 1, setup).await;
    let mut index = setup as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for offset in 0..MEASURED_ENTRIES {
        let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
        order.sequence = offset as u64;
        apply_entry(
            &mut store,
            index,
            EntryPayload::Normal(MatchEngineRequest::Cancel { order }),
        )
        .await;
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report("cancel (1 event)", MEASURED_ENTRIES, elapsed, alloc);

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
    let mut index = setup as u64 + 1;

    reset_allocations();
    let started = Instant::now();
    for _ in 0..MEASURED_ENTRIES {
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
        index += 1;
    }
    let elapsed = started.elapsed();
    let alloc = allocation_snapshot();
    report("fill (1 event)", MEASURED_ENTRIES, elapsed, alloc);

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

async fn batch_workload(label: &str, rest: bool) -> anyhow::Result<()> {
    let (mut store, dir) = open_store(label)?;
    let mut index = 1u64;
    if rest {
        apply_rests(&mut store, 1, ENTRY_WINDOW).await;
        index = ENTRY_WINDOW as u64 + 1;
    }
    let template = batch_requests(BATCH_SIZE, rest);

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
    report(label, MEASURED_BATCHES * BATCH_SIZE, elapsed, alloc);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("apply-path allocation benchmark (direct StateMachineStore::apply)");
    println!("counts are heap allocations, not live bytes; bytes include realloc churn\n");

    blank_workload().await?;
    rest_workload().await?;
    cancel_workload().await?;
    fill_workload().await?;
    batch_workload("batch blank", false).await?;
    batch_workload("batch rest (1 event each)", true).await?;

    Ok(())
}
