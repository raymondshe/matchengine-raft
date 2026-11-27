//! RAFT-E10: concurrent writes across a failover have no loss or duplicates.
//!
//! Four client tasks submit 200 keyed writes each (800 total). Once roughly
//! half have committed, the leader is stopped and every in-flight request is
//! retried with the same key through the new leader. The durable contract is
//! checked on the surviving replicas: exactly 800 contiguous sequences, one
//! event per sequence on every node, identical book and tape.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
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

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDRS: [TestAddr; 4] = [
    TestAddr::new(21745),
    TestAddr::new(21746),
    TestAddr::new(21747),
    TestAddr::new(21748),
];
const WRITES_PER_TASK: u64 = 200;
const TASKS: u64 = 4;
const TOTAL_WRITES: u64 = WRITES_PER_TASK * TASKS;

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 150;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
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

async fn wait_for_leader(clients: &[MatchEngineClient<C>]) -> anyhow::Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for (index, client) in clients.iter().enumerate() {
            if let Ok(metrics) = client.metrics().await
                && metrics.current_leader == Some(index as u64 + 1)
                && metrics.state == ServerState::Leader
                && metrics.membership_config.voter_ids().count() == 4
            {
                return Ok(index as u64 + 1);
            }
        }
        anyhow::ensure!(Instant::now() <= deadline, "no stable four-voter leader");
        C::sleep(Duration::from_millis(50)).await;
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
        anyhow::ensure!(
            Instant::now() <= deadline,
            "node did not reach sequence {expected}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

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

#[test]
fn concurrent_writes_across_failover_have_no_loss_or_duplicates() {
    C::run(async {
        let temp = TempDir::new("concurrent-failover");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("concurrent-failover");
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
        let killed_leader = Arc::new(AtomicU64::new(1));
        let shutdown_target = killed_leader.clone();

        let outcome = AssertUnwindSafe(async {
            let http = reqwest::Client::builder().no_proxy().build()?;
            init_four_voters(&http).await?;
            let leader = wait_for_leader(&observers).await?;
            killed_leader.store(leader, Ordering::Relaxed);
            println!("concurrent-failover: initial leader {leader}");

            // A single leader-aware client discovers the new leader from the
            // member list after the kill. Per-key retries make the at-least-
            // once transport safe: a lost response replays the original key.
            let load_client = Arc::new(MatchEngineClient::<C>::with_members(
                members,
                ClientConfig {
                    timeout: Duration::from_millis(500),
                    max_retries: 2,
                    max_forward_retries: 6,
                    initial_backoff: Duration::from_millis(20),
                    max_backoff: Duration::from_millis(100),
                },
            ));
            let results: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
            let timed_out: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let progress = Arc::new(AtomicUsize::new(0));

            let mut tasks = Vec::new();
            for task_id in 0..TASKS {
                let task_client = load_client.clone();
                let task_results = results.clone();
                let task_timed_out = timed_out.clone();
                let task_progress = progress.clone();
                tasks.push(tokio::spawn(async move {
                    for index in 0..WRITES_PER_TASK {
                        let key = format!("conc-{task_id}-{index}");
                        let request = MatchEngineRequest::place_with_client_id(
                            Order::new(
                                OrderSide::Buy,
                                100.0 - (task_id * WRITES_PER_TASK + index) as f64 * 0.001,
                                1.0,
                                0,
                            ),
                            key.clone(),
                        );
                        let deadline = Instant::now() + Duration::from_secs(12);
                        let mut attempt = 0usize;
                        loop {
                            let detail = match task_client.write(&request).await {
                                Ok(Ok(response)) => {
                                    let sequence = response
                                        .data
                                        .sequence
                                        .expect("a committed write has a sequence");
                                    task_results.lock().unwrap().push((key.clone(), sequence));
                                    task_progress.fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                                Ok(Err(error)) => format!("raft rejection: {error:?}"),
                                Err(error) => format!("transport error: {error:?}"),
                            };
                            task_timed_out.lock().unwrap().push(key.clone());
                            attempt += 1;
                            anyhow::ensure!(
                                attempt <= 20 && Instant::now() <= deadline,
                                "key {key} still failing after {attempt} attempts: {detail}"
                            );
                            C::sleep(Duration::from_millis(50)).await;
                        }
                        C::sleep(Duration::from_millis(3)).await;
                    }
                    Ok::<(), anyhow::Error>(())
                }));
            }

            // Kill the leader roughly halfway through the stream.
            let kill_deadline = Instant::now() + Duration::from_secs(10);
            while progress.load(Ordering::Relaxed) < (TOTAL_WRITES / 2) as usize
                && Instant::now() <= kill_deadline
            {
                C::sleep(Duration::from_millis(5)).await;
            }
            let committed_at_kill = progress.load(Ordering::Relaxed);
            anyhow::ensure!(
                committed_at_kill >= (TOTAL_WRITES / 2) as usize,
                "only {committed_at_kill} writes committed before the kill deadline"
            );
            println!(
                "concurrent-failover: stopping leader {leader} after {committed_at_kill} writes"
            );
            nodes[(leader - 1) as usize]
                .take()
                .unwrap()
                .shutdown()
                .await
                .unwrap();

            let live: Vec<MatchEngineClient<C>> = ADDRS
                .iter()
                .enumerate()
                .filter(|(index, _)| *index as u64 + 1 != leader)
                .map(|(index, addr)| client(index as u64 + 1, *addr, ClientConfig::default()))
                .collect();
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut new_leader = 0u64;
            loop {
                for observer in &live {
                    if let Ok(metrics) = observer.metrics().await
                        && let Some(current) = metrics.current_leader
                        && current != leader
                    {
                        new_leader = current;
                        break;
                    }
                }
                if new_leader != 0 {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() <= deadline,
                    "no new leader after killing {leader}"
                );
                C::sleep(Duration::from_millis(50)).await;
            }
            println!("concurrent-failover: new leader {new_leader}");

            for task in tasks {
                task.await.unwrap().unwrap();
            }

            let recorded = results.lock().unwrap().clone();
            anyhow::ensure!(
                recorded.len() as u64 == TOTAL_WRITES,
                "expected {TOTAL_WRITES} acknowledged writes, got {}: {:?}",
                recorded.len(),
                timed_out.lock().unwrap()
            );
            let keys: HashSet<_> = recorded.iter().map(|(key, _)| key).collect();
            let sequences: HashSet<u64> = recorded.iter().map(|(_, sequence)| *sequence).collect();
            anyhow::ensure!(
                keys.len() as u64 == TOTAL_WRITES,
                "duplicate client_order_id in acknowledged responses"
            );
            anyhow::ensure!(
                sequences.len() as u64 == TOTAL_WRITES,
                "duplicate sequence in acknowledged responses: {} distinct",
                sequences.len()
            );
            let expected_sequences: HashSet<u64> = (0..TOTAL_WRITES).collect();
            anyhow::ensure!(
                sequences == expected_sequences,
                "sequences have gaps: missing {:?}",
                expected_sequences
                    .difference(&sequences)
                    .collect::<Vec<_>>()
            );
            println!("concurrent-failover: {TOTAL_WRITES} distinct contiguous sequences");

            // A retry of a key that timed out around the kill must return the
            // original sequence and place no second order.
            let timed_out_keys = timed_out.lock().unwrap().clone();
            anyhow::ensure!(
                !timed_out_keys.is_empty(),
                "no write timed out during the failover"
            );
            let retry_key = timed_out_keys[0].clone();
            let original = recorded
                .iter()
                .find(|(key, _)| *key == retry_key)
                .map(|(_, sequence)| *sequence)
                .unwrap();
            let duplicate = load_client
                .write(&MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Sell, 1.0, 1.0, 0),
                    retry_key.clone(),
                ))
                .await??;
            anyhow::ensure!(
                duplicate.data.sequence == Some(original),
                "retry of {retry_key} returned {:?}, expected {original}",
                duplicate.data.sequence
            );

            let expected = TOTAL_WRITES.to_string();
            let deadline = Instant::now() + Duration::from_secs(10);
            for observer in &live {
                wait_for_sequence(observer, &expected, deadline).await?;
            }
            let orders = live[0].read(&"orderbook_orders".to_string()).await?;
            let tape = event_tape(&live[0]).await?;
            let tape_sequences: Vec<u64> = tape.iter().map(|event| event.order_sequence).collect();
            anyhow::ensure!(
                tape_sequences == (0..TOTAL_WRITES).collect::<Vec<_>>(),
                "event tape is not the contiguous committed prefix ({} events)",
                tape_sequences.len()
            );
            for observer in live.iter().skip(1) {
                anyhow::ensure!(
                    observer.read(&"orderbook_orders".to_string()).await? == orders,
                    "final order books diverged"
                );
                anyhow::ensure!(
                    event_tape(observer).await? == tape,
                    "final event tapes diverged"
                );
            }
            println!(
                "concurrent-failover: {} events and identical books on all three live nodes",
                tape.len()
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        // The stopped node is already gone; shut down live nodes leader first.
        let leader_index = (shutdown_target.load(Ordering::Relaxed) - 1) as usize;
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
