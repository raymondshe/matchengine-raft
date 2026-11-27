//! A voter that falls far behind must recover from the live cluster.
//!
//! This is stricter than an empty learner joining: the restarted node has an
//! older snapshot and WAL of its own, misses many committed writes while it is
//! down, and must replace/advance that state from the leader or the other voter.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineNode;
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
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR1: TestAddr = TestAddr::new(21601);
const ADDR2: TestAddr = TestAddr::new(21602);
const ADDR3: TestAddr = TestAddr::new(21603);
const ADDR4: TestAddr = TestAddr::new(21611);
const ADDR5: TestAddr = TestAddr::new(21612);
const ADDR6: TestAddr = TestAddr::new(21613);

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.heartbeat_min_interval = None;
    config.election_timeout_max = 1_000;
    config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
    config.max_in_snapshot_log_to_keep = 0;
    config
}

async fn start_node(
    id: u64,
    addr: impl std::fmt::Display,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), test_config(), storage.clone()).await?)
}

fn client(id: u64, addr: impl std::fmt::Display) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(id, addr.to_string())
}

async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    deadline: Instant,
) -> anyhow::Result<u64> {
    loop {
        for client in clients {
            if let Ok(metrics) = client.metrics().await
                && let Some(leader) = metrics.current_leader
            {
                return Ok(leader);
            }
        }
        if Instant::now() > deadline {
            anyhow::bail!("cluster did not elect a leader");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_sequence(
    client: &MatchEngineClient<C>,
    expected: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
            && value == expected
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("node did not reach sequence {expected}");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn order_count(client: &MatchEngineClient<C>) -> anyhow::Result<usize> {
    let json = client.read(&"orderbook_orders".to_string()).await?;
    Ok(serde_json::from_str::<Vec<Order>>(&json)?.len())
}

async fn place_buy(client: &MatchEngineClient<C>, price: f64) -> anyhow::Result<()> {
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            price,
            1.0,
            0,
        )))
        .await??;
    Ok(())
}

/// Highest entry log index durably stored in a node's WAL segments. `read_wal_file`
/// returns byte offsets in its first tuple element, so the index must come from
/// the entry itself. Reading the files (rather than the live node) lets the test
/// observe the lag of a node that is still stopped — the state a late starter
/// actually begins from.
fn persisted_wal_index(log_dir: &Path) -> u64 {
    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "node has no WAL segment in {}",
        log_dir.display()
    );
    files
        .iter()
        .flat_map(|path| read_wal_file::<C>(path).unwrap())
        .filter_map(|(_, record)| match record {
            WalRecord::Entry(entry) => Some(entry.log_id.index),
            WalRecord::Committed(_) => None,
        })
        .max()
        .unwrap_or(0)
}

async fn raw_metrics(addr: TestAddr) -> anyhow::Result<Value> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let response = http.get(format!("http://{addr}/metrics")).send().await?;
    Ok(response.json::<Value>().await?)
}

fn data_index(metrics: &Value, field: &str) -> u64 {
    metrics["Ok"]["data_metrics"][field]["index"]
        .as_u64()
        .unwrap_or(0)
}

#[test]
fn stale_follower_catches_up_from_live_members() {
    C::run(async {
        let temp = TempDir::new("late-join");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("late-join");

        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (index, addr) in [ADDR1, ADDR2, ADDR3].iter().enumerate() {
            nodes.push(Some(
                start_node((index + 1) as u64, addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients = [client(1, ADDR1), client(2, ADDR2), client(3, ADDR3)];

        clients[0].init().await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        assert_eq!(
            1,
            wait_for_leader(&clients, deadline).await.unwrap(),
            "node 1 should lead the fresh cluster"
        );
        clients[0]
            .add_learner((2, ADDR2.to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .add_learner((3, ADDR3.to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();

        // Three committed orders, present on the node that will go stale.
        for price in [100.0, 99.0, 98.0] {
            place_buy(&clients[0], price).await.unwrap();
        }
        wait_for_sequence(&clients[1], "3", deadline).await.unwrap();

        // Give the soon-to-be-offline node a real, but stale, snapshot on disk.
        let stale_snapshot = nodes[1]
            .as_ref()
            .unwrap()
            .state_machine
            .persist_snapshot()
            .await
            .unwrap();
        let stale_index = stale_snapshot
            .last_log_id
            .expect("stale node has an applied log id")
            .index;
        assert!(
            storage.snapshot_dir(2).join("current.snap").exists(),
            "stale snapshot file must exist before the node stops"
        );

        // Remove the soon-to-be-stale node first. This stops the leader's
        // inbound AppendStream without racing a graceful shutdown, then the node
        // falls behind while the remaining majority commits new writes.
        clients[0]
            .change_membership(&BTreeSet::from([1, 3]))
            .await
            .unwrap()
            .unwrap();
        C::sleep(Duration::from_millis(500)).await;
        nodes[1].take().unwrap().shutdown().await.unwrap();
        for i in 0..40u64 {
            place_buy(&clients[0], 90.0 - i as f64 * 0.1).await.unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let metrics = clients[0].metrics().await.unwrap();
            let snapshot = metrics.snapshot.map(|log_id| log_id.index);
            let purged = metrics.purged.map(|log_id| log_id.index);
            if let (Some(snapshot), Some(purged)) = (snapshot, purged)
                && snapshot > stale_index
                && purged >= snapshot
            {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "leader did not snapshot/purge past the stale node: stale={stale_index}, snapshot={snapshot:?}, purged={purged:?}"
                );
            }
            C::sleep(Duration::from_millis(100)).await;
        }

        // Observe the lag on the node's *persisted* WAL while it is still
        // stopped: this is the real starting state of a late starter, not a
        // value inferred from the leader's replication progress.
        let leader_commit = clients[0]
            .metrics()
            .await
            .unwrap()
            .last_log_index
            .unwrap_or(0);
        let stale_wal = persisted_wal_index(&storage.log_dir(2));
        assert!(
            stale_wal < leader_commit,
            "expected a persisted WAL lag on the removed node: wal={stale_wal}, leader={leader_commit}"
        );

        // Restart the stale voter on its old storage. Its WAL and snapshot are
        // both behind; the leader must bring it back to the live state.
        nodes[1] = Some(start_node(2, ADDR2, &storage).await.unwrap());
        clients[0]
            .add_learner((2, ADDR2.to_string()))
            .await
            .unwrap()
            .unwrap();
        let restored = client(2, ADDR2);
        let deadline = Instant::now() + Duration::from_secs(20);
        wait_for_sequence(&restored, "43", deadline).await.unwrap();
        assert_eq!(43, order_count(&restored).await.unwrap());

        // The leader and the other voter still serve the same state.
        wait_for_sequence(&clients[0], "43", deadline)
            .await
            .unwrap();
        wait_for_sequence(&clients[2], "43", deadline)
            .await
            .unwrap();
        assert_eq!(43, order_count(&clients[0]).await.unwrap());
        assert_eq!(43, order_count(&clients[2]).await.unwrap());

        // Stop the leader first so the remaining nodes have no inbound
        // replication stream while they shut down.
        if let Some(node) = nodes[0].take() {
            node.shutdown().await.unwrap();
        }
        if let Some(node) = nodes[2].take() {
            node.shutdown().await.unwrap();
        }
        if let Some(node) = nodes[1].take() {
            node.shutdown().await.unwrap();
        }
    });
}

/// A voter that starts late with its own WAL and snapshot on disk must rejoin
/// the quorum, not just be re-added as an empty learner.
///
/// Unlike the test above, this one deliberately keeps the stopped node in the
/// three-voter membership. The leader therefore has replication progress for
/// that exact node id when it returns, which is the case that stuck in the
/// no-fsync lab: heartbeats alone did not backfill the missing entries.
#[test]
fn late_started_voter_with_persistent_wal_joins_quorum() {
    C::run(async {
        let temp = TempDir::new("late-join-persisted");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("late-join-persisted");
        let addrs = [ADDR4, ADDR5, ADDR6];

        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (index, addr) in addrs.iter().enumerate() {
            nodes.push(Some(
                start_node((index + 1) as u64, addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients = [client(1, ADDR4), client(2, ADDR5), client(3, ADDR6)];

        clients[0].init().await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        assert_eq!(1, wait_for_leader(&clients, deadline).await.unwrap());
        clients[0]
            .add_learner((2, ADDR5.to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .add_learner((3, ADDR6.to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();

        for price in [100.0, 99.0, 98.0] {
            place_buy(&clients[0], price).await.unwrap();
        }
        wait_for_sequence(&clients[1], "3", deadline).await.unwrap();
        wait_for_sequence(&clients[2], "3", deadline).await.unwrap();

        // The late starter is an OLD node: give it a real snapshot and a real
        // WAL on disk, both of which survive the restart below.
        let snapshot = nodes[1]
            .as_ref()
            .unwrap()
            .state_machine
            .persist_snapshot()
            .await
            .unwrap();
        let snapshot_index = snapshot.last_log_id.unwrap().index;
        assert!(
            storage.snapshot_dir(2).join("current.snap").exists(),
            "the returning node must have a durable snapshot"
        );
        let wal_dir = storage.log_dir(2);
        assert!(
            persisted_wal_index(&wal_dir) >= snapshot_index,
            "the returning node must have a durable WAL past its snapshot"
        );

        // Stop the voter but leave the membership alone. The leader keeps its
        // progress for node 2 while node 1 and node 3 commit new writes.
        nodes[1].take().unwrap().shutdown().await.unwrap();
        for i in 0..40u64 {
            place_buy(&clients[0], 90.0 - i as f64 * 0.1).await.unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let metrics = clients[0].metrics().await.unwrap();
            if let (Some(snapshot), Some(purged)) = (metrics.snapshot, metrics.purged)
                && snapshot.index > snapshot_index
                && purged.index >= snapshot.index
            {
                break;
            }
            if Instant::now() > deadline {
                panic!("leader did not snapshot/purge past the late starter's snapshot");
            }
            C::sleep(Duration::from_millis(100)).await;
        }
        let leader_commit = clients[0]
            .metrics()
            .await
            .unwrap()
            .last_log_index
            .unwrap_or(0);

        // Observe the lag on the OLD node's persisted WAL. This is the state
        // the node starts from when it is finally launched.
        let stale_wal = persisted_wal_index(&wal_dir);
        assert!(
            stale_wal < leader_commit,
            "expected a persisted WAL lag on the late starter: wal={stale_wal}, leader={leader_commit}"
        );
        println!("late starter persisted WAL index {stale_wal} lags leader commit {leader_commit}");

        // Restart the same node id on the same durable state. No add_learner,
        // no membership rewrite: it must rejoin the existing quorum itself.
        nodes[1] = Some(start_node(2, ADDR5, &storage).await.unwrap());
        let late = client(2, ADDR5);
        let deadline = Instant::now() + Duration::from_secs(25);
        wait_for_sequence(&late, "43", deadline).await.unwrap();
        assert_eq!(43, order_count(&late).await.unwrap());

        // Its live Raft log and applied index crossed the observed lag.
        let late_metrics = raw_metrics(ADDR5).await.unwrap();
        let late_log = data_index(&late_metrics, "last_log");
        let late_applied = data_index(&late_metrics, "last_applied");
        assert!(
            late_log >= leader_commit && late_applied >= leader_commit,
            "late starter did not close the WAL lag: log={late_log}, applied={late_applied}, leader={leader_commit}"
        );

        // Prove it is a voting member of the quorum: with node 3 stopped,
        // node 1 plus the recovered node 2 are the only live voters, and a new
        // committed write must still reach the late starter.
        nodes[2].take().unwrap().shutdown().await.unwrap();
        place_buy(&clients[0], 50.0).await.unwrap();
        wait_for_sequence(&late, "44", Instant::now() + Duration::from_secs(15))
            .await
            .unwrap();
        assert_eq!(44, order_count(&late).await.unwrap());
        wait_for_sequence(&clients[0], "44", Instant::now() + Duration::from_secs(15))
            .await
            .unwrap();

        if let Some(node) = nodes[0].take() {
            node.shutdown().await.unwrap();
        }
        if let Some(node) = nodes[1].take() {
            node.shutdown().await.unwrap();
        }
    });
}
