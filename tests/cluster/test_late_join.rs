//! A voter that falls far behind must recover from the live cluster.
//!
//! This is stricter than an empty learner joining: the restarted node has an
//! older snapshot and WAL of its own, misses many committed writes while it is
//! down, and must replace/advance that state from the leader or the other voter.

use std::collections::BTreeSet;
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
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR1: &str = "127.0.0.1:21601";
const ADDR2: &str = "127.0.0.1:21602";
const ADDR3: &str = "127.0.0.1:21603";

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
    config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
    config.max_in_snapshot_log_to_keep = 0;
    config
}

async fn start_node(
    id: u64,
    addr: &str,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), test_config(), storage.clone()).await?)
}

fn client(id: u64, addr: &str) -> MatchEngineClient<C> {
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
