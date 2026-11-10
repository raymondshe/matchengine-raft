//! Storage-layer tests: WAL append/read/recovery and snapshot persistence.
//!
//! These exercise the storage types directly, without starting a Raft node.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::store::StateMachineData;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_bytes;
use matchengine_raft::store::log_store::read_wal_file;
use matchengine_raft::store::state_machine::SnapshotData;
use matchengine_raft::store::state_machine::read_snapshot_bytes;
use matchengine_raft::store::state_machine::read_snapshot_file;
use openraft::Entry;
use openraft::LogId;
use openraft::LogState;
use openraft::RaftSnapshotBuilder;
use openraft::Vote;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryOf;
use openraft::alias::LogIdOf;
use openraft::alias::SnapshotMetaOf;
use openraft::entry::RaftEntry;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftStateMachine;
#[cfg(feature = "async-wal")]
use openraft::type_config::TypeConfigExt;
use openraft::vote::RaftLeaderId;

type C = MatchEngineTypeConfig;

/// Runs a future on a fresh current-thread Tokio runtime, since the storage layer
/// uses `spawn_blocking` and therefore needs a reactor.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}
type Store = matchengine_raft::LogStore;
type Sm = matchengine_raft::StateMachineStore;

// --- Helpers

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "matchengine-storage-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn log_id(term: u64, index: u64) -> LogIdOf<C> {
    LogId::new(CommittedLeaderIdOf::<C>::new(term, 1), index)
}

fn blank(index: u64) -> EntryOf<C> {
    Entry::new_blank(log_id(1, index))
}

async fn append(store: &Store, entries: Vec<EntryOf<C>>) {
    let mut store = store.clone();
    store.append(entries, IOFlushed::noop()).await.unwrap();
}

async fn read(store: &Store, range: std::ops::Range<u64>) -> Vec<EntryOf<C>> {
    let mut store = store.clone();
    store.try_get_log_entries(range).await.unwrap()
}

async fn log_state(store: &Store) -> LogState<C> {
    let mut store = store.clone();
    store.get_log_state().await.unwrap()
}

fn wal_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "wal"))
        .collect();
    files.sort();
    files
}

fn indexes(entries: &[EntryOf<C>]) -> Vec<u64> {
    entries.iter().map(|e| e.log_id().index).collect()
}

// --- WAL tests

#[test]
fn append_and_read_roundtrip() {
    let dir = TempDir::new("append-read");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();

        append(&store, vec![blank(0), blank(1), blank(2)]).await;
        append(&store, vec![blank(3), blank(4)]).await;

        assert_eq!(vec![0, 1, 2, 3, 4], indexes(&read(&store, 0..10).await));
        assert_eq!(vec![2, 3], indexes(&read(&store, 2..4).await));
        assert!(read(&store, 10..20).await.is_empty());

        assert_eq!(Some(log_id(1, 4)), log_state(&store).await.last_log_id);
        assert_eq!(None, log_state(&store).await.last_purged_log_id);
    });
}

#[test]
fn entries_survive_reopen() {
    let dir = TempDir::new("reopen");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, (0..10).map(blank).collect()).await;
        append(&store, (10..20).map(blank).collect()).await;
        drop(store);
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..20).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );

        // Appending after recovery continues the same log.
        append(&store, vec![blank(20)]).await;
        assert_eq!(Some(log_id(1, 20)), log_state(&store).await.last_log_id);
    });
}

#[test]
fn torn_tail_is_discarded_on_reopen() {
    let dir = TempDir::new("torn-tail");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, (0..5).map(blank).collect()).await;
        drop(store);
    });

    let files = wal_files(dir.path());
    let last = files.last().unwrap();
    let good_len = std::fs::metadata(last).unwrap().len();

    // A frame that claims 64 bytes but only carries 3: the classic torn write.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(last).unwrap();
        f.write_all(&64u32.to_le_bytes()).unwrap();
        f.write_all(&0xdead_beefu32.to_le_bytes()).unwrap();
        f.write_all(b"abc").unwrap();
    }

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..5).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 4)), log_state(&store).await.last_log_id);
    });

    // The torn suffix was truncated away, so the file is back to the last good frame.
    assert_eq!(good_len, std::fs::metadata(last).unwrap().len());

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, vec![blank(5)]).await;
    });
    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..6).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
    });
}

#[test]
fn segments_rotate_and_survive_reopen() {
    let dir = TempDir::new("rotate");

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        for i in 0..40 {
            append(&store, vec![blank(i)]).await;
        }
        assert_eq!(
            (0..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        drop(store);
    });

    assert!(
        wal_files(dir.path()).len() > 1,
        "small segments should have rotated"
    );

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        assert_eq!(
            (0..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 39)), log_state(&store).await.last_log_id);
    });
}

#[test]
fn corrupt_non_last_segment_recovers_a_valid_prefix_without_fsync() {
    let dir = TempDir::new("corrupt-non-last");

    block_on(async {
        let store = Store::open_with_segment_size_and_fsync(dir.path(), 512, None).unwrap();
        for i in 0..40 {
            append(&store, vec![blank(i)]).await;
        }
        drop(store);
    });

    let files = wal_files(dir.path());
    assert!(files.len() > 1, "small segments should have rotated");
    let first = &files[0];
    let good_len = std::fs::metadata(first).unwrap().len();
    assert!(good_len > 16, "first segment should contain a record");

    // Cut three bytes off the last frame in a non-last segment: a torn record
    // that a crash can leave when append fsync is disabled.
    std::fs::OpenOptions::new()
        .write(true)
        .open(first)
        .unwrap()
        .set_len(good_len - 3)
        .unwrap();

    // Strict recovery still rejects corruption in a non-last segment.
    assert!(
        Store::open_with_segment_size_and_fsync(dir.path(), 512, Some(std::time::Duration::ZERO))
            .is_err()
    );

    // Best-effort recovery keeps the valid prefix, removes later segments, and
    // leaves the log appendable so Raft can catch up from peers.
    block_on(async {
        let store = Store::open_with_segment_size_and_fsync(dir.path(), 512, None).unwrap();
        let entries = read(&store, 0..100).await;
        assert!(!entries.is_empty());
        assert_eq!(
            (0..entries.len() as u64).collect::<Vec<_>>(),
            indexes(&entries)
        );
        assert_eq!(
            Some(log_id(1, entries.len() as u64 - 1)),
            log_state(&store).await.last_log_id
        );
        assert_eq!(1, wal_files(dir.path()).len(), "later segments are removed");

        append(&store, vec![blank(entries.len() as u64)]).await;
        assert_eq!(
            (0..=entries.len() as u64).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
    });
}

#[test]
fn periodic_fsync_task_clears_dirty_state() {
    let dir = TempDir::new("periodic-fsync");

    block_on(async {
        let store = Store::open_with_segment_size_and_fsync(
            dir.path(),
            512,
            Some(std::time::Duration::from_millis(20)),
        )
        .unwrap();
        store.start_fsync_worker().unwrap();
        append(&store, vec![blank(0)]).await;
        assert!(store.wal_dirty(), "append should mark the WAL dirty");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !store.wal_dirty(),
            "periodic fsync should clear the dirty flag"
        );
    });
}

#[cfg(feature = "async-wal")]
#[test]
fn async_wal_defers_io_flushed_and_recovers_a_lost_tail() {
    let dir = TempDir::new("async-wal");

    block_on(async {
        let store =
            Store::open_with_fsync_and_async_wal(dir.path(), Some(std::time::Duration::ZERO), true)
                .unwrap();
        store.start_async_wal_worker().unwrap();

        let (tx, rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(0)], IOFlushed::signal(tx))
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        let files = wal_files(dir.path());
        let last = files.last().unwrap().clone();
        let durable_len = std::fs::metadata(&last).unwrap().len();
        drop(writer);
        drop(store);
        tokio::task::yield_now().await;

        // A second append is readable before the async fsync, and its
        // IOFlushed callback is still pending.
        let store =
            Store::open_with_fsync_and_async_wal(dir.path(), Some(std::time::Duration::ZERO), true)
                .unwrap();
        store
            .start_async_wal_worker_with_delay(std::time::Duration::from_secs(60))
            .unwrap();
        let (tx, mut rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(1)], IOFlushed::signal(tx))
            .await
            .unwrap();
        assert!(rx.try_recv().is_err(), "IOFlushed must wait for the fsync");
        assert_eq!(vec![0, 1], indexes(&read(&store, 0..10).await));
        assert_eq!(Some(log_id(1, 1)), log_state(&store).await.last_log_id);
        drop(writer);
        drop(store);
        tokio::task::yield_now().await;

        // Simulate a crash that loses the unsynced tail: truncate the last
        // record back to the last durable offset, then reopen.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&last)
            .unwrap()
            .set_len(durable_len)
            .unwrap();

        let store = Store::open_with_fsync_and_async_wal(
            dir.path(),
            Some(std::time::Duration::ZERO),
            false,
        )
        .unwrap();
        assert_eq!(vec![0], indexes(&read(&store, 0..10).await));
        assert_eq!(Some(log_id(1, 0)), log_state(&store).await.last_log_id);

        let mut writer = store.clone();
        writer
            .append(vec![blank(1)], IOFlushed::noop())
            .await
            .unwrap();
        assert_eq!(vec![0, 1], indexes(&read(&store, 0..10).await));
    });
}

#[test]
fn truncate_after_drops_the_suffix_across_segments() {
    let dir = TempDir::new("truncate");

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        for i in 0..40 {
            append(&store, vec![blank(i)]).await;
        }
        assert!(wal_files(dir.path()).len() > 1);

        let mut store = store.clone();
        store.truncate_after(Some(log_id(1, 10))).await.unwrap();

        assert_eq!(
            (0..=10).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 10)), log_state(&store).await.last_log_id);

        // The log continues after the new tail.
        append(&store, vec![blank(11)]).await;
        assert_eq!(
            (0..=11).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        drop(store);
    });

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        assert_eq!(
            (0..=11).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 11)), log_state(&store).await.last_log_id);
    });
}

#[test]
fn truncate_after_none_empties_the_log() {
    let dir = TempDir::new("truncate-all");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, (0..5).map(blank).collect()).await;

        let mut store = store.clone();
        store.truncate_after(None).await.unwrap();

        assert!(read(&store, 0..100).await.is_empty());
        assert_eq!(None, log_state(&store).await.last_log_id);

        append(&store, vec![blank(0)]).await;
        assert_eq!(vec![0], indexes(&read(&store, 0..100).await));
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(vec![0], indexes(&read(&store, 0..100).await));
    });
}

#[test]
fn purge_deletes_covered_segments_and_persists_the_marker() {
    let dir = TempDir::new("purge");

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        for i in 0..40 {
            append(&store, vec![blank(i)]).await;
        }
        let before = wal_files(dir.path()).len();
        assert!(before > 1);

        let mut store = store.clone();
        store.purge(log_id(1, 20)).await.unwrap();

        let state = log_state(&store).await;
        assert_eq!(Some(log_id(1, 20)), state.last_purged_log_id);
        assert_eq!(Some(log_id(1, 39)), state.last_log_id);
        assert_eq!(
            (21..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert!(
            wal_files(dir.path()).len() < before,
            "purge should delete fully covered segments"
        );
        drop(store);
    });

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 160).unwrap();
        let state = log_state(&store).await;
        assert_eq!(Some(log_id(1, 20)), state.last_purged_log_id);
        assert_eq!(
            (21..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );

        // Purging everything leaves one empty active segment and a clean get_log_state.
        let mut store = store.clone();
        store.purge(log_id(1, 39)).await.unwrap();
        assert!(read(&store, 0..100).await.is_empty());
        let state = log_state(&store).await;
        assert_eq!(Some(log_id(1, 39)), state.last_purged_log_id);
        assert_eq!(Some(log_id(1, 39)), state.last_log_id);
    });
}

/// Reopening a WAL with many segments must replay every record, not just the
/// active segment. The tiny segment size forces rotation without writing 64 MiB.
#[test]
fn reopen_replays_large_multi_segment_log() {
    let dir = TempDir::new("large-wal");
    const ENTRIES: u64 = 20_000;
    const BATCH: u64 = 500;

    block_on(async {
        let store = Store::open_with_segment_size(dir.path(), 4_096).unwrap();
        let mut next = 0u64;
        while next < ENTRIES {
            let end = (next + BATCH).min(ENTRIES);
            append(&store, (next..end).map(blank).collect()).await;
            next = end;
        }

        assert!(
            wal_files(dir.path()).len() > 10,
            "expected many rotated segments, got {}",
            wal_files(dir.path()).len()
        );
        assert_eq!(
            (0..ENTRIES).collect::<Vec<_>>(),
            indexes(&read(&store, 0..ENTRIES).await)
        );
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..ENTRIES).collect::<Vec<_>>(),
            indexes(&read(&store, 0..ENTRIES).await)
        );

        // The reopened WAL is still appendable.
        append(&store, vec![blank(ENTRIES)]).await;
        let tail = read(&store, ENTRIES - 2..ENTRIES + 1).await;
        assert_eq!(vec![ENTRIES - 2, ENTRIES - 1, ENTRIES], indexes(&tail));
    });
}

#[test]
fn vote_and_committed_survive_reopen() {
    let dir = TempDir::new("hard-state");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, (0..5).map(blank).collect()).await;

        let mut store = store.clone();
        let vote = Vote::new(7, 1);
        store.save_vote(&vote).await.unwrap();
        store.save_committed(Some(log_id(1, 3))).await.unwrap();

        let mut store = store.clone();
        assert_eq!(Some(vote), store.read_vote().await.unwrap());
        assert_eq!(Some(log_id(1, 3)), store.read_committed().await.unwrap());

        // A commit marker is made durable by the next append, exactly as the WAL promises.
        append(&store, vec![blank(5)]).await;
        drop(store);
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        let mut store = store.clone();
        assert_eq!(Some(Vote::new(7, 1)), store.read_vote().await.unwrap());
        assert_eq!(Some(log_id(1, 3)), store.read_committed().await.unwrap());
        assert_eq!(
            (0..6).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
    });
}

// --- State machine tests

async fn fill_sm(sm: &Sm) {
    let mut inner = sm.inner().lock().await;
    inner.last_applied_log = Some(log_id(1, 9));
    inner.last_membership = openraft::StoredMembership::new(Some(log_id(1, 1)), Default::default());

    inner.orderbook.sequence = 100;
    // Two non-crossing bids and one ask, so the book keeps all of them.
    inner
        .orderbook
        .place_order(&mut Order::new(OrderSide::Buy, 10.0, 2.0, 0));
    inner
        .orderbook
        .place_order(&mut Order::new(OrderSide::Buy, 9.0, 3.0, 0));
    inner
        .orderbook
        .place_order(&mut Order::new(OrderSide::Sell, 20.0, 4.0, 0));

    // A parked stop and a last trade price: both travel in snapshots too.
    inner.orderbook.last_trade_price = Some(10.0.into());
    let mut stop = Order::new(OrderSide::Sell, 0.0, 4.0, 0);
    stop.live = OrderType::Market;
    stop.stop_price = 5.0.into();
    stop.tif = TimeInForce::Gtd(9_000);
    stop.timestamp = 1_000;
    inner.orderbook.execute_order(&mut stop);
}

#[test]
fn snapshot_survives_reopen() {
    let dir = TempDir::new("sm-snapshot");

    block_on(async {
        let sm = Sm::open(dir.path()).unwrap();
        fill_sm(&sm).await;
        let meta = sm.persist_snapshot().await.unwrap();
        assert_eq!(Some(log_id(1, 9)), meta.last_log_id);
    });

    block_on(async {
        let sm = Sm::open(dir.path()).unwrap();
        let content = sm.to_content().await;
        assert_eq!(3, content.orders.len());
        assert_eq!(104, content.sequence);
        assert_eq!(1, content.stops.len(), "the parked stop must survive");
        assert_eq!(
            Some(10.0.into()),
            content.last_trade_price,
            "and so must the last trade"
        );

        let mut sm2 = sm.clone();
        let (applied, _) = sm2.applied_state().await.unwrap();
        assert_eq!(Some(log_id(1, 9)), applied);
    });
}

#[test]
fn build_snapshot_is_served_and_persisted() {
    let dir = TempDir::new("sm-build");

    block_on(async {
        let mut sm = Sm::open(dir.path()).unwrap();
        fill_sm(&sm).await;

        let built = sm.build_snapshot().await.unwrap();
        assert_eq!(Some(log_id(1, 9)), built.meta.last_log_id);

        let current = sm
            .get_current_snapshot()
            .await
            .unwrap()
            .expect("snapshot is cached in memory");
        assert_eq!(built.meta.last_log_id, current.meta.last_log_id);
        assert_eq!(
            built.snapshot.into_bytes().unwrap(),
            current.snapshot.into_bytes().unwrap()
        );
    });

    block_on(async {
        let sm = Sm::open(dir.path()).unwrap();
        assert_eq!(3, sm.to_content().await.orders.len());
    });
}

#[test]
fn install_snapshot_replaces_state_and_is_durable() {
    let source_dir = TempDir::new("sm-source");
    let target_dir = TempDir::new("sm-target");

    let snapshot = block_on(async {
        let mut sm = Sm::open(source_dir.path()).unwrap();
        fill_sm(&sm).await;
        sm.build_snapshot().await.unwrap()
    });

    block_on(async {
        let mut target = Sm::open(target_dir.path()).unwrap();
        target
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();

        let content = target.to_content().await;
        assert_eq!(3, content.orders.len());
        assert_eq!(1, content.stops.len(), "the parked stop is installed too");
        assert_eq!(Some(10.0.into()), content.last_trade_price);
    });

    // A restarted node must boot from the installed snapshot even without any log replay.
    block_on(async {
        let target = Sm::open(target_dir.path()).unwrap();
        let content = target.to_content().await;
        assert_eq!(3, content.orders.len());
    });
}

#[test]
fn invalid_snapshot_is_rejected_without_clobbering_state() {
    let dir = TempDir::new("sm-invalid");
    let meta = SnapshotMetaOf::<C> {
        last_log_id: Some(log_id(2, 99)),
        last_membership: Default::default(),
    };

    block_on(async {
        let mut sm = Sm::open(dir.path()).unwrap();
        fill_sm(&sm).await;
        sm.persist_snapshot().await.unwrap();

        let err = sm
            .install_snapshot(
                &meta,
                SnapshotData::memory(Arc::<[u8]>::from(vec![0xff, 0xff, 0xff])),
            )
            .await
            .unwrap_err();
        assert_eq!(std::io::ErrorKind::InvalidData, err.kind());

        // In-memory state is untouched.
        assert_eq!(3, sm.to_content().await.orders.len());
    });

    block_on(async {
        let sm = Sm::open(dir.path()).unwrap();
        assert_eq!(3, sm.to_content().await.orders.len());
    });
}

#[test]
fn corrupt_snapshot_file_fails_to_open() {
    let dir = TempDir::new("sm-corrupt");
    std::fs::write(
        dir.path().join("current.snap"),
        b"MRFSNAP\x01\x00\x00\x00garbage",
    )
    .unwrap();

    let err = Sm::open(dir.path()).unwrap_err();
    assert_eq!(std::io::ErrorKind::InvalidData, err.kind());
}

#[test]
fn bit_flipped_snapshot_fails_the_checksum() {
    let dir = TempDir::new("sm-checksum");

    block_on(async {
        let sm = Sm::open(dir.path()).unwrap();
        fill_sm(&sm).await;
        sm.persist_snapshot().await.unwrap();
    });

    let path = dir.path().join("current.snap");
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let err = Sm::open(dir.path()).unwrap_err();
    assert_eq!(std::io::ErrorKind::InvalidData, err.kind());
}

#[test]
fn state_machine_data_conversion_roundtrips() {
    let data = StateMachineData {
        orders: vec![Order::new(OrderSide::Buy, 1.5, 2.0, 3)],
        sequence: 4,
        stops: Vec::new(),
        last_trade_price: None,
        symbol_config: matchengine_raft::order_book::SymbolConfig::default(),
        symbol_locked: false,
        events: Vec::new(),
        events_evicted: 0,
        submissions: Vec::new(),
        submissions_evicted: 0,
    };

    let bytes = postcard::to_allocvec(&data).unwrap();
    let back: StateMachineData = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(1, back.orders.len());
    assert_eq!(4, back.sequence);
}

#[test]
fn dump_helpers_read_wal_records_and_snapshot_content() {
    let dir = TempDir::new("dump-helpers");

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        append(&store, vec![blank(0), blank(1)]).await;
        let mut store = store.clone();
        store.save_committed(Some(log_id(1, 1))).await.unwrap();

        let sm = Sm::open(dir.path().join("snapshot")).unwrap();
        sm.persist_snapshot().await.unwrap();
    });

    let wal = &wal_files(dir.path())[0];
    let records = read_wal_file::<C>(wal).unwrap();
    let wal_bytes = std::fs::read(wal).unwrap();
    let records_from_bytes = read_wal_bytes::<C>(&wal_bytes).unwrap();
    assert_eq!(records.len(), records_from_bytes.len());
    assert_eq!(
        records
            .iter()
            .map(|(offset, _)| *offset)
            .collect::<Vec<_>>(),
        records_from_bytes
            .iter()
            .map(|(offset, _)| *offset)
            .collect::<Vec<_>>()
    );
    assert!(
        records.iter().any(|(_, record)| matches!(
            record,
            WalRecord::Entry(entry) if entry.log_id().index == 0
        )),
        "WAL dump is missing the first entry"
    );
    assert!(
        records.iter().any(|(_, record)| matches!(
            record,
            WalRecord::Committed(Some(log_id)) if log_id.index == 1
        )),
        "WAL dump is missing the committed marker"
    );

    let snapshot_dir = dir.path().join("snapshot");
    let (meta, data) = read_snapshot_file::<C>(&snapshot_dir)
        .unwrap()
        .expect("snapshot exists");
    let snapshot_bytes = std::fs::read(snapshot_dir.join("current.snap")).unwrap();
    let (meta_from_bytes, data_from_bytes) = read_snapshot_bytes::<C>(&snapshot_bytes).unwrap();
    assert_eq!(meta.last_log_id, meta_from_bytes.last_log_id);
    assert_eq!(data, data_from_bytes);
    let content: StateMachineData = postcard::from_bytes(&data).unwrap();
    assert!(meta.last_log_id.is_none());
    assert!(content.orders.is_empty());
}

#[test]
fn entries_survive_chunked_append_and_mid_batch_rotation() {
    let dir = TempDir::new("chunked-append");

    block_on(async {
        let store =
            Store::open_with_segment_size_fsync_async_wal_and_cap(dir.path(), 160, None, false, 1)
                .unwrap();
        // A one-byte cap puts one entry in each write, forcing both chunking
        // and segment rotation inside one openraft append batch.
        append(&store, (0..40).map(blank).collect()).await;

        assert_eq!(
            (0..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 39)), log_state(&store).await.last_log_id);
        assert!(
            wal_files(dir.path()).len() > 1,
            "the batch crossed segments"
        );
        drop(store);
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..40).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );

        // Recovery can continue appending after a chunked batch.
        append(&store, vec![blank(40)]).await;
        assert_eq!(
            (0..41).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
    });
}

#[test]
fn zero_wal_append_cap_writes_the_whole_batch() {
    let dir = TempDir::new("uncapped-append");

    block_on(async {
        let store = Store::open_with_segment_size_fsync_async_wal_and_cap(
            dir.path(),
            64 * 1024,
            None,
            false,
            0,
        )
        .unwrap();
        append(&store, (0..10).map(blank).collect()).await;
        assert_eq!(
            (0..10).collect::<Vec<_>>(),
            indexes(&read(&store, 0..20).await)
        );
    });
}

#[test]
fn a_mid_batch_append_failure_rolls_back_every_chunk() {
    let dir = TempDir::new("chunked-rollback");

    block_on(async {
        let store =
            Store::open_with_segment_size_fsync_async_wal_and_cap(dir.path(), 160, None, false, 1)
                .unwrap();

        // Fail after three chunks have been written; with a one-byte cap each
        // chunk is one entry, and the tiny segment size forces rotation too.
        store.inject_append_error_after_chunks(3);
        let mut writer = store.clone();
        let error = writer
            .append((0..10).map(blank).collect::<Vec<_>>(), IOFlushed::noop())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("injected"),
            "unexpected error: {error}"
        );

        // No entry is visible and the log state did not advance.
        assert!(read(&store, 0..100).await.is_empty());
        assert_eq!(None, log_state(&store).await.last_log_id);

        // The retry succeeds and writes the whole batch.
        append(&store, (0..10).map(blank).collect()).await;
        assert_eq!(
            (0..10).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
        assert_eq!(Some(log_id(1, 9)), log_state(&store).await.last_log_id);
        drop(store);
    });

    block_on(async {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            (0..10).collect::<Vec<_>>(),
            indexes(&read(&store, 0..100).await)
        );
    });
}
