//! WAL append, read and recovery benchmark.
//!
//! Run with `cargo bench --bench storage`. It uses the real segmented WAL and
//! the real serialized `MatchEngineRequest` payload, not blank log entries.

use std::path::PathBuf;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::LogStore;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::LogId;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryOf;
use openraft::alias::LogIdOf;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use openraft::vote::RaftLeaderId;

type C = MatchEngineTypeConfig;

const ENTRIES: u64 = 20_000;
const BATCH: u64 = 100;
const SEGMENT_SIZE: u64 = 512 * 1024;

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-storage-bench-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn log_id(index: u64) -> LogIdOf<C> {
    LogId::new(CommittedLeaderIdOf::<C>::new(1, 1), index)
}

fn entry(index: u64) -> EntryOf<C> {
    Entry {
        log_id: log_id(index),
        payload: EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            100.0 + (index % 100) as f64 * 0.01,
            1.0,
            0,
        ))),
    }
}

async fn append(store: &LogStore, entries: Vec<EntryOf<C>>) {
    let mut store = store.clone();
    store.append(entries, IOFlushed::noop()).await.unwrap();
}

async fn read(store: &LogStore, range: std::ops::Range<u64>) -> usize {
    let mut store = store.clone();
    store.try_get_log_entries(range).await.unwrap().len()
}

#[tokio::main]
async fn main() {
    let dir = temp_dir("wal");
    let store = LogStore::open_with_segment_size(&dir, SEGMENT_SIZE).unwrap();

    let started = Instant::now();
    for start in (1..=ENTRIES).step_by(BATCH as usize) {
        let end = (start + BATCH - 1).min(ENTRIES);
        append(&store, (start..=end).map(entry).collect()).await;
    }
    let append_elapsed = started.elapsed();

    let started = Instant::now();
    let read_count = read(&store, 1..ENTRIES + 1).await;
    let read_elapsed = started.elapsed();
    assert_eq!(ENTRIES as usize, read_count);

    drop(store);

    let started = Instant::now();
    let reopened = LogStore::open_with_segment_size(&dir, SEGMENT_SIZE).unwrap();
    let reopen_elapsed = started.elapsed();
    let recovered = read(&reopened, 1..ENTRIES + 1).await;
    assert_eq!(ENTRIES as usize, recovered);

    let segments = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wal"))
        .count();

    println!(
        "storage benchmark: {ENTRIES} entries, batch {BATCH}, segment {SEGMENT_SIZE} B, {segments} segments"
    );
    println!(
        "  append: {:>8.2?} ({:>10.0} entries/s)",
        append_elapsed,
        ENTRIES as f64 / append_elapsed.as_secs_f64()
    );
    println!(
        "  read:   {:>8.2?} ({:>10.0} entries/s)",
        read_elapsed,
        ENTRIES as f64 / read_elapsed.as_secs_f64()
    );
    println!("  reopen: {:>8.2?}", reopen_elapsed);
    println!("  recovered entries: {recovered}");

    let _ = std::fs::remove_dir_all(dir);
}
