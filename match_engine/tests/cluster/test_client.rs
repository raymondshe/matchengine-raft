//! Client transport and failover coverage.
//!
//! These tests exercise the public Rust client rather than the server directly:
//! typed HTTP edge errors, retry/backoff after a dropped connection, and
//! forwarding to the new leader after a failover.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;

use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::ClientConfig;
use matchengine_raft::client::ClientError;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::LeaderRedirect;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::errors::RPCError;
use openraft::type_config::TypeConfigExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR: TestAddr = TestAddr::new(21912);
const FORWARD_ADDR: TestAddr = TestAddr::new(21913);
const PROXY_ADDR: TestAddr = TestAddr::new(21914);

// E17 addresses. The exhausted-discovery members are dead ports (no node ever
// binds them); the auto-forward cluster uses the remaining E17 ports. The dead
// ports 21794/21795 belong to E19's storage-only block and are never bound.
const DEAD_ADDRS: [TestAddr; 3] = [
    TestAddr::new(21785),
    TestAddr::new(21794),
    TestAddr::new(21795),
];
const AUTO_FORWARD_ADDRS: [TestAddr; 3] = [
    TestAddr::new(21786),
    TestAddr::new(21787),
    TestAddr::new(21788),
];

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
) -> MatchEngineNode {
    build_match_engine_node(id, addr.to_string(), test_config(), storage.clone())
        .await
        .unwrap()
}

async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    exclude: Option<u64>,
    deadline: Instant,
) -> u64 {
    loop {
        for client in clients {
            if let Ok(metrics) = client.metrics().await
                && let Some(leader) = metrics.current_leader
                && Some(leader) != exclude
            {
                return leader;
            }
        }
        assert!(Instant::now() <= deadline, "no leader was elected");
        C::sleep(Duration::from_millis(100)).await;
    }
}

fn place() -> MatchEngineRequest {
    MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0))
}

fn addr(id: u64) -> String {
    TestAddr::new(21914 + id as u16).to_string()
}

/// A tiny TCP proxy that drops the first `drops` connections, then forwards to
/// the real HTTP node. Used to prove retry/backoff without a real network fault.
async fn flaky_proxy(
    listen: impl std::fmt::Display,
    target: impl std::fmt::Display,
    drops: usize,
    connections: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    let listener = TcpListener::bind(listen.to_string()).await.unwrap();
    let target = target.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            let index = connections.fetch_add(1, Ordering::Relaxed);
            if index < drops {
                drop(inbound);
                continue;
            }
            let target = target.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = TcpStream::connect(&target).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    })
}

#[test]
fn client_reports_typed_no_leader_and_api_errors() {
    C::run(async {
        let temp = TempDir::new("client-typed-errors");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("client-typed-errors");
        let node = start_node(1, ADDR, &storage).await;
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());

        // Before init there is no leader. The client must expose a typed error,
        // not a bare message or a transport/decode failure.
        match client.write(&place()).await {
            Err(ClientError::NoLeader) => {}
            _ => panic!("expected ClientError::NoLeader before initialization"),
        }

        client.init().await.unwrap().unwrap();
        wait_for_leader(
            std::slice::from_ref(&client),
            None,
            Instant::now() + crate::common::wait_secs(15),
        )
        .await;

        // The gateway symbol guard returns a 400 with a plain-text body; the
        // client must expose the status and body as a typed API error.
        let mut wrong_symbol = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
        wrong_symbol.symbol_id = 1;
        match client.write(&MatchEngineRequest::place(wrong_symbol)).await {
            Err(ClientError::Api { status, body }) => {
                assert_eq!(400, status);
                assert!(body.contains("does not match"), "{body}");
            }
            _ => panic!("expected ClientError::Api for a wrong-symbol write"),
        }

        assert_eq!(
            "",
            client.read(&"does_not_exist".to_string()).await.unwrap()
        );
        let response = client.write(&place()).await.unwrap().unwrap();
        assert_eq!(Some(0), response.data.sequence);

        node.shutdown().await.unwrap();
    });
}

#[test]
fn client_retries_a_dropped_connection() {
    C::run(async {
        let temp = TempDir::new("client-retry");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("client-retry");
        let node = start_node(1, FORWARD_ADDR, &storage).await;
        let direct = MatchEngineClient::<C>::new(1, FORWARD_ADDR.to_string());
        direct.init().await.unwrap().unwrap();
        wait_for_leader(
            std::slice::from_ref(&direct),
            None,
            Instant::now() + crate::common::wait_secs(15),
        )
        .await;

        let connections = Arc::new(AtomicUsize::new(0));
        let proxy = flaky_proxy(PROXY_ADDR, FORWARD_ADDR, 1, connections.clone()).await;

        let client = MatchEngineClient::<C>::with_config(
            1,
            PROXY_ADDR.to_string(),
            ClientConfig {
                max_retries: 3,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(100),
                ..ClientConfig::default()
            },
        );
        let response = client.write(&place()).await.unwrap().unwrap();
        assert_eq!(Some(0), response.data.sequence);
        assert!(
            connections.load(Ordering::Relaxed) >= 2,
            "the client must reconnect after the dropped connection"
        );

        proxy.abort();
        node.shutdown().await.unwrap();
    });
}

#[test]
fn client_discovers_new_leader_after_target_dies() {
    C::run(async {
        let temp = TempDir::new("client-discovery-failover");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("client-discovery");
        let bases = [21950u16, 21951, 21952];
        let addr = |id: u64| TestAddr::new(bases[(id - 1) as usize]).to_string();

        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (i, id) in (1..=3u64).enumerate() {
            nodes.push(Some(
                start_node(id, TestAddr::new(bases[i]), &storage).await,
            ));
        }
        let observers: Vec<MatchEngineClient<C>> = (1..=3u64)
            .map(|id| MatchEngineClient::<C>::new(id, addr(id)))
            .collect();

        observers[0].init().await.unwrap().unwrap();
        let leader = wait_for_leader(
            &observers,
            None,
            Instant::now() + crate::common::wait_secs(15),
        )
        .await;
        assert_eq!(1, leader);
        observers[0]
            .add_learner((2, addr(2)))
            .await
            .unwrap()
            .unwrap();
        observers[0]
            .add_learner((3, addr(3)))
            .await
            .unwrap()
            .unwrap();
        observers[0]
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();

        // Kill the leader; a survivor takes over.
        nodes[(leader - 1) as usize]
            .take()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        let new_leader = wait_for_leader(
            &observers,
            Some(leader),
            Instant::now() + crate::common::wait_secs(20),
        )
        .await;
        assert_ne!(leader, new_leader);

        // Seed the client with the DEAD leader as its first member/hint. The
        // first write must fail over through Raft-native discovery on /metrics.
        let members: Vec<(u64, String)> = (1..=3u64).map(|id| (id, addr(id))).collect();
        let client = MatchEngineClient::<C>::with_members(
            members,
            ClientConfig {
                max_retries: 4,
                initial_backoff: Duration::from_millis(50),
                max_backoff: Duration::from_millis(200),
                ..ClientConfig::default()
            },
        );
        let response = client.write(&place()).await.unwrap().unwrap();
        assert_eq!(Some(0), response.data.sequence);
        assert_eq!(
            (new_leader, addr(new_leader)),
            *client.leader.lock().unwrap(),
            "the client must cache the newly discovered leader"
        );

        for node in nodes.into_iter().flatten() {
            node.shutdown().await.unwrap();
        }
    });
}

#[test]
fn client_forwards_after_leader_failover() {
    C::run(async {
        let temp = TempDir::new("client-failover");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("client-failover");
        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for id in 1..=3u64 {
            nodes.push(Some(start_node(id, &addr(id), &storage).await));
        }
        let clients: Vec<MatchEngineClient<C>> = (1..=3u64)
            .map(|id| MatchEngineClient::<C>::new(id, addr(id)))
            .collect();

        clients[0].init().await.unwrap().unwrap();
        let deadline = Instant::now() + crate::common::wait_secs(15);
        let leader = wait_for_leader(&clients, None, deadline).await;
        assert_eq!(1, leader);

        clients[0].add_learner((2, addr(2))).await.unwrap().unwrap();
        clients[0].add_learner((3, addr(3))).await.unwrap().unwrap();
        clients[0]
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();

        // A follower forwards the first write to the leader.
        let follower = if leader == 1 { 2 } else { 1 };
        let follower_client = MatchEngineClient::<C>::new(follower, addr(follower));
        let idempotent = || {
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                "failover-idempotent",
            )
        };
        let first = follower_client.write(&idempotent()).await.unwrap().unwrap();
        assert_eq!(Some(0), first.data.sequence);

        // Kill the leader and wait for a survivor to take over.
        nodes[(leader - 1) as usize]
            .take()
            .expect("leader node")
            .shutdown()
            .await
            .unwrap();
        let new_leader = wait_for_leader(
            &clients,
            Some(leader),
            Instant::now() + crate::common::wait_secs(20),
        )
        .await;
        assert_ne!(leader, new_leader);

        // A client bound to another survivor must follow the new leader. The
        // retry budget covers the election/lease settling window.
        let survivor = (1..=3)
            .find(|id| *id != leader && *id != new_leader)
            .expect("a surviving follower");
        let client = MatchEngineClient::<C>::with_config(
            survivor,
            addr(survivor),
            ClientConfig {
                max_forward_retries: 50,
                initial_backoff: Duration::from_millis(50),
                max_backoff: Duration::from_millis(500),
                ..ClientConfig::default()
            },
        );

        // The idempotency map is part of replicated state, so retrying the
        // same submission through the new leader must not place a second order.
        let duplicate = client.write(&idempotent()).await.unwrap().unwrap();
        assert_eq!(Some(0), duplicate.data.sequence);
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.len(), "the retry duplicated the order");

        let response = client.write(&place()).await.unwrap().unwrap();
        assert_eq!(Some(1), response.data.sequence);
        assert_eq!(
            "2",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        for node in nodes.into_iter().flatten() {
            node.shutdown().await.unwrap();
        }
    });
}

#[test]
fn follower_write_returns_a_typed_leader_redirect() {
    C::run(async {
        let first_dir = TempDir::new("redirect-1");
        let second_dir = TempDir::new("redirect-2");
        let first_storage = StorageConfig::new(first_dir.path()).with_instance_prefix("redirect-1");
        let second_storage =
            StorageConfig::new(second_dir.path()).with_instance_prefix("redirect-2");

        let leader_addr = TestAddr::new(21970);
        let peer_addr = TestAddr::new(21971);
        let node1 = start_node(1, leader_addr, &first_storage).await;
        let node2 = start_node(2, peer_addr, &second_storage).await;
        let client = MatchEngineClient::<C>::new(1, leader_addr.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(
            std::slice::from_ref(&client),
            None,
            Instant::now() + crate::common::wait_secs(15),
        )
        .await;
        client
            .add_learner((2, peer_addr.to_string()))
            .await
            .unwrap()
            .unwrap();
        client
            .change_membership(&BTreeSet::from([1, 2]))
            .await
            .unwrap()
            .unwrap();
        wait_for_leader(
            std::slice::from_ref(&client),
            None,
            Instant::now() + crate::common::wait_secs(15),
        )
        .await;

        let metrics: serde_json::Value = reqwest::get(format!("http://{leader_addr}/metrics"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let leader_id = metrics["Ok"]["current_leader"].as_u64().unwrap();
        let own_id: serde_json::Value = reqwest::get(format!("http://{leader_addr}/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let follower_addr = if own_id["node_id"].as_u64() == Some(leader_id) {
            peer_addr
        } else {
            leader_addr
        };

        let response = reqwest::Client::new()
            .post(format!("http://{follower_addr}/write"))
            .json(&place())
            .send()
            .await
            .unwrap();
        assert_eq!(503, response.status());
        let redirect: LeaderRedirect = response.json().await.unwrap();
        assert_eq!("not_leader", redirect.error);
        assert_eq!(Some(leader_id), redirect.leader_id);
        assert!(
            redirect
                .leader_addr
                .as_deref()
                .is_some_and(|addr| !addr.is_empty()),
            "the body must carry the leader address"
        );

        node1.shutdown().await.unwrap();
        node2.shutdown().await.unwrap();
    });
}

/// RAFT-E17 (exhausted discovery): with every bootstrap member dead, the
/// client must return a typed transport error quickly instead of hanging.
#[test]
fn exhausted_leader_discovery_returns_a_typed_error() {
    C::run(async {
        let members: Vec<(u64, String)> = DEAD_ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
            .collect();
        let client = MatchEngineClient::<C>::with_members(
            members,
            ClientConfig {
                timeout: Duration::from_millis(250),
                max_retries: 1,
                max_forward_retries: 1,
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(10),
            },
        );

        let started = Instant::now();
        let metrics = C::timeout(crate::common::wait_secs(5), client.metrics()).await;
        let read = C::timeout(
            crate::common::wait_secs(5),
            client.read(&"orderbook_sequence".to_string()),
        )
        .await;
        println!(
            "exhausted discovery returned in {:?}: metrics={metrics:?}, read={read:?}",
            started.elapsed()
        );
        assert!(
            matches!(
                metrics,
                Ok(Err(ClientError::Rpc(
                    RPCError::Unreachable(_) | RPCError::Network(_)
                ))) | Ok(Err(ClientError::Timeout))
            ),
            "expected a typed transport error, got {metrics:?}"
        );
        assert!(
            matches!(read, Ok(Err(_))),
            "read against dead members must return a typed error, got {read:?}"
        );
        assert!(
            started.elapsed() < crate::common::wait_secs(10),
            "low-retry discovery took too long"
        );
    });
}

/// RAFT-E17 (read auto-forward): a client pinned to a follower must transparently
/// forward a linearizable read to the leader, observe the latest committed
/// value, and leave the follower's local state untouched.
#[test]
fn follower_linearizable_read_auto_forwards_to_the_leader() {
    C::run(async {
        let temp = TempDir::new("client-auto-forward");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("client-auto-forward");
        let mut nodes = Vec::new();
        for (index, addr) in AUTO_FORWARD_ADDRS.iter().enumerate() {
            nodes.push(Some(start_node(index as u64 + 1, *addr, &storage).await));
        }
        let clients: Vec<MatchEngineClient<C>> = AUTO_FORWARD_ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| MatchEngineClient::<C>::new(index as u64 + 1, addr.to_string()))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let leader = wait_for_leader(
                &clients,
                None,
                Instant::now() + crate::common::wait_secs(15),
            )
            .await;
            assert_eq!(1, leader);
            for id in 2..=3u64 {
                clients[0]
                    .add_learner((id, AUTO_FORWARD_ADDRS[(id - 1) as usize].to_string()))
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
            for client in &clients {
                loop {
                    if client
                        .read(&"orderbook_sequence".to_string())
                        .await
                        .is_ok_and(|value| value == "10")
                    {
                        break;
                    }
                    assert!(
                        Instant::now() <= deadline,
                        "cluster did not reach sequence 10"
                    );
                    C::sleep(Duration::from_millis(50)).await;
                }
            }

            let follower = if leader == 1 { 2 } else { 1 };
            let follower_addr = AUTO_FORWARD_ADDRS[(follower - 1) as usize];
            // A pinned client observes the follower's *local* book before and
            // after the forwarded read; the forwarding client may rewire itself.
            let pinned = MatchEngineClient::<C>::new(follower, follower_addr.to_string());
            let before_sequence = pinned.read(&"orderbook_sequence".to_string()).await?;
            let before_orders = pinned.read(&"orderbook_orders".to_string()).await?;

            let forwarding = MatchEngineClient::<C>::with_config(
                follower,
                follower_addr.to_string(),
                ClientConfig {
                    max_forward_retries: 10,
                    initial_backoff: Duration::from_millis(50),
                    max_backoff: Duration::from_millis(200),
                    ..ClientConfig::default()
                },
            );
            let value = C::timeout(
                crate::common::wait_secs(8),
                forwarding.linearizable_read_auto_forward(&"orderbook_sequence".to_string()),
            )
            .await
            .expect("forwarded linearizable read hung")??;
            assert_eq!("10", value, "forwarded read missed the leader's commit");
            let orders = forwarding
                .linearizable_read_auto_forward(&"orderbook_orders".to_string())
                .await??;
            assert_eq!(
                before_orders, orders,
                "forwarded read returned a different book than the leader"
            );

            let metrics = pinned.metrics().await?;
            assert_ne!(
                Some(follower),
                metrics.current_leader,
                "the read target must remain a follower"
            );
            assert_eq!(
                before_sequence,
                pinned.read(&"orderbook_sequence".to_string()).await?,
                "a read forwarded to the leader changed the follower's state"
            );
            assert_eq!(
                before_orders,
                pinned.read(&"orderbook_orders".to_string()).await?
            );
            println!("auto-forward from follower {follower} to leader {leader} returned {value}");
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
