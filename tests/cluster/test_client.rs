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
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::type_config::TypeConfigExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.heartbeat_min_interval = None;
    config.election_timeout_max = 1_000;
    config
}

async fn start_node(id: u64, addr: &str, storage: &StorageConfig) -> MatchEngineNode {
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
    format!("127.0.0.1:{}", 21914 + id)
}

/// A tiny TCP proxy that drops the first `drops` connections, then forwards to
/// the real HTTP node. Used to prove retry/backoff without a real network fault.
async fn flaky_proxy(
    listen: &str,
    target: &str,
    drops: usize,
    connections: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    let listener = TcpListener::bind(listen).await.unwrap();
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
        let node = start_node(1, "127.0.0.1:21912", &storage).await;
        let client = MatchEngineClient::<C>::new(1, "127.0.0.1:21912".to_string());

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
            Instant::now() + Duration::from_secs(15),
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
        let node = start_node(1, "127.0.0.1:21913", &storage).await;
        let direct = MatchEngineClient::<C>::new(1, "127.0.0.1:21913".to_string());
        direct.init().await.unwrap().unwrap();
        wait_for_leader(
            std::slice::from_ref(&direct),
            None,
            Instant::now() + Duration::from_secs(15),
        )
        .await;

        let connections = Arc::new(AtomicUsize::new(0));
        let proxy = flaky_proxy("127.0.0.1:21914", "127.0.0.1:21913", 1, connections.clone()).await;

        let client = MatchEngineClient::<C>::with_config(
            1,
            "127.0.0.1:21914".to_string(),
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
        let deadline = Instant::now() + Duration::from_secs(15);
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
            Instant::now() + Duration::from_secs(20),
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
