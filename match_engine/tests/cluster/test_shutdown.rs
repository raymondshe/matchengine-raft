//! Graceful SIGTERM shutdown for the containers-mode engine binary.
//!
//! The engine is PID 1 in a one-container pod, where the kernel ignores signals
//! with the default disposition. Without an explicit handler a rollout waits the
//! full termination grace period and then SIGKILLs the process.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::time::sleep;

use crate::common::{TempDir, TestAddr};

const ADDR: TestAddr = TestAddr::new(21940);

#[tokio::test]
async fn sigterm_stops_the_node_promptly() {
    let dir = TempDir::new("shutdown");
    let mut child = Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            "0",
            "--http-addr",
            &ADDR.http(),
            "--instance-prefix",
            "shutdown",
        ])
        .arg("--data-dir")
        .arg(dir.path())
        .env("RAFT_WAL_FSYNC_MS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start engine binary");

    let client = reqwest::Client::new();
    let deadline = Instant::now() + crate::common::wait_secs(20);
    let mut healthy = false;
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("{}/health", ADDR.http_url()))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            && response.status().is_success()
        {
            healthy = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "engine did not answer /health before shutdown");

    let pid = child.id().expect("child pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let started = Instant::now();
    let status = tokio::time::timeout(crate::common::wait_secs(10), child.wait())
        .await
        .expect("engine ignored SIGTERM for 10s")
        .expect("wait for engine");
    assert!(
        status.success(),
        "engine exited with {status:?} instead of a clean shutdown"
    );
    assert!(
        started.elapsed() < crate::common::wait_secs(10),
        "engine shutdown took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn sigterm_drains_promptly_under_sustained_http_load() {
    let dir = TempDir::new("shutdown-load");
    let addr = TestAddr::new(21942);
    let mut child = Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            "0",
            "--http-addr",
            &addr.http(),
            "--instance-prefix",
            "shutdown-load",
        ])
        .arg("--data-dir")
        .arg(dir.path())
        .env("RAFT_WAL_FSYNC_MS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start engine binary");

    let client = reqwest::Client::new();
    let deadline = Instant::now() + crate::common::wait_secs(20);
    let mut healthy = false;
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("{}/health", addr.http_url()))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            && response.status().is_success()
        {
            healthy = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "engine did not answer /health before shutdown");

    // Mirror the live roll: an initialized single node, real Raft writes and
    // several keep-alive clients still busy when SIGTERM arrives.
    client
        .post(format!("{}/init", addr.http_url()))
        .json(&Vec::<u64>::new())
        .send()
        .await
        .expect("init");
    let load = tokio::spawn(async move {
        let client = reqwest::Client::new();
        loop {
            let _ = client
                .post(format!("{}/write", addr.http_url()))
                .json(&serde_json::json!({
                    "place": {
                        "side": "Buy",
                        "price": "100",
                        "volume": "1",
                        "symbol_id": 0,
                        "tif": "GTC"
                    }
                }))
                .timeout(Duration::from_millis(500))
                .send()
                .await;
            sleep(Duration::from_millis(2)).await;
        }
    });
    sleep(Duration::from_millis(500)).await;

    let pid = child.id().expect("child pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let started = Instant::now();
    let status = tokio::time::timeout(crate::common::wait_secs(8), child.wait())
        .await
        .expect("engine ignored SIGTERM for 8s under load")
        .expect("wait for engine");
    load.abort();
    assert!(
        status.success(),
        "engine exited with {status:?} instead of a clean shutdown"
    );
    assert!(
        started.elapsed() < crate::common::wait_secs(8),
        "engine shutdown under load took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn shutdown_drains_promptly_in_a_four_voter_cluster_under_load() {
    use std::collections::BTreeSet;

    use matchengine_raft::StorageConfig;
    use matchengine_raft::build_match_engine_node;
    use matchengine_raft::client::MatchEngineClient;
    use matchengine_raft::default_raft_config;
    use matchengine_raft::order_book::Order;
    use matchengine_raft::order_book::OrderSide;
    use matchengine_raft::store::MatchEngineRequest;

    type C = matchengine_raft::MatchEngineTypeConfig;

    let dir = TempDir::new("shutdown-cluster-load");
    let storage = StorageConfig::new(dir.path()).with_instance_prefix("shutdown-cluster");
    let addrs: Vec<TestAddr> = (0..4).map(|i| TestAddr::new(21960 + i)).collect();
    let mut nodes = Vec::new();
    for (id, addr) in addrs.iter().enumerate() {
        nodes.push(
            build_match_engine_node(
                id as u64,
                addr.to_string(),
                default_raft_config(),
                storage.clone(),
            )
            .await
            .expect("build node"),
        );
    }
    let clients: Vec<MatchEngineClient<C>> = addrs
        .iter()
        .enumerate()
        .map(|(id, addr)| MatchEngineClient::new(id as u64, addr.to_string()))
        .collect();
    clients[0].init().await.unwrap().unwrap();
    // Wait for the single-node election before adding learners.
    let deadline = Instant::now() + crate::common::wait_secs(15);
    loop {
        if clients[0]
            .metrics()
            .await
            .ok()
            .and_then(|m| m.current_leader)
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "no leader elected");
        sleep(Duration::from_millis(100)).await;
    }
    for id in 1..4u64 {
        clients[0]
            .add_learner((id, addrs[id as usize].to_string()))
            .await
            .unwrap()
            .unwrap();
    }
    clients[0]
        .change_membership(&BTreeSet::from([0, 1, 2, 3]))
        .await
        .unwrap()
        .unwrap();

    // Sustained writes against the leader while a voter is asked to shut down.
    let write_client = MatchEngineClient::<C>::new(0, addrs[0].to_string());
    let load = tokio::spawn(async move {
        loop {
            let _ = write_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0,
                    1.0,
                    0,
                )))
                .await;
        }
    });
    sleep(Duration::from_millis(500)).await;

    let victim = nodes.remove(3);
    let started = Instant::now();
    tokio::time::timeout(crate::common::wait_secs(12), victim.shutdown())
        .await
        .expect("voter shutdown hung for 12s under cluster load")
        .expect("clean shutdown");
    load.abort();
    assert!(
        started.elapsed() < crate::common::wait_secs(12),
        "shutdown took {:?}",
        started.elapsed()
    );

    for node in nodes {
        node.shutdown().await.expect("shutdown remaining node");
    }
}

#[tokio::test]
async fn id_offset_changes_the_raft_id_and_data_directory() {
    let dir = TempDir::new("id-offset");
    let addr = TestAddr::new(21941);
    let mut child = Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            "0",
            "--id-offset",
            "4",
            "--http-addr",
            &addr.http(),
            "--instance-prefix",
            "offset",
        ])
        .arg("--data-dir")
        .arg(dir.path())
        .env("RAFT_WAL_FSYNC_MS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start engine binary");

    let client = reqwest::Client::new();
    let deadline = Instant::now() + crate::common::wait_secs(20);
    let mut node_id = None;
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("{}/health", addr.http_url()))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            && response.status().is_success()
            && let Ok(body) = response.json::<serde_json::Value>().await
        {
            node_id = body["node_id"].as_u64();
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(node_id, Some(4), "engine did not use the Raft ID offset");
    assert!(
        dir.path().join("offset-4/log").exists(),
        "data directory did not use the offset node ID"
    );

    let pid = child.id().expect("child pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(crate::common::wait_secs(10), child.wait())
        .await
        .expect("engine ignored SIGTERM for 10s")
        .expect("wait for engine");
    assert!(status.success());
}
