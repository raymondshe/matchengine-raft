//! Default lease reads, explicit ReadIndex reads, and the read-policy status counters.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::network::read_policy::ReadPolicyMode;
use matchengine_raft::network::read_policy::ReadPolicyStatus;
#[cfg(debug_assertions)]
use matchengine_raft::network::read_policy::TEST_LEASE_SKEW_ENV;
#[cfg(debug_assertions)]
use matchengine_raft::network::read_policy::TEST_LEASE_SKEW_TARGET_ENV;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const LEASE_ADDR: TestAddr = TestAddr::new(21930);
const READ_INDEX_ADDR: TestAddr = TestAddr::new(21932);
const LOCAL_LEASE_ADDR: TestAddr = TestAddr::new(21938);
const FOLLOWER_READ_ADDR: TestAddr = TestAddr::new(21939);
const STALE_LEASE_ADDRS: [TestAddr; 3] = [
    TestAddr::new(21946),
    TestAddr::new(21947),
    TestAddr::new(21948),
];

#[test]
fn default_local_lease_reads_serve_leader_reads_and_report_hits() {
    C::run(run_policy_case(LEASE_ADDR, None, "local_lease"));
}

#[test]
fn local_lease_reads_use_the_caller_side_cache() {
    C::run(run_policy_case(
        LOCAL_LEASE_ADDR,
        Some(ReadPolicyMode::LocalLease),
        "local_lease",
    ));
}

#[test]
fn read_index_policy_serves_leader_reads_without_lease_attempts() {
    C::run(run_policy_case(
        READ_INDEX_ADDR,
        Some(ReadPolicyMode::ReadIndex),
        "read_index",
    ));
}

#[test]
fn follower_read_fetches_a_leader_linearizer() {
    C::run(run_follower_read_case(FOLLOWER_READ_ADDR));
}

async fn run_follower_read_case(addr: impl std::fmt::Display) {
    let addr = addr.to_string();
    let temp = TempDir::new("follower-read");
    let storage = StorageConfig::new(temp.path()).with_instance_prefix("follower-read");
    let node = build_match_engine_node(1, addr.clone(), default_raft_config(), storage)
        .await
        .unwrap();

    let client = MatchEngineClient::<C>::new(1, addr.clone());
    client.init().await.unwrap().unwrap();
    wait_for_leader(&client).await;
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
    let depth = client.follower_read(&key).await.unwrap().unwrap();
    assert!(depth.contains("100"), "{depth}");

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let status: ReadPolicyStatus = http
        .get(format!("http://{addr}/read-policy/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(1, status.get_linearizer.lease_attempts);
    assert_eq!(1, status.get_linearizer.lease_hits);

    node.shutdown().await.unwrap();
}

async fn run_policy_case(
    addr: impl std::fmt::Display,
    policy: Option<ReadPolicyMode>,
    expected_mode: &str,
) {
    let addr = addr.to_string();
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

    if expected_mode == "read_index" {
        assert_eq!(0, status.totals.lease_attempts);
        assert_eq!(2, status.totals.read_index_attempts);
        assert_eq!(0, status.totals.read_index_failures);
    } else {
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
    }

    node.shutdown().await.unwrap();
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + crate::common::wait_secs(15);
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

/// E08: with a skewed caller-side lease, leader reads must fall back to
/// `ReadIndex`, never record a new lease hit, and still see the latest commit.
#[cfg(debug_assertions)]
#[test]
fn stale_lease_falls_back_to_read_index() {
    C::run(async {
        // The skew hook is sampled by `ReadPolicyRuntime::new` while each node
        // is built. Install it only for these builds and restore immediately so
        // concurrently running tests never observe it.
        let skew = LeaseSkewEnv::install("1000", &STALE_LEASE_ADDRS[0].http());
        let temp = TempDir::new("stale-lease");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("stale-lease")
            .with_read_policy(ReadPolicyMode::LocalLease);
        let mut nodes = Vec::new();
        for (index, addr) in STALE_LEASE_ADDRS.iter().enumerate() {
            nodes.push(Some(
                build_match_engine_node(
                    (index + 1) as u64,
                    addr.to_string(),
                    default_raft_config(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            ));
        }
        drop(skew);

        let clients: Vec<_> = STALE_LEASE_ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| MatchEngineClient::<C>::new((index + 1) as u64, addr.to_string()))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            loop {
                if let Ok(metrics) = clients[0].metrics().await
                    && metrics.current_leader == Some(1)
                {
                    break;
                }
                assert!(Instant::now() < deadline, "node 1 never became leader");
                C::sleep(Duration::from_millis(50)).await;
            }
            clients[0]
                .add_learner((2, STALE_LEASE_ADDRS[1].to_string()))
                .await
                .unwrap()
                .unwrap();
            clients[0]
                .add_learner((3, STALE_LEASE_ADDRS[2].to_string()))
                .await
                .unwrap()
                .unwrap();
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
            for client in &clients {
                wait_for_sequence(client, "10", deadline).await;
            }

            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let url = format!("{}/read-policy/status", STALE_LEASE_ADDRS[0].http_url());
            let before: ReadPolicyStatus =
                http.get(&url).send().await.unwrap().json().await.unwrap();
            assert_eq!("local_lease", before.mode);

            // Both leader-consistent read paths must use the fallback.
            let sequence = clients[0]
                .linearizable_read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
                .unwrap();
            assert_eq!("10", sequence, "ReadIndex read saw a stale sequence");
            let page = clients[0]
                .events(&EventsRequest {
                    after: None,
                    limit: Some(100),
                    wait_ms: None,
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(10, page.events.len());

            let after: ReadPolicyStatus =
                http.get(&url).send().await.unwrap().json().await.unwrap();
            for (label, before, after) in [
                (
                    "linearizable_read",
                    before.linearizable_read,
                    after.linearizable_read,
                ),
                ("events", before.events, after.events),
            ] {
                println!("{label}: before={before:?} after={after:?}");
                assert_eq!(before.lease_attempts + 1, after.lease_attempts, "{label}");
                assert_eq!(
                    before.lease_hits, after.lease_hits,
                    "{label} got a lease hit"
                );
                assert_eq!(before.lease_fallbacks + 1, after.lease_fallbacks, "{label}");
                assert_eq!(
                    before.read_index_attempts + 1,
                    after.read_index_attempts,
                    "{label}"
                );
                assert_eq!(
                    before.read_index_failures, after.read_index_failures,
                    "{label}"
                );
            }

            // Contrast: `read_index_policy_serves_leader_reads_without_lease_attempts`
            // proves the explicit `--read-policy read_index` mode reports zero
            // lease attempts. This node must still identify as local_lease.
            assert_eq!("local_lease", after.mode);

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

#[cfg(debug_assertions)]
async fn wait_for_sequence(client: &MatchEngineClient<C>, expected: &str, deadline: Instant) {
    loop {
        if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
            && value == expected
        {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "node did not reach sequence {expected}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

/// Restores `MATCHENGINE_TEST_LEASE_SKEW_MS` on drop so a panic cannot leave a
/// process-wide hook armed for other tests.
#[cfg(debug_assertions)]
struct LeaseSkewEnv(Option<OsString>, Option<OsString>);

#[cfg(debug_assertions)]
impl LeaseSkewEnv {
    fn install(value: &str, target_addr: &str) -> Self {
        let previous = (
            std::env::var_os(TEST_LEASE_SKEW_ENV),
            std::env::var_os(TEST_LEASE_SKEW_TARGET_ENV),
        );
        // SAFETY: edition 2024 marks process-wide env mutation unsafe. The
        // variables are set only while the test's nodes are built and restored
        // immediately afterwards, and the target filter prevents other nodes
        // created in that window from taking the fallback path.
        unsafe {
            std::env::set_var(TEST_LEASE_SKEW_ENV, value);
            std::env::set_var(TEST_LEASE_SKEW_TARGET_ENV, target_addr);
        }
        Self(previous.0, previous.1)
    }
}

#[cfg(debug_assertions)]
impl Drop for LeaseSkewEnv {
    fn drop(&mut self) {
        // SAFETY: see `install`.
        unsafe {
            match self.0.take() {
                Some(value) => std::env::set_var(TEST_LEASE_SKEW_ENV, value),
                None => std::env::remove_var(TEST_LEASE_SKEW_ENV),
            }
            match self.1.take() {
                Some(value) => std::env::set_var(TEST_LEASE_SKEW_TARGET_ENV, value),
                None => std::env::remove_var(TEST_LEASE_SKEW_TARGET_ENV),
            }
        }
    }
}
