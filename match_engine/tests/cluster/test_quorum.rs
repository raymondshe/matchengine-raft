//! RAFT-E01: quorum loss and recovery.
//!
//! Existing failover tests kill exactly one voter. This case stops the leader
//! *and* another voter (RaftCore only, so HTTP stays up to observe the
//! survivor), proves the remaining 1-of-3 node refuses service instead of
//! guessing, then restarts one stopped voter on its original storage and
//! requires the cluster to converge on one history with no divergent commit.

use std::collections::BTreeSet;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::ClientConfig;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR1: TestAddr = TestAddr::new(21711);
const ADDR2: TestAddr = TestAddr::new(21712);
const ADDR3: TestAddr = TestAddr::new(21713);

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
    config.heartbeat_min_interval = None;
    // Keep every committed entry in the WAL so the event tape is comparable.
    config.snapshot_policy = SnapshotPolicy::Never;
    config
}

async fn start_node(
    id: u64,
    addr: TestAddr,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), test_config(), storage.clone()).await?)
}

fn client(id: u64, addr: TestAddr) -> MatchEngineClient<C> {
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

async fn place_buy(client: &MatchEngineClient<C>, price: f64) {
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            price,
            1.0,
            0,
        )))
        .await
        .unwrap()
        .unwrap();
}

async fn event_tape(client: &MatchEngineClient<C>) -> Vec<MatchEvent> {
    client
        .events(&EventsRequest {
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap()
        .events
}

async fn orders_json(client: &MatchEngineClient<C>) -> String {
    client.read(&"orderbook_orders".to_string()).await.unwrap()
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
fn quorum_loss_rejects_writes_and_recovers_without_divergence() {
    C::run(async {
        let temp = TempDir::new("quorum");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("quorum")
            .with_snapshot_interval(Duration::ZERO);

        let mut nodes = vec![
            Some(start_node(1, ADDR1, &storage).await.unwrap()),
            Some(start_node(2, ADDR2, &storage).await.unwrap()),
            Some(start_node(3, ADDR3, &storage).await.unwrap()),
        ];
        let clients = [client(1, ADDR1), client(2, ADDR2), client(3, ADDR3)];

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            assert_eq!(1, wait_for_leader(&clients, deadline).await.unwrap());
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

            for i in 0..20u64 {
                place_buy(&clients[0], 100.0 - i as f64 * 0.1).await;
            }
            {
                let deadline = Instant::now() + Duration::from_secs(15);
                for client in &clients {
                    wait_for_sequence(client, "20", deadline).await.unwrap();
                }
            }
            let leader_tape = event_tape(&clients[0]).await;
            let leader_orders = orders_json(&clients[0]).await;
            let survivor_applied_before = data_index(&raw_metrics(ADDR2).await?, "last_applied");

            // Stop the leader and one other voter. Only RaftCore stops: the
            // HTTP/gRPC listeners stay up so the survivor can be observed and
            // its `/write` endpoint can be exercised directly.
            nodes[0].as_ref().unwrap().raft.shutdown().await.unwrap();
            nodes[2].as_ref().unwrap().raft.shutdown().await.unwrap();

            // Two election timeouts (500-1000 ms) plus a margin: no 2-of-3
            // quorum exists, so node 2 must never campaign successfully.
            C::sleep(Duration::from_secs(3)).await;

            let survivor = &clients[1];
            let metrics = survivor.metrics().await?;
            println!(
                "quorum-lost survivor leader={:?} term={} applied={:?}",
                metrics.current_leader,
                metrics.current_term,
                metrics.last_applied.map(|id| id.index)
            );
            assert_ne!(
                Some(2),
                metrics.current_leader,
                "the 1-of-3 survivor must not elect itself"
            );

            // A low-retry client keeps the negative assertion quick and local:
            // no forwarding to the dead voters, no long backoff.
            let quiet = MatchEngineClient::<C>::with_config(
                2,
                ADDR2.to_string(),
                ClientConfig {
                    timeout: Duration::from_millis(500),
                    max_retries: 0,
                    max_forward_retries: 0,
                    ..Default::default()
                },
            );
            let single = MatchEngineRequest::place(Order::new(OrderSide::Buy, 50.0, 1.0, 0));
            let write = quiet.write(&single).await;
            assert!(
                !matches!(write, Ok(Ok(_))),
                "single write was accepted without quorum"
            );
            let batch = quiet.write_batch(&[single.clone(), single.clone()]).await;
            assert!(
                !matches!(batch, Ok(Ok(_))),
                "batch write was accepted without quorum"
            );

            let survivor_applied_after = data_index(&raw_metrics(ADDR2).await?, "last_applied");
            assert_eq!(
                survivor_applied_before, survivor_applied_after,
                "applied index advanced without a quorum"
            );
            assert_eq!(
                "20",
                survivor.read(&"orderbook_sequence".to_string()).await?,
                "sequence changed without a quorum"
            );
            // Local `/read` still serves the last committed book; the
            // linearizable `/events` path cannot succeed without a leader, so
            // the tape is compared only after the quorum is restored below.
            assert_eq!(leader_orders, orders_json(survivor).await);
            assert!(!leader_tape.is_empty());

            // Restart the stopped *non-leader* voter on its original storage
            // and id. Both dead RaftCores still hold their ports, so fully
            // shut down node 3 before rebinding its gRPC listener.
            nodes[2].take().unwrap().shutdown().await.unwrap();
            nodes[2] = Some(start_node(3, ADDR3, &storage).await.unwrap());

            let live = [client(2, ADDR2), client(3, ADDR3)];
            // Node 2 can keep reporting the *stale* leader id 1 until a real
            // election completes, so require both live nodes to agree on a
            // leader that is actually one of them.
            let deadline = Instant::now() + Duration::from_secs(20);
            let new_leader = loop {
                let mut leaders = Vec::new();
                for client in &live {
                    if let Ok(metrics) = client.metrics().await {
                        leaders.push(metrics.current_leader);
                    }
                }
                if leaders.len() == live.len()
                    && leaders[0].is_some()
                    && leaders[0] == leaders[1]
                    && (leaders[0] == Some(2) || leaders[0] == Some(3))
                {
                    break leaders[0].unwrap();
                }
                assert!(
                    Instant::now() <= deadline,
                    "no live leader after recovery: {leaders:?}"
                );
                C::sleep(Duration::from_millis(100)).await;
            };

            let deadline = Instant::now() + Duration::from_secs(20);
            for client in &live {
                wait_for_sequence(client, "20", deadline).await.unwrap();
            }
            // A fresh write gets exactly 21 and reaches both live nodes.
            let leader_client = &live[(new_leader - 2) as usize];
            let response = leader_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    49.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(20), response.data.sequence);
            let deadline = Instant::now() + Duration::from_secs(15);
            for client in &live {
                wait_for_sequence(client, "21", deadline).await.unwrap();
            }

            // Durable state, not just log lines: identical book and tape on
            // both live nodes after the quorum loss and recovery.
            let tape = event_tape(&live[0]).await;
            assert_eq!(tape, event_tape(&live[1]).await);
            assert_eq!(orders_json(&live[0]).await, orders_json(&live[1]).await);
            assert_eq!(tape.len(), 21, "expected one event per committed order");

            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        for node in nodes.into_iter().flatten() {
            if let Err(error) = node.shutdown().await {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}
