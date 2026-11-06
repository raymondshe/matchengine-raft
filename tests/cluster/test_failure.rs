//! Failure behavior: a stopped Raft core must stop the node.
//!
//! The WAL returns `io::Error` on a failed append or fsync, and openraft turns
//! storage errors into a fatal core exit. `MatchEngineNode::run()` must observe
//! that instead of leaving HTTP and `/health` serving while the node can no
//! longer make progress.

use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
#[cfg(debug_assertions)]
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

#[test]
fn a_stopped_raft_core_stops_the_node() {
    C::run(async {
        let temp = TempDir::new("failure");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("failure");
        let node = build_match_engine_node(
            1,
            "127.0.0.1:21918".to_string(),
            default_raft_config(),
            storage,
        )
        .await
        .unwrap();

        // A fatal WAL/snapshot error leaves RaftCore in the same state as this
        // explicit stop: `running_state` becomes `Err` and the core exits.
        node.raft.shutdown().await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(10), node.run())
            .await
            .expect("run() did not observe the stopped Raft core");
        let error = result.expect_err("run() must fail after the Raft core stops");
        assert!(
            error.to_string().contains("Raft core stopped"),
            "unexpected error: {error}"
        );
    });
}

#[cfg(debug_assertions)]
#[test]
fn a_wal_storage_error_stops_the_node() {
    C::run(async {
        assert_wal_failure_stops_the_node(21919, true).await;
        assert_wal_failure_stops_the_node(21920, false).await;
    });
}

/// Arms a one-shot WAL append failure, writes, and expects the node to exit.
#[cfg(debug_assertions)]
async fn assert_wal_failure_stops_the_node(port: u16, storage_full: bool) {
    let label = if storage_full { "enospc" } else { "eio" };
    let temp = TempDir::new(label);
    let storage = StorageConfig::new(temp.path()).with_instance_prefix(label);
    let addr = format!("127.0.0.1:{port}");
    let node = build_match_engine_node(1, addr.clone(), default_raft_config(), storage)
        .await
        .unwrap();

    let client = MatchEngineClient::<C>::new(1, addr);
    client.init().await.unwrap().unwrap();
    wait_for_leader(&client).await;

    // Arm the failure after initialization so the next client write is the
    // first append that consumes it.
    node.log_store.inject_append_error(storage_full);
    let node_run = tokio::spawn(node.run());

    let request = MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0));
    let _ = tokio::time::timeout(Duration::from_secs(5), client.write(&request)).await;

    let result = tokio::time::timeout(Duration::from_secs(10), node_run)
        .await
        .expect("run() did not stop after a WAL append failure")
        .expect("node run task panicked");
    let error = result.expect_err("run() must fail after a fatal WAL error");
    assert!(
        error.to_string().contains("Raft core stopped"),
        "unexpected error: {error}"
    );
}

#[test]
fn sigkill_during_writes_recovers_the_committed_prefix() {
    C::run(async {
        let temp = TempDir::new("sigkill");
        let addr = "127.0.0.1:21921";
        let base = format!("http://{addr}");
        let data_dir = temp.path().to_path_buf();

        let mut child = ChildGuard(start_node_process(1, addr, &data_dir, "sigkill", &[]));
        wait_for_health(&base).await;

        let client = MatchEngineClient::<C>::new(1, addr.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let acked = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = tokio::spawn(write_batches(client, acked.clone(), stop.clone()));

        let deadline = Instant::now() + Duration::from_secs(10);
        while acked.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "writer never committed a batch");
            C::sleep(Duration::from_millis(10)).await;
        }
        // Let the next batch be in flight when the process dies.
        C::sleep(Duration::from_millis(10)).await;

        child.0.kill().unwrap();
        child.0.wait().unwrap();
        stop.store(true, Ordering::SeqCst);
        writer.abort();
        let _ = writer.await;

        let committed = acked.load(Ordering::SeqCst);
        assert!(committed > 0, "no acknowledged sequence before SIGKILL");

        // A short pause lets the listener port leave TIME_WAIT before the restart.
        C::sleep(Duration::from_millis(100)).await;
        let restarted = ChildGuard(start_node_process(1, addr, &data_dir, "sigkill", &[]));
        wait_for_health(&base).await;

        let reader = MatchEngineClient::<C>::new(1, addr.to_string());
        wait_for_leader(&reader).await;
        let restored: u64 = reader
            .read(&"orderbook_sequence".to_string())
            .await
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            restored >= committed,
            "restarted node lost acknowledged writes: acked {committed}, restored {restored}"
        );

        drop(restarted);
    });
}

/// Kill a learner after it has received a snapshot chunk but before it installs
/// the snapshot. The receiver buffers the stream in memory, so nothing durable
/// may exist; the restart must retry and install the complete snapshot.
#[cfg(debug_assertions)]
#[test]
fn sigkill_during_learner_snapshot_install_leaves_no_partial_snapshot() {
    C::run(async {
        let leader_temp = TempDir::new("install-leader");
        let learner_temp = TempDir::new("install-learner");
        let leader_addr = "127.0.0.1:21922";
        let learner_addr = "127.0.0.1:21923";
        let learner_base = format!("http://{learner_addr}");

        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.heartbeat_min_interval = None;
        config.election_timeout_max = 1_000;
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
        // Purge everything the snapshot covers so the learner must install it.
        config.max_in_snapshot_log_to_keep = 0;

        let storage = StorageConfig::new(leader_temp.path()).with_instance_prefix("install-leader");
        let node = build_match_engine_node(1, leader_addr.to_string(), config, storage)
            .await
            .unwrap();
        let leader = MatchEngineClient::<C>::new(1, leader_addr.to_string());
        leader.init().await.unwrap().unwrap();
        wait_for_leader(&leader).await;

        for i in 0..30u64 {
            leader
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

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let metrics = leader.metrics().await.unwrap();
            if let (Some(snapshot), Some(purged)) = (metrics.snapshot, metrics.purged)
                && purged.index >= snapshot.index
            {
                break;
            }
            assert!(Instant::now() < deadline, "snapshot/purge did not happen");
            C::sleep(Duration::from_millis(50)).await;
        }

        let marker = learner_temp.path().join("snapshot-install-started");
        let learner_data = learner_temp.path().to_path_buf();
        let mut child = ChildGuard(start_node_process(
            2,
            learner_addr,
            &learner_data,
            "install-learner",
            &[
                ("MATCHENGINE_TEST_SNAPSHOT_INSTALL_DELAY_MS", "5000"),
                (
                    "MATCHENGINE_TEST_SNAPSHOT_INSTALL_MARKER",
                    marker.to_str().unwrap(),
                ),
            ],
        ));
        wait_for_health(&learner_base).await;

        // `add_learner` blocks until the learner catches up; run it in the
        // background so the test can kill the learner mid-install.
        let adder = MatchEngineClient::<C>::new(1, leader_addr.to_string());
        let add =
            tokio::spawn(async move { adder.add_learner((2, learner_addr.to_string())).await });

        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "learner never started the snapshot install"
            );
            C::sleep(Duration::from_millis(20)).await;
        }

        child.0.kill().unwrap();
        child.0.wait().unwrap();

        let snapshot_path = StorageConfig::new(learner_temp.path())
            .with_instance_prefix("install-learner")
            .snapshot_dir(2)
            .join("current.snap");
        assert!(
            !snapshot_path.exists(),
            "a killed snapshot install left a partial current.snap"
        );

        // Restart without the delay: the leader must resend and complete the install.
        C::sleep(Duration::from_millis(100)).await;
        let restarted = ChildGuard(start_node_process(
            2,
            learner_addr,
            &learner_data,
            "install-learner",
            &[],
        ));
        wait_for_health(&learner_base).await;

        let learner = MatchEngineClient::<C>::new(2, learner_addr.to_string());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(value) = learner.read(&"orderbook_sequence".to_string()).await
                && value == "30"
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "learner did not retry the snapshot install"
            );
            C::sleep(Duration::from_millis(100)).await;
        }

        let orders: Vec<Order> =
            serde_json::from_str(&learner.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(30, orders.len());
        assert!(
            snapshot_path.exists(),
            "the restarted learner did not persist the installed snapshot"
        );

        // The original add-learner request completes once the learner catches up.
        let _ = tokio::time::timeout(Duration::from_secs(5), add).await;
        drop(restarted);
        node.shutdown().await.unwrap();
    });
}

async fn write_batches(client: MatchEngineClient<C>, acked: Arc<AtomicU64>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let batch: Vec<MatchEngineRequest> = (0..16)
            .map(|_| MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0)))
            .collect();
        if let Ok(Ok(response)) = client.write_batch(&batch).await {
            let sequence = response
                .data
                .batch
                .iter()
                .filter_map(|item| item.sequence)
                .max()
                .or(response.data.sequence);
            if let Some(sequence) = sequence {
                acked.fetch_max(sequence, Ordering::SeqCst);
            }
        }
    }
}

/// A child node process that is killed and reaped when the test ends or panics.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_node_process(
    id: u64,
    addr: &str,
    data_dir: &Path,
    prefix: &str,
    envs: &[(&str, &str)],
) -> Child {
    let id = id.to_string();
    Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            &id,
            "--http-addr",
            addr,
            "--instance-prefix",
            prefix,
        ])
        .arg("--data-dir")
        .arg(data_dir)
        .envs(envs.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start the matchengine-raft binary")
}

async fn wait_for_health(base: &str) {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = http.get(format!("{base}/health")).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(Instant::now() < deadline, "node HTTP API did not start");
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return;
        }
        assert!(Instant::now() <= deadline, "node never became leader");
        C::sleep(Duration::from_millis(100)).await;
    }
}
