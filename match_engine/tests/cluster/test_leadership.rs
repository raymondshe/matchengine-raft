//! RAFT-E07: leader transfer under sustained write load.
//!
//! Four voters take a continuous stream of keyed `/write_batch` requests while
//! leadership moves twice: once through the guarded console action and once
//! through OpenRaft's direct `Raft::trigger().transfer_leader` API (the same
//! call the console action makes internally). Every acknowledged sequence must
//! be unique, appear exactly once in every node's event tape, and the final
//! book/sequence must be identical on all replicas.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
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
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::ServerState;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;
use serde_json::Value;
use serde_json::json;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDRS: [TestAddr; 4] = [
    TestAddr::new(21741),
    TestAddr::new(21742),
    TestAddr::new(21743),
    TestAddr::new(21744),
];

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 150;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1000;
    config.heartbeat_min_interval = None;
    // Keep every entry in the WAL so the whole event tape is comparable.
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

fn client(id: u64, addr: TestAddr, config: ClientConfig) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::with_config(id, addr.to_string(), config)
}

async fn init_four_voters(http: &reqwest::Client) -> anyhow::Result<()> {
    let membership: Vec<(u64, String)> = ADDRS
        .iter()
        .enumerate()
        .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
        .collect();
    let response = http
        .post(format!("{}/init", ADDRS[0].http_url()))
        .json(&membership)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "four-voter /init failed: {status} {body}"
    );
    Ok(())
}

/// Waits until *every* node reports `expected` as the current leader.
async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    expected: u64,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        let mut agreeing = 0;
        for client in clients {
            if let Ok(metrics) = client.metrics().await
                && metrics.current_leader == Some(expected)
            {
                agreeing += 1;
            }
        }
        if agreeing == clients.len() {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() <= deadline,
            "only {agreeing}/{} nodes report leader {expected}",
            clients.len()
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads the whole replicated event window through one node's `/events`.
async fn event_tape(client: &MatchEngineClient<C>) -> anyhow::Result<Vec<MatchEvent>> {
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .events(&EventsRequest {
                after,
                limit: Some(1000),
                wait_ms: None,
            })
            .await??;
        events.extend(page.events);
        if !page.has_more {
            return Ok(events);
        }
        after = page.next_after;
        anyhow::ensure!(after.is_some(), "paged /events without a cursor");
    }
}

async fn post_action(
    http: &reqwest::Client,
    base: &str,
    action: &str,
    body: Value,
) -> reqwest::Response {
    http.post(format!("{base}/console/api/actions/{action}"))
        .header("X-Console-Action", "1")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The console action refuses a target that is lagging; under load that can
/// momentarily be true, so retry within a small bound before failing.
async fn console_transfer(http: &reqwest::Client, base: &str, target: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + crate::common::wait_secs(5);
    loop {
        let response =
            post_action(http, base, "transfer-leader", json!({ "target": target })).await;
        let status = response.status();
        if status == StatusCode::ACCEPTED {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        anyhow::ensure!(
            Instant::now() <= deadline,
            "console transfer to {target} failed: {status} {body}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn leader_transfer_under_sustained_write_load() {
    C::run(async {
        let temp = TempDir::new("leadership-load");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("leadership-load");
        let mut nodes = Vec::new();
        for (index, addr) in ADDRS.iter().enumerate() {
            nodes.push(Some(
                start_node(index as u64 + 1, *addr, &storage).await.unwrap(),
            ));
        }
        let observers: Vec<_> = ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| client(index as u64 + 1, *addr, ClientConfig::default()))
            .collect();
        let members: Vec<(u64, String)> = ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
            .collect();

        // Shared with the shutdown epilogue so the last leader stops first.
        let current_leader = Arc::new(AtomicU64::new(1));
        let shutdown_leader = current_leader.clone();

        let outcome = AssertUnwindSafe(async {
            let http = reqwest::Client::builder().no_proxy().build()?;
            init_four_voters(&http).await?;
            let deadline = Instant::now() + crate::common::wait_secs(15);
            let mut leader = 0u64;
            loop {
                for (index, observer) in observers.iter().enumerate() {
                    if let Ok(metrics) = observer.metrics().await
                        && metrics.current_leader == Some(index as u64 + 1)
                        && metrics.state == ServerState::Leader
                        && metrics.membership_config.voter_ids().count() == 4
                    {
                        leader = index as u64 + 1;
                        break;
                    }
                }
                if leader != 0 {
                    current_leader.store(leader, Ordering::Relaxed);
                    break;
                }
                anyhow::ensure!(Instant::now() <= deadline, "no initial leader");
                C::sleep(Duration::from_millis(50)).await;
            }
            println!("leadership-load: initial leader {leader}");

            // One leader-aware client shared by the load task: it re-discovers
            // the leader from the member list after each transport failure.
            let load_client = Arc::new(MatchEngineClient::<C>::with_members(
                members,
                ClientConfig {
                    timeout: Duration::from_millis(1_500),
                    max_retries: 2,
                    max_forward_retries: 6,
                    initial_backoff: Duration::from_millis(20),
                    max_backoff: Duration::from_millis(200),
                },
            ));
            let results: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
            let progress = Arc::new(AtomicUsize::new(0));
            let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));

            let task_client = load_client.clone();
            let task_results = results.clone();
            let task_progress = progress.clone();
            let task_errors = errors.clone();
            let task_stop = stop.clone();
            let load = tokio::spawn(async move {
                let mut batch_no = 0u64;
                while !task_stop.load(Ordering::Relaxed) {
                    let mut requests = Vec::with_capacity(8);
                    let mut keys = Vec::with_capacity(8);
                    for offset in 0..8u64 {
                        let key = format!("lead-{batch_no}-{offset}");
                        requests.push(MatchEngineRequest::place_with_client_id(
                            Order::new(
                                OrderSide::Buy,
                                100.0 - (batch_no * 8 + offset) as f64 * 0.001,
                                1.0,
                                0,
                            ),
                            key.clone(),
                        ));
                        keys.push(key);
                    }
                    match task_client.write_batch(&requests).await {
                        Ok(Ok(response)) => {
                            let mut accepted = 0;
                            for (item, key) in response.data.batch.iter().zip(keys.iter()) {
                                if let Some(sequence) = item.sequence {
                                    task_results.lock().unwrap().push((key.clone(), sequence));
                                    accepted += 1;
                                }
                            }
                            task_progress.fetch_add(accepted, Ordering::Relaxed);
                        }
                        Ok(Err(error)) => task_errors
                            .lock()
                            .unwrap()
                            .push(format!("batch {batch_no} rejected: {error:?}")),
                        Err(error) => task_errors
                            .lock()
                            .unwrap()
                            .push(format!("batch {batch_no} transport error: {error:?}")),
                    }
                    batch_no += 1;
                    C::sleep(Duration::from_millis(50)).await;
                }
            });

            // Let a few batches commit, then transfer through the console.
            let load_start = Instant::now();
            while progress.load(Ordering::Relaxed) < 32
                && load_start.elapsed() < Duration::from_secs(5)
            {
                C::sleep(Duration::from_millis(10)).await;
            }
            anyhow::ensure!(
                progress.load(Ordering::Relaxed) >= 32,
                "load task did not commit any batches: {:?}",
                errors.lock().unwrap()
            );
            let target = (1..=4u64).find(|id| *id != leader).unwrap();
            let base = ADDRS[(leader - 1) as usize].http_url();
            console_transfer(&http, &base, target).await?;
            wait_for_leader(
                &observers,
                target,
                Instant::now() + crate::common::wait_secs(10),
            )
            .await?;
            println!("leadership-load: console transfer {leader} -> {target}");
            leader = target;
            current_leader.store(leader, Ordering::Relaxed);

            // Same mechanism the console uses, invoked directly: OpenRaft's
            // `Raft::trigger().transfer_leader` is reachable from a test
            // because `MatchEngineNode::raft` is public.
            let direct_target = (1..=4u64).find(|id| *id != leader).unwrap();
            let outcome = nodes[(leader - 1) as usize]
                .as_ref()
                .unwrap()
                .raft
                .trigger()
                .transfer_leader(direct_target)
                .await;
            anyhow::ensure!(
                outcome.is_ok(),
                "direct transfer to {direct_target} failed: {:?}",
                outcome.err()
            );
            wait_for_leader(
                &observers,
                direct_target,
                Instant::now() + crate::common::wait_secs(10),
            )
            .await?;
            println!("leadership-load: direct transfer {leader} -> {direct_target}");
            leader = direct_target;
            current_leader.store(leader, Ordering::Relaxed);

            // Keep writing after both transfers so the load spans the whole
            // transition, then stop before taking any durable-state snapshot.
            let settled = Instant::now();
            while progress.load(Ordering::Relaxed) < 64
                && settled.elapsed() < Duration::from_secs(3)
            {
                C::sleep(Duration::from_millis(10)).await;
            }
            stop.store(true, Ordering::Relaxed);
            stop.store(true, Ordering::Relaxed);
            load.await.unwrap();
            C::sleep(Duration::from_millis(200)).await;

            let recorded = results.lock().unwrap().clone();
            println!(
                "leadership-load: {} acknowledged orders, {} transport errors, leader {leader}",
                recorded.len(),
                errors.lock().unwrap().len()
            );
            anyhow::ensure!(!recorded.is_empty(), "no orders were acknowledged");
            let keys: HashSet<_> = recorded.iter().map(|(key, _)| key).collect();
            let sequences: HashSet<u64> = recorded.iter().map(|(_, sequence)| *sequence).collect();
            anyhow::ensure!(
                keys.len() == recorded.len(),
                "duplicate client_order_id in acknowledged responses"
            );
            anyhow::ensure!(
                sequences.len() == recorded.len(),
                "duplicate sequence in acknowledged responses: {sequences:?}"
            );

            // A retry with the same key returns the original sequence and
            // places no second order.
            let (retry_key, retry_sequence) = recorded[0].clone();
            let duplicate = load_client
                .write(&MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Sell, 1.0, 1.0, 0),
                    retry_key.clone(),
                ))
                .await??;
            anyhow::ensure!(
                duplicate.data.sequence == Some(retry_sequence),
                "retry of {retry_key} returned {:?}, expected {retry_sequence}",
                duplicate.data.sequence
            );

            // Wait for every replica to apply the last acknowledged sequence.
            let last = *sequences.iter().max().unwrap();
            let expected = (last + 1).to_string();
            let deadline = Instant::now() + crate::common::wait_secs(10);
            for observer in &observers {
                loop {
                    if let Ok(value) = observer.read(&"orderbook_sequence".to_string()).await
                        && value == expected
                    {
                        break;
                    }
                    anyhow::ensure!(
                        Instant::now() <= deadline,
                        "node did not reach sequence {expected}"
                    );
                    C::sleep(Duration::from_millis(50)).await;
                }
            }

            let orders = observers[0].read(&"orderbook_orders".to_string()).await?;
            let tape = event_tape(&observers[0]).await?;
            let tape_sequences: Vec<u64> = tape.iter().map(|event| event.order_sequence).collect();
            anyhow::ensure!(
                tape_sequences.len() == tape_sequences.iter().collect::<HashSet<_>>().len(),
                "event tape contains a duplicate sequence"
            );
            for sequence in &sequences {
                anyhow::ensure!(
                    tape_sequences
                        .iter()
                        .filter(|value| *value == sequence)
                        .count()
                        == 1,
                    "acknowledged sequence {sequence} is not in /events exactly once"
                );
            }
            for observer in observers.iter().skip(1) {
                anyhow::ensure!(
                    observer.read(&"orderbook_orders".to_string()).await? == orders,
                    "final order books diverged"
                );
                anyhow::ensure!(
                    observer.read(&"orderbook_sequence".to_string()).await? == expected,
                    "final sequences diverged"
                );
                anyhow::ensure!(
                    event_tape(observer).await? == tape,
                    "final event tapes diverged"
                );
            }
            println!(
                "leadership-load: {} events agree on all four nodes, final sequence {expected}",
                tape.len()
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        // Stop the current leader first so followers have no inbound
        // replication stream while their gRPC servers shut down.
        let leader_index = (shutdown_leader.load(Ordering::Relaxed) - 1) as usize;
        let mut order: Vec<usize> = vec![leader_index];
        order.extend((0..nodes.len()).filter(|index| *index != leader_index));
        for index in order {
            if let Some(node) = nodes[index].take()
                && let Err(error) = node.shutdown().await
            {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}
