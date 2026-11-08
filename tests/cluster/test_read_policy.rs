//! Default lease reads, explicit ReadIndex reads, and the read-policy status counters.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::network::read_policy::ReadPolicyMode;
use matchengine_raft::network::read_policy::ReadPolicyStatus;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const LEASE_ADDR: &str = "127.0.0.1:21930";
const READ_INDEX_ADDR: &str = "127.0.0.1:21932";

#[test]
fn default_lease_reads_serve_leader_reads_and_report_hits() {
    C::run(run_policy_case(LEASE_ADDR, None, "lease"));
}

#[test]
fn read_index_policy_serves_leader_reads_without_lease_attempts() {
    C::run(run_policy_case(
        READ_INDEX_ADDR,
        Some(ReadPolicyMode::ReadIndex),
        "read_index",
    ));
}

async fn run_policy_case(addr: &str, policy: Option<ReadPolicyMode>, expected_mode: &str) {
    let temp = TempDir::new("read-policy");
    let mut storage = StorageConfig::new(temp.path()).with_instance_prefix("read-policy");
    if let Some(policy) = policy {
        storage = storage.with_read_policy(policy);
    }
    let node = build_match_engine_node(1, addr.to_string(), default_raft_config(), storage)
        .await
        .unwrap();

    let client = MatchEngineClient::<C>::new(1, addr.to_string());
    client.init().await.unwrap().unwrap();
    wait_for_leader(&client).await;

    // Seed one resting order so /read and /events have data.
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            100.0,
            1.0,
            0,
        )))
        .await
        .unwrap()
        .unwrap();

    let key = "orderbook_depth:20".to_string();
    let depth = client.linearizable_read(&key).await.unwrap().unwrap();
    assert!(
        depth.contains("\"bids\"") || depth.contains("100"),
        "{depth}"
    );

    let page = client
        .events(&EventsRequest {
            after: None,
            limit: Some(1),
            wait_ms: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(1, page.events.len());

    // A local /read must not enter the leader-read policy.
    let _ = client.read(&key).await.unwrap();

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let status: ReadPolicyStatus = http
        .get(format!("http://{addr}/read-policy/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(expected_mode, status.mode);
    assert_eq!(0, status.get_linearizer.lease_attempts);

    if expected_mode == "lease" {
        assert_eq!(1, status.linearizable_read.lease_attempts);
        assert_eq!(1, status.events.lease_attempts);
        for metrics in [status.linearizable_read, status.events] {
            assert_eq!(
                metrics.lease_attempts,
                metrics.lease_hits + metrics.lease_fallbacks + metrics.lease_failures
            );
        }
        // A single node is its own quorum, so the lease path should hit; a
        // fallback would still be correct but indicates an unexpected state.
        assert_eq!(
            2, status.totals.lease_hits,
            "single-node lease reads should not need a ReadIndex fallback"
        );
    } else {
        assert_eq!(0, status.totals.lease_attempts);
        assert_eq!(2, status.totals.read_index_attempts);
        assert_eq!(0, status.totals.read_index_failures);
    }

    node.shutdown().await.unwrap();
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return;
        }
        assert!(Instant::now() <= deadline, "node 1 never became leader");
        C::sleep(Duration::from_millis(100)).await;
    }
}
