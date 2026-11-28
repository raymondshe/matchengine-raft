//! Raft path for the prototype async WAL.

#![cfg(feature = "async-wal")]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::LogStore;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
use openraft::Entry;
use openraft::LogId;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryOf;
use openraft::alias::LogIdOf;
use openraft::entry::RaftEntry;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use openraft::type_config::TypeConfigExt;
use openraft::vote::RaftLeaderId;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR: TestAddr = TestAddr::new(21931);

#[test]
fn async_wal_node_commits_and_recovers() {
    C::run(async {
        let temp = TempDir::new("async-wal-node");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("async-wal-node")
            .with_wal_fsync(Some(Duration::ZERO))
            .with_async_wal(true)
            .with_min_voters(1);

        let node =
            build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage.clone())
                .await
                .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        for index in 0..10 {
            client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0 + index as f64,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        }
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(10, orders.len());
        node.shutdown().await.unwrap();

        // Reopen the same durable state with the async worker enabled again.
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        wait_for_leader(&client).await;
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(10, orders.len());
        node.shutdown().await.unwrap();
    });
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + crate::common::wait_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return;
        }
        assert!(Instant::now() <= deadline, "node 1 never became leader");
        C::sleep(Duration::from_millis(100)).await;
    }
}

// --- RAFT-E19: storage-only pending-tail and structural-change gate cases ---

fn log_id(index: u64) -> LogIdOf<C> {
    LogId::new(CommittedLeaderIdOf::<C>::new(1, 1), index)
}

fn blank(index: u64) -> EntryOf<C> {
    Entry::new_blank(log_id(index))
}

fn wal_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    files
}

/// Entry indexes decoded from durable WAL frames (no in-memory pending cache).
fn disk_indexes(dir: &Path) -> Vec<u64> {
    wal_files(dir)
        .iter()
        .flat_map(|path| read_wal_file::<C>(path).unwrap())
        .filter_map(|(_, record)| match record {
            WalRecord::Entry(entry) => Some(entry.log_id.index),
            WalRecord::Committed(_) => None,
        })
        .collect()
}

async fn visible_indexes(store: &LogStore, range: std::ops::Range<u64>) -> Vec<u64> {
    let mut reader = store.clone();
    reader
        .try_get_log_entries(range)
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.log_id().index)
        .collect()
}

async fn last_log_index(store: &LogStore) -> Option<u64> {
    let mut store = store.clone();
    store
        .get_log_state()
        .await
        .unwrap()
        .last_log_id
        .map(|id| id.index)
}

/// A delayed fsync worker leaves entries readable through the pending cache but
/// not durable. Simulating the crash by truncating back to the last durable
/// offset must not resurrect that tail on reopen; `truncate_after` and `purge`
/// must flush pending appends before changing the log structure.
#[test]
fn async_wal_pending_tail_is_not_resurrected_and_truncate_flushes_pending() {
    let temp = TempDir::new("async-wal-pending-tail");

    C::run(async {
        let dir = temp.path().join("lost-tail");
        let store =
            LogStore::open_with_fsync_and_async_wal(&dir, Some(Duration::ZERO), true).unwrap();
        store.start_async_wal_worker().unwrap();
        let (tx, rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(1)], IOFlushed::signal(tx))
            .await
            .unwrap();
        rx.await.unwrap().unwrap();
        let durable_len = std::fs::metadata(wal_files(&dir).last().unwrap())
            .unwrap()
            .len();
        drop(writer);
        drop(store);

        // Delay the fsync worker so the second append stays pending.
        let store =
            LogStore::open_with_fsync_and_async_wal(&dir, Some(Duration::ZERO), true).unwrap();
        store
            .start_async_wal_worker_with_delay(crate::common::wait_secs(60))
            .unwrap();
        let (tx, mut rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(2)], IOFlushed::signal(tx))
            .await
            .unwrap();
        assert!(rx.try_recv().is_err(), "IOFlushed must wait for the fsync");
        assert_eq!(vec![1, 2], visible_indexes(&store, 0..10).await);
        assert_eq!(Some(2), last_log_index(&store).await);
        drop(writer);
        drop(store);
        C::yield_now().await;

        // Model the power cut: the pending frame never reached stable storage.
        let last = wal_files(&dir).last().unwrap().clone();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&last)
            .unwrap()
            .set_len(durable_len)
            .unwrap();
        let store =
            LogStore::open_with_fsync_and_async_wal(&dir, Some(Duration::ZERO), false).unwrap();
        assert_eq!(vec![1], visible_indexes(&store, 0..10).await);
        assert_eq!(Some(1), last_log_index(&store).await);
        assert_eq!(vec![1], disk_indexes(&dir));
        // The recovered log is contiguous and accepts the lost index again.
        let mut writer = store.clone();
        writer
            .append(vec![blank(2)], IOFlushed::noop())
            .await
            .unwrap();
        assert_eq!(vec![1, 2], visible_indexes(&store, 0..10).await);
        assert_eq!(vec![1, 2], disk_indexes(&dir));
        drop(writer);
        drop(store);

        // truncate_after must flush a pending append before cutting the tail.
        let gate_dir = temp.path().join("truncate-gate");
        let store =
            LogStore::open_with_fsync_and_async_wal(&gate_dir, Some(Duration::ZERO), true).unwrap();
        store
            .start_async_wal_worker_with_delay(Duration::from_millis(300))
            .unwrap();
        let (tx, mut rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(1), blank(2), blank(3)], IOFlushed::signal(tx))
            .await
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "batch is still pending before truncate"
        );
        writer.truncate_after(Some(log_id(1))).await.unwrap();
        assert!(
            rx.try_recv().is_ok_and(|result| result.is_ok()),
            "truncate_after returned before the pending appends were flushed"
        );
        assert_eq!(vec![1], visible_indexes(&store, 0..10).await);
        assert_eq!(Some(1), last_log_index(&store).await);
        assert_eq!(vec![1], disk_indexes(&gate_dir));
        drop(writer);
        drop(store);

        // purge must apply the same flush gate and leave a contiguous suffix.
        let mut store =
            LogStore::open_with_fsync_and_async_wal(&gate_dir, Some(Duration::ZERO), true).unwrap();
        store
            .start_async_wal_worker_with_delay(Duration::from_millis(300))
            .unwrap();
        let (tx, mut rx) = C::oneshot();
        let mut writer = store.clone();
        writer
            .append(vec![blank(2), blank(3), blank(4)], IOFlushed::signal(tx))
            .await
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "batch is still pending before purge"
        );
        writer.purge(log_id(2)).await.unwrap();
        assert!(
            rx.try_recv().is_ok_and(|result| result.is_ok()),
            "purge returned before the pending appends were flushed"
        );
        assert_eq!(
            Some(2),
            store
                .get_log_state()
                .await
                .unwrap()
                .last_purged_log_id
                .map(|id| id.index)
        );
        assert_eq!(vec![3, 4], visible_indexes(&store, 0..10).await);
        let mut next = store.clone();
        next.append(vec![blank(5)], IOFlushed::noop())
            .await
            .unwrap();
        assert_eq!(vec![3, 4, 5], visible_indexes(&store, 0..10).await);
        drop(next);
        drop(writer);
        drop(store);
    });
}
