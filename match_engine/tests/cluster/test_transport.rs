//! RAFT-E16: gRPC status mapping and bounded backoff.
//!
//! A debug-only server hook returns a chosen tonic status from the Raft append
//! handlers on one peer. The leader must map it to the matching `RPCError`,
//! back off without panicking, keep committing through the other follower and
//! converge once the hook is cleared.

use std::collections::BTreeSet;
use std::ffi::OsString;
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
use matchengine_raft::network::grpc::server::injected_status_count;
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

const LEADER_ADDR: TestAddr = TestAddr::new(21781);
const TARGET_ADDR: TestAddr = TestAddr::new(21782);
const HEALTHY_ADDR: TestAddr = TestAddr::new(21783);

const CODE_ENV: &str = "MATCHENGINE_TEST_GRPC_STATUS_CODE";
const TARGET_ENV: &str = "MATCHENGINE_TEST_GRPC_STATUS_TARGET";

/// Restores the process-wide injection env vars on drop so concurrent tests
/// keep their default (inert) behavior.
struct StatusEnv {
    code: Option<OsString>,
    target: Option<OsString>,
}

impl StatusEnv {
    fn new() -> Self {
        Self {
            code: std::env::var_os(CODE_ENV),
            target: std::env::var_os(TARGET_ENV),
        }
    }

    fn arm(&self, code: u8) {
        // SAFETY: edition 2024 marks process-wide env mutation unsafe. The vars
        // are scoped to this test and restored on drop; the target substring is
        // this test's unique peer port, so no other in-process node injects.
        unsafe {
            std::env::set_var(CODE_ENV, code.to_string());
            std::env::set_var(TARGET_ENV, TARGET_ADDR.http());
        }
    }

    fn clear(&self) {
        // SAFETY: see `arm`.
        unsafe {
            std::env::remove_var(CODE_ENV);
            std::env::remove_var(TARGET_ENV);
        }
    }

    fn restore(&self) {
        // SAFETY: see `arm`.
        unsafe {
            match self.code.clone() {
                Some(value) => std::env::set_var(CODE_ENV, value),
                None => std::env::remove_var(CODE_ENV),
            }
            match self.target.clone() {
                Some(value) => std::env::set_var(TARGET_ENV, value),
                None => std::env::remove_var(TARGET_ENV),
            }
        }
    }
}

impl Drop for StatusEnv {
    fn drop(&mut self) {
        self.restore();
    }
}

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
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_sequence(
    client: &MatchEngineClient<C>,
    expected: u64,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
            && value.parse::<u64>().ok() == Some(expected)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "node did not reach sequence {expected} (saw {:?})",
                client.read(&"orderbook_sequence".to_string()).await
            );
        }
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn sequence_number(client: &MatchEngineClient<C>) -> anyhow::Result<u64> {
    Ok(client
        .read(&"orderbook_sequence".to_string())
        .await?
        .parse()?)
}

async fn orders_json(client: &MatchEngineClient<C>) -> String {
    client.read(&"orderbook_orders".to_string()).await.unwrap()
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

async fn raw_metrics(addr: TestAddr) -> anyhow::Result<Value> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    Ok(http
        .get(format!("http://{addr}/metrics"))
        .send()
        .await?
        .json::<Value>()
        .await?)
}

/// Append-stream sessions opened by the leader. Failed RPCs are retried with
/// backoff, so an increase while a peer is unreachable is the observable signal
/// that the status was mapped to a retryable `RPCError`.
fn append_streams(metrics: &Value) -> u64 {
    let transport = &metrics["Ok"]["transport_metrics"];
    transport["heartbeat_streams"].as_u64().unwrap_or(0)
        + transport["replication_streams"].as_u64().unwrap_or(0)
}

#[test]
fn grpc_statuses_map_to_raft_errors_and_recover() {
    C::run(async {
        let temp = TempDir::new("transport-status");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("transport-status")
            .with_snapshot_interval(Duration::ZERO);
        let addresses = [LEADER_ADDR, TARGET_ADDR, HEALTHY_ADDR];

        let mut nodes = Vec::new();
        for (index, addr) in addresses.iter().enumerate() {
            nodes.push(Some(
                start_node((index + 1) as u64, *addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients: Vec<MatchEngineClient<C>> = addresses
            .iter()
            .enumerate()
            .map(|(index, addr)| client((index + 1) as u64, *addr))
            .collect();
        let env = StatusEnv::new();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            let leader = wait_for_leader(&clients, deadline).await?;
            assert_eq!(1, leader, "the initialized node should lead");
            for id in 2..=3u64 {
                clients[0]
                    .add_learner((id, addresses[(id - 1) as usize].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            }
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3]))
                .await
                .unwrap()
                .unwrap();

            // A resting order per price keeps every committed sequence distinct.
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
                wait_for_sequence(client, 10, deadline).await?;
            }
            let leader_client = &clients[(leader - 1) as usize];
            let target_client = &clients[1];
            let healthy_client = &clients[2];

            for (code, label, stall_ms) in [
                (14u8, "Unavailable", 350u64),
                (4, "DeadlineExceeded", 350),
                (2, "Unknown", 350),
            ] {
                let before = sequence_number(target_client).await?;
                let streams_before = append_streams(&raw_metrics(LEADER_ADDR).await?);
                let injected_before = injected_status_count();

                env.arm(code);
                // Give the leader time to open a stream and have it rejected.
                C::sleep(Duration::from_millis(stall_ms)).await;
                for index in 0..5u64 {
                    leader_client
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
                let expected = before + 5;
                wait_for_sequence(
                    healthy_client,
                    expected,
                    Instant::now() + crate::common::wait_secs(6),
                )
                .await?;
                // Hold the hook while the healthy follower advances so a
                // briefly stalled target cannot catch up before we sample it.
                C::sleep(Duration::from_millis(stall_ms)).await;

                // The leader's match index for the injected peer must trail the
                // committed frontier: every append stream to it is rejected, so
                // it cannot acknowledge the new entries. The healthy follower
                // still supplies the 2/3 quorum and reaches the frontier.
                let metrics = leader_client.metrics().await?;
                let match_index = |id: u64| {
                    metrics
                        .replication
                        .as_ref()
                        .and_then(|replication| replication.get(&id))
                        .and_then(|entry| entry.as_ref())
                        .map(|log_id| log_id.index)
                };
                let target_match = match_index(2);
                let leader_match = match_index(leader);
                let healthy_match = match_index(3);
                let streams_after = append_streams(&raw_metrics(LEADER_ADDR).await?);
                let injected_after = injected_status_count();
                let target_local = sequence_number(target_client).await?;
                println!(
                    "status {label}: target sequence {target_local} (expected {expected}), \
                     match target={target_match:?} healthy={healthy_match:?} leader={leader_match:?}, \
                     streams {streams_before}->{streams_after}, injected {}",
                    injected_after - injected_before
                );
                assert!(
                    target_match < leader_match,
                    "injected follower acknowledged entries despite {label} rejections"
                );
                assert_eq!(
                    healthy_match, leader_match,
                    "healthy follower did not supply the write quorum"
                );
                assert_eq!(
                    Some(leader),
                    metrics.current_leader,
                    "status injection disrupted leadership"
                );
                assert!(
                    streams_after > streams_before,
                    "no retried append streams after {label}: {streams_before} -> {streams_after}"
                );
                assert!(
                    injected_after > injected_before,
                    "the {label} hook never reached an append handler"
                );

                env.clear();
                let deadline = Instant::now() + crate::common::wait_secs(12);
                for client in &clients {
                    wait_for_sequence(client, expected, deadline).await?;
                }
                let tape = event_tape(leader_client).await;
                for client in &clients {
                    assert_eq!(tape, event_tape(client).await, "event tape diverged");
                    assert_eq!(orders_json(leader_client).await, orders_json(client).await);
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        // Clear before shutdown so a panicking test cannot leave the hook armed.
        env.clear();
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
