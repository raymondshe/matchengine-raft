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

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
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

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.heartbeat_min_interval = None;
    config.election_timeout_max = 1000;
    config
}

async fn start_node(
    id: u64,
    addr: impl std::fmt::Display,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), test_config(), storage.clone()).await?)
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

fn addr(index: usize) -> String {
    TestAddr::new(21401 + index as u16).to_string()
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
        let deadline = Instant::now() + crate::common::wait_secs(15);
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
        let deadline = Instant::now() + crate::common::wait_secs(10);
        for id in 2..=3u64 {
            wait_for_sequence(&clients[(id - 1) as usize], "3", deadline)
                .await
                .unwrap();
        }

        let leader = wait_for_leader(&clients, None, Instant::now() + crate::common::wait_secs(5))
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
        let deadline = Instant::now() + crate::common::wait_secs(20);
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
        let deadline = Instant::now() + crate::common::wait_secs(10);
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

        let deadline = Instant::now() + crate::common::wait_secs(10);
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
        let addr1 = TestAddr::new(21501);
        let addr2 = TestAddr::new(21502);
        let node1 = start_node(1, addr1, &storage).await.unwrap();
        let node2 = start_node(2, addr2, &storage).await.unwrap();
        let client1 = MatchEngineClient::<C>::new(1, addr1.to_string());
        let client2 = MatchEngineClient::<C>::new(2, addr2.to_string());

        client1.init().await.unwrap().unwrap();
        let deadline = Instant::now() + crate::common::wait_secs(15);
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

/// RAFT-E05: interrupt a 3 -> 4 voter change by stopping the leader mid-flight.
///
/// The survivors must converge on one of the two *final* configurations
/// ({1,2,3} if the change never committed, {1,2,3,4} if it did) and never expose
/// a partial/joint set as the cluster's configuration. A change issued on a
/// follower is forwarded or rejected without splitting the cluster.
#[test]
fn interrupted_membership_change_converges_to_a_valid_config() {
    C::run(async {
        let temp = TempDir::new("membership-interrupt");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("membership-interrupt")
            .with_snapshot_interval(Duration::ZERO);
        let addrs = [
            TestAddr::new(21721),
            TestAddr::new(21722),
            TestAddr::new(21723),
            TestAddr::new(21724),
        ];

        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.election_timeout_max = 1000;
        config.heartbeat_min_interval = None;
        config.snapshot_policy = SnapshotPolicy::Never;

        let mut nodes = Vec::new();
        for (index, addr) in addrs.iter().enumerate() {
            nodes.push(Some(
                build_match_engine_node(
                    (index + 1) as u64,
                    addr.to_string(),
                    config.clone(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            ));
        }
        let clients: Vec<MatchEngineClient<C>> = addrs
            .iter()
            .enumerate()
            .map(|(i, addr)| client_for((i + 1) as u64, *addr))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            assert_eq!(
                1,
                wait_for_leader(&clients[..3], None, deadline)
                    .await
                    .unwrap()
            );
            for id in 2..=4u64 {
                clients[0]
                    .add_learner((id, addrs[(id - 1) as usize].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            }
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3]))
                .await
                .unwrap()
                .unwrap();
            for i in 0..10u64 {
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
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients[..3] {
                wait_for_sequence(client, "10", deadline).await.unwrap();
            }

            // Fire the change and shut the leader down before it can commit.
            // Whether the first (joint) entry made it to disk or not, the
            // survivors must converge to one of the two final configs.
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let requested = BTreeSet::from([1u64, 2, 3, 4]);
            let in_flight = http
                .post(format!("http://{}/change-membership", addrs[0]))
                .json(&requested)
                .send();
            let _ = C::timeout(Duration::from_millis(5), in_flight).await;
            nodes[0].as_ref().unwrap().raft.shutdown().await.unwrap();
            nodes[0].take().unwrap().shutdown().await.unwrap();

            let survivors = &clients[1..];
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let new_leader = wait_for_leader(survivors, Some(1), deadline).await.unwrap();
            let leader_client = &clients[(new_leader - 1) as usize];
            println!("membership change interrupted; new leader {new_leader}");

            // Poll until OpenRaft has finalized a single config set: a joint
            // config is a legitimate transient, but it must not be the result.
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let voters = loop {
                if let Ok(metrics) = leader_client.metrics().await {
                    let joint = metrics.membership_config.membership().get_joint_config();
                    if joint.len() == 1 {
                        let set = joint[0].clone();
                        assert!(
                            set == BTreeSet::from([1, 2, 3]) || set == BTreeSet::from([1, 2, 3, 4]),
                            "interrupted change converged to a partial config: {set:?}"
                        );
                        break set;
                    }
                }
                assert!(Instant::now() <= deadline, "membership never finalized");
                C::sleep(Duration::from_millis(100)).await;
            };

            // Retrying add_learner is idempotent when the learner is still a
            // learner. If 4 was already promoted, it must simply not be added
            // as a learner again.
            if voters == BTreeSet::from([1, 2, 3]) {
                leader_client
                    .add_learner((4, addrs[3].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                let metrics = leader_client.metrics().await?;
                assert!(
                    metrics
                        .membership_config
                        .membership()
                        .voter_ids()
                        .any(|id| id == 4),
                    "node 4 must be a voter in the four-voter outcome"
                );
            }

            // Writes commit after the interrupted change and every live node
            // sees the same sequence and event tape.
            let response = leader_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    90.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(10), response.data.sequence);
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in survivors {
                wait_for_sequence(client, "11", deadline).await.unwrap();
            }
            let tape = event_tape(leader_client).await;
            for client in survivors {
                assert_eq!(tape, event_tape(client).await);
            }

            // A change posted to a follower is rejected/forwarded, never
            // applied locally as a second, conflicting configuration.
            let follower_id = survivors
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    let id = (index + 2) as u64;
                    (id != new_leader).then_some(id)
                })
                .expect("a live follower exists");
            let follower_client = &clients[(follower_id - 1) as usize];
            let follower_result = follower_client
                .change_membership(&BTreeSet::from([1, 2, 3, 4]))
                .await;
            println!(
                "follower change-membership result ok={}",
                follower_result.is_ok()
            );

            let metrics = leader_client.metrics().await?;
            let joint = metrics.membership_config.membership().get_joint_config();
            assert_eq!(
                1,
                joint.len(),
                "follower change split the cluster into a joint config: {joint:?}"
            );
            assert_eq!(voters, joint[0]);

            let response = leader_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    89.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(11), response.data.sequence);
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in survivors {
                wait_for_sequence(client, "12", deadline).await.unwrap();
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

fn client_for(id: u64, addr: TestAddr) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(id, addr.to_string())
}

/// RAFT-E18: two learners added concurrently must both install the purged
/// snapshot, then be promotable one at a time without a joint config left over.
#[test]
fn two_concurrent_learners_catch_up_and_promote_one_at_a_time() {
    C::run(async {
        let temp = TempDir::new("concurrent-learners");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("concurrent-learners")
            .with_snapshot_interval(Duration::ZERO);
        // 21794/21795 belong to E19's storage-only block (no network bind).
        let addrs = [
            TestAddr::new(21789),
            TestAddr::new(21790),
            TestAddr::new(21791),
            TestAddr::new(21792),
            TestAddr::new(21793),
        ];

        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.election_timeout_max = 1000;
        config.heartbeat_min_interval = None;
        // Ten orders trigger a snapshot and purge of everything it covers, so
        // the late learners can only catch up through gRPC snapshot install.
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
        config.max_in_snapshot_log_to_keep = 0;

        let mut nodes = Vec::new();
        for (index, addr) in addrs.iter().enumerate() {
            nodes.push(Some(
                build_match_engine_node(
                    index as u64 + 1,
                    addr.to_string(),
                    config.clone(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            ));
        }
        let clients: Vec<MatchEngineClient<C>> = addrs
            .iter()
            .enumerate()
            .map(|(index, addr)| client_for(index as u64 + 1, *addr))
            .collect();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            assert_eq!(1, wait_for_leader(&clients[..3], None, deadline).await?);
            for id in 2..=3u64 {
                clients[0]
                    .add_learner((id, addrs[(id - 1) as usize].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            }
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3]))
                .await
                .unwrap()
                .unwrap();
            for index in 0..10u64 {
                clients[0]
                    .write(&MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        100.0 - index as f64,
                        1.0,
                        0,
                    )))
                    .await
                    .unwrap()
                    .unwrap();
            }
            let deadline = Instant::now() + crate::common::wait_secs(10);
            for client in &clients[..3] {
                wait_for_sequence(client, "10", deadline).await?;
            }

            // Wait for a snapshot on the leader that covers the purged log.
            let deadline = Instant::now() + crate::common::wait_secs(15);
            let snapshot_index = loop {
                let metrics = clients[0].metrics().await?;
                let snapshot = metrics.snapshot.map(|id| id.index);
                let purged = metrics.purged.map(|id| id.index);
                if let (Some(snapshot), Some(purged)) = (snapshot, purged)
                    && purged >= snapshot
                    && snapshot > 0
                {
                    println!("E18: leader snapshot={snapshot} purged={purged}");
                    break snapshot;
                }
                assert!(
                    Instant::now() <= deadline,
                    "leader never snapshotted: {metrics:?}"
                );
                C::sleep(Duration::from_millis(50)).await;
            };

            // Both learners are added before either has caught up. OpenRaft
            // serializes membership changes, so the second call can see a
            // transient `InProgress` while the first config commits; retry that
            // without waiting for snapshot catch-up (the race under test).
            clients[0]
                .add_learner((4, addrs[3].to_string()))
                .await
                .unwrap()
                .unwrap();
            let join_deadline = Instant::now() + crate::common::wait_secs(8);
            loop {
                match clients[0].add_learner((5, addrs[4].to_string())).await {
                    Ok(Ok(_)) => break,
                    Ok(Err(error)) if format!("{error:?}").contains("InProgress") => {
                        assert!(
                            Instant::now() <= join_deadline,
                            "second learner never joined: {error:?}"
                        );
                        C::sleep(Duration::from_millis(50)).await;
                    }
                    other => panic!("second add-learner failed: {other:?}"),
                }
            }

            let deadline = Instant::now() + crate::common::wait_secs(20);
            for id in 4..=5u64 {
                let client = &clients[(id - 1) as usize];
                wait_for_sequence(client, "10", deadline).await?;
                // A learner that installed a snapshot reports it and counts the
                // gRPC receive in its transport metrics; log replay alone would
                // be impossible after the leader's purge.
                let metrics = client.metrics().await?;
                assert!(
                    metrics.snapshot.map(|id| id.index).unwrap_or(0) >= snapshot_index,
                    "learner {id} did not install the snapshot: {metrics:?}"
                );
                let body: Value = http
                    .get(format!("http://{}/metrics", addrs[(id - 1) as usize]))
                    .send()
                    .await?
                    .json()
                    .await?;
                let received = body["Ok"]["transport_metrics"]["snapshot_received"]
                    .as_u64()
                    .unwrap_or(0);
                assert!(
                    received >= 1,
                    "learner {id} never received a snapshot over gRPC: {body}"
                );
                let health: Value = http
                    .get(format!("http://{}/health", addrs[(id - 1) as usize]))
                    .send()
                    .await?
                    .json()
                    .await?;
                assert_eq!(
                    Some(true),
                    health["ready"].as_bool(),
                    "learner {id} did not report ready: {health}"
                );
            }

            // Promote the first learner; a fresh write must replicate through it.
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3, 4]))
                .await
                .unwrap()
                .unwrap();
            for index in 0..5u64 {
                clients[0]
                    .write(&MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        50.0 - index as f64,
                        1.0,
                        0,
                    )))
                    .await
                    .unwrap()
                    .unwrap();
            }
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients[..4] {
                wait_for_sequence(client, "15", deadline).await?;
            }

            // Promote the second learner once the first is a healthy voter.
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3, 4, 5]))
                .await
                .unwrap()
                .unwrap();
            let config_deadline = Instant::now() + crate::common::wait_secs(20);
            loop {
                let mut converged = true;
                for client in &clients {
                    let metrics = client.metrics().await?;
                    let membership = metrics.membership_config.membership();
                    converged &= membership.voter_ids().count() == 5
                        && membership.get_joint_config().len() == 1
                        && membership.get_joint_config()[0] == BTreeSet::from([1, 2, 3, 4, 5]);
                }
                if converged {
                    break;
                }
                assert!(
                    Instant::now() <= config_deadline,
                    "five-voter config never reached every node"
                );
                C::sleep(Duration::from_millis(50)).await;
            }
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                let metrics = client.metrics().await?;
                assert_eq!(
                    5,
                    metrics.membership_config.membership().voter_ids().count(),
                    "node does not report exactly five voters"
                );
                assert_eq!(
                    1,
                    metrics
                        .membership_config
                        .membership()
                        .get_joint_config()
                        .len(),
                    "a joint config remained after both promotions"
                );
                wait_for_sequence(client, "15", deadline).await?;
            }

            // One more write reaches all five voters with one identical tape.
            let response = clients[0]
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    40.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(15), response.data.sequence);
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                wait_for_sequence(client, "16", deadline).await?;
            }
            let tape = event_tape(&clients[0]).await;
            for client in &clients {
                assert_eq!(
                    tape,
                    event_tape(client).await,
                    "tape diverged across five voters"
                );
                assert_eq!(
                    read_orders(&clients[0]).await?,
                    read_orders(client).await?,
                    "book diverged across five voters"
                );
            }
            println!(
                "E18: five voters converged at sequence 16 with {} events",
                16
            );

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

async fn event_tape(client: &MatchEngineClient<C>) -> String {
    serde_json::to_string(
        &client
            .events(&EventsRequest {
                limit: Some(1000),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap()
            .events,
    )
    .unwrap()
}
