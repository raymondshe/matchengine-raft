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

        let mut child = ChildGuard(start_node_process(addr, &data_dir));
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
        let restarted = ChildGuard(start_node_process(addr, &data_dir));
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

fn start_node_process(addr: &str, data_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            "1",
            "--http-addr",
            addr,
            "--instance-prefix",
            "sigkill",
        ])
        .arg("--data-dir")
        .arg(data_dir)
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
