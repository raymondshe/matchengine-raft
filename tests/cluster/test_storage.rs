//! Storage-layer tests: WAL append/read/recovery and snapshot persistence.
//!
//! These exercise the storage types directly, without starting a Raft node.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::matchengine::Order;
use matchengine_raft::matchengine::OrderSide;
use matchengine_raft::matchengine::OrderType;
use matchengine_raft::matchengine::TimeInForce;
use matchengine_raft::store::StateMachineData;
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
        assert_eq!(built.snapshot.into_inner(), current.snapshot.into_inner());
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
            .install_snapshot(&meta, std::io::Cursor::new(vec![0xff, 0xff, 0xff]))
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
