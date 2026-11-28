//! RAFT-E02: a restarted former leader with a stale log must not disrupt the
//! healthy quorum.
//!
//! This is the PreVote-on-rejoin **runtime proof**, not an iptables partition.
//! There is deliberately no new network hook: stopping the leader and letting
//! 2/3/4 commit more entries, then restarting it on the same durable state, is
//! enough to give it an old term and a stale log tail. If PreVote were inert,
//! its campaign would inflate the term and depose the healthy leader. The test
//! pins that the term is unchanged across the restart and the single history
//! survives.

use std::collections::BTreeSet;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
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

const ADDR1: TestAddr = TestAddr::new(21714);
const ADDR2: TestAddr = TestAddr::new(21715);
const ADDR3: TestAddr = TestAddr::new(21716);
const ADDR4: TestAddr = TestAddr::new(21717);

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1000;
    config.heartbeat_min_interval = None;
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
            anyhow::bail!("no leader elected (excluded {exclude:?})");
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
fn restarted_former_leader_with_stale_log_cannot_disrupt_quorum() {
    C::run(async {
        let temp = TempDir::new("partition-rejoin");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("partition-rejoin")
            .with_snapshot_interval(Duration::ZERO);
        let addrs = [ADDR1, ADDR2, ADDR3, ADDR4];

        let mut nodes = Vec::new();
        for (index, addr) in addrs.iter().enumerate() {
            nodes.push(Some(
                start_node((index + 1) as u64, *addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients: Vec<MatchEngineClient<C>> = addrs
            .iter()
            .enumerate()
            .map(|(i, addr)| client((i + 1) as u64, *addr))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            assert_eq!(1, wait_for_leader(&clients, None, deadline).await.unwrap());
            for id in 2..=4u64 {
                clients[0]
                    .add_learner((id, addrs[(id - 1) as usize].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            }
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3, 4]))
                .await
                .unwrap()
                .unwrap();
            for i in 0..20u64 {
                place_buy(&clients[0], 100.0 - i as f64 * 0.1).await;
            }
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                wait_for_sequence(client, "20", deadline).await.unwrap();
            }

            // Stop node 1 (the leader). Its WAL/vote are left untouched on
            // disk: that stale state is exactly what the restart below loads.
            nodes[0].as_ref().unwrap().raft.shutdown().await.unwrap();
            nodes[0].take().unwrap().shutdown().await.unwrap();

            let majority = &clients[1..];
            // Survivors can keep reporting the stale leader id 1 for a while;
            // require all three to agree on a leader that is actually alive.
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let healthy = loop {
                let mut leaders = Vec::new();
                for client in majority {
                    if let Ok(metrics) = client.metrics().await {
                        leaders.push(metrics.current_leader);
                    }
                }
                if leaders.len() == majority.len()
                    && leaders[0].is_some()
                    && leaders[0] == leaders[1]
                    && leaders[1] == leaders[2]
                    && (2..=4).contains(&leaders[0].unwrap())
                {
                    break leaders[0].unwrap();
                }
                assert!(
                    Instant::now() <= deadline,
                    "majority did not agree on a live leader: {leaders:?}"
                );
                C::sleep(Duration::from_millis(100)).await;
            };
            let healthy_client = &clients[(healthy - 1) as usize];
            for i in 0..20u64 {
                place_buy(healthy_client, 90.0 - i as f64 * 0.1).await;
            }
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in majority {
                wait_for_sequence(client, "40", deadline).await.unwrap();
            }

            // Capture the term *before* node 1 returns. PreVote means the
            // rejoin must not bump it even though node 1 carries an older vote
            // and a shorter log.
            let term_before = healthy_client.metrics().await?.current_term;
            let applied_before = data_index(
                &raw_metrics(addrs[(healthy - 1) as usize]).await?,
                "last_applied",
            );
            let baseline_tape = event_tape(healthy_client).await;
            let baseline_orders = orders_json(healthy_client).await;

            // Restart the stale former leader on its original storage/id/addr.
            nodes[0] = Some(start_node(1, ADDR1, &storage).await.unwrap());

            // Give the rejoined node ample time to campaign if PreVote were
            // broken, then pin the term and the single-leader invariant.
            C::sleep(Duration::from_secs(15)).await;

            let deadline = Instant::now() + crate::common::wait_secs(10);
            let mut agreed_leader = None;
            while Instant::now() <= deadline {
                let mut leaders = Vec::new();
                for client in &clients {
                    if let Ok(metrics) = client.metrics().await {
                        leaders.push(metrics.current_leader);
                    }
                }
                if leaders.len() == clients.len()
                    && leaders.iter().all(|leader| *leader == leaders[0])
                    && let Some(leader) = leaders[0]
                {
                    agreed_leader = Some(leader);
                    break;
                }
                C::sleep(Duration::from_millis(100)).await;
            }
            let agreed_leader = agreed_leader.expect("cluster did not converge on one leader");
            assert_eq!(
                healthy, agreed_leader,
                "healthy leader was deposed by the rejoin"
            );

            let term_after = healthy_client.metrics().await?.current_term;
            assert_eq!(
                term_before, term_after,
                "stale node rejoined with a higher term (PreVote proof failed)"
            );

            // Node 1 is a follower, not a leader with a divergent log.
            let rejoined_metrics = clients[0].metrics().await?;
            assert_eq!(
                Some(healthy),
                rejoined_metrics.current_leader,
                "rejoined node does not follow the healthy leader"
            );
            let rejoined_metrics = raw_metrics(ADDR1).await?;
            assert!(
                data_index(&rejoined_metrics, "last_applied") >= applied_before,
                "rejoined node's applied index regressed below the pre-rejoin frontier"
            );

            let deadline = Instant::now() + crate::common::wait_secs(20);
            for client in &clients {
                wait_for_sequence(client, "40", deadline).await.unwrap();
            }
            for client in &clients {
                assert_eq!(baseline_tape, event_tape(client).await);
                assert_eq!(baseline_orders, orders_json(client).await);
            }

            // A new write commits to 41 on every node.
            let response = healthy_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    80.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(40), response.data.sequence);
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                wait_for_sequence(client, "41", deadline).await.unwrap();
            }
            let tape = event_tape(&clients[0]).await;
            assert_eq!(tape.len(), 41);
            for client in &clients {
                assert_eq!(tape, event_tape(client).await);
            }

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
