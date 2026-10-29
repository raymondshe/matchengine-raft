//! Membership and failover coverage.
//!
//! `test_cluster` covers a controlled membership shrink, and `test_snapshot`
//! covers a learner installing a snapshot after the log was purged. These tests
//! cover the two remaining paths:
//!
//! * a leader crash: a surviving voter takes over and keeps the committed book;
//! * a new learner catches up from a live leader through normal log replication.

use std::collections::BTreeSet;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
    config
}

async fn start_node(
    id: u64,
    addr: &str,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(
        id,
        addr.to_string(),
        Transport::Grpc,
        test_config(),
        storage.clone(),
    )
    .await?)
}

async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    exclude: Option<u64>,
    deadline: Instant,
) -> anyhow::Result<u64> {
    loop {
        for client in clients {
            if let Ok(metrics) = client.metrics().await
                && let Some(leader) = metrics.current_leader
                && Some(leader) != exclude
            {
                return Ok(leader);
            }
        }

        if Instant::now() > deadline {
            anyhow::bail!("no leader was elected (excluded {exclude:?})");
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
            anyhow::bail!("state machine did not reach sequence {expected}");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn read_orders(client: &MatchEngineClient<C>) -> anyhow::Result<Vec<Order>> {
    let json = client.read(&"orderbook_orders".to_string()).await?;
    Ok(serde_json::from_str(&json)?)
}

fn addr(index: usize) -> &'static str {
    ["127.0.0.1:21401", "127.0.0.1:21402", "127.0.0.1:21403"][index]
}

fn client(id: u64) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(id, addr((id - 1) as usize).to_string())
}

/// Kill the elected leader: a remaining voter must take over, preserve the
/// committed orders and accept new writes.
#[test]
fn leader_failover_keeps_committed_data() {
    C::run(async {
        let temp = TempDir::new("failover");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("failover");
        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for id in 1..=3u64 {
            nodes.push(Some(
                start_node(id, addr((id - 1) as usize), &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients: Vec<MatchEngineClient<C>> = (1..=3u64).map(client).collect();

        clients[0].init().await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let leader = wait_for_leader(&clients, None, deadline).await.unwrap();
        assert_eq!(1, leader, "the initialised node should lead");

        clients[0]
            .add_learner((2, addr(1).to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .add_learner((3, addr(2).to_string()))
            .await
            .unwrap()
            .unwrap();
        clients[0]
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();

        for i in 0..3u64 {
            clients[0]
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0 - i as f64,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        }

        // Wait until both followers have applied the committed entries.
        let deadline = Instant::now() + Duration::from_secs(10);
        for id in 2..=3u64 {
            wait_for_sequence(&clients[(id - 1) as usize], "3", deadline)
                .await
                .unwrap();
        }

        let leader = wait_for_leader(&clients, None, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        let leader_idx = (leader - 1) as usize;
        nodes[leader_idx]
            .take()
            .expect("leader node")
            .shutdown()
            .await
            .unwrap();

        // Two voters remain; wait for one of them to be elected.
        let deadline = Instant::now() + Duration::from_secs(20);
        let new_leader = wait_for_leader(&clients, Some(leader), deadline)
            .await
            .unwrap();
        assert_ne!(leader, new_leader);

        // The surviving leader and the other voter still have the full book.
        for id in 1..=3u64 {
            if id == leader {
                continue;
            }
            wait_for_sequence(&clients[(id - 1) as usize], "3", deadline)
                .await
                .unwrap();
            let orders = read_orders(&clients[(id - 1) as usize]).await.unwrap();
            assert_eq!(3, orders.len(), "node {id} lost the committed book");
        }

        // The new leader accepts a write and replicates it to the survivor.
        // The first attempt can still see `LeaseExpired` while the new term
        // settles, so retry until the write is actually accepted.
        let new_client = client(new_leader);
        let deadline = Instant::now() + Duration::from_secs(10);
        let response = loop {
            match new_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    90.0,
                    2.0,
                    0,
                )))
                .await
            {
                Ok(Ok(response)) => break response,
                Ok(Err(error)) => {
                    println!("write retry after failover: {error}");
                }
                Err(error) => {
                    println!("write transport retry after failover: {error}");
                }
            }
            if Instant::now() > deadline {
                panic!("new leader never accepted a write after failover");
            }
            C::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(Some(3), response.data.sequence);

        let deadline = Instant::now() + Duration::from_secs(10);
        for id in 1..=3u64 {
            if id == leader {
                continue;
            }
            wait_for_sequence(&clients[(id - 1) as usize], "4", deadline)
                .await
                .unwrap();
            let orders = read_orders(&clients[(id - 1) as usize]).await.unwrap();
            assert_eq!(4, orders.len(), "node {id} missed the post-failover write");
        }

        for node in nodes.into_iter().flatten() {
            node.shutdown().await.unwrap();
        }
    });
}

/// Add a learner after the leader has committed data and let it catch up through
/// normal log replication, then promote it and write through it.
#[test]
fn new_learner_catches_up_from_live_log() {
    C::run(async {
        let temp = TempDir::new("learner");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("learner");
        let addr1 = "127.0.0.1:21501";
        let addr2 = "127.0.0.1:21502";
        let node1 = start_node(1, addr1, &storage).await.unwrap();
        let node2 = start_node(2, addr2, &storage).await.unwrap();
        let client1 = MatchEngineClient::<C>::new(1, addr1.to_string());
        let client2 = MatchEngineClient::<C>::new(2, addr2.to_string());

        client1.init().await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        assert_eq!(
            1,
            wait_for_leader(std::slice::from_ref(&client1), None, deadline)
                .await
                .unwrap()
        );

        for i in 0..5u64 {
            client1
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    200.0 - i as f64,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        }
        wait_for_sequence(&client1, "5", deadline).await.unwrap();

        // Node 2 starts empty; the learner must receive the existing log tail.
        client1
            .add_learner((2, addr2.to_string()))
            .await
            .unwrap()
            .unwrap();
        wait_for_sequence(&client2, "5", deadline).await.unwrap();
        assert_eq!(5, read_orders(&client2).await.unwrap().len());

        // Promote and write through the former learner.
        client1
            .change_membership(&BTreeSet::from([1, 2]))
            .await
            .unwrap()
            .unwrap();
        let response = client2
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                190.0,
                1.0,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(5), response.data.sequence);
        wait_for_sequence(&client1, "6", deadline).await.unwrap();
        assert_eq!(6, read_orders(&client1).await.unwrap().len());

        node1.shutdown().await.unwrap();
        node2.shutdown().await.unwrap();
    });
}
