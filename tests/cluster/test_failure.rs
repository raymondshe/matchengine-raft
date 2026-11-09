//! Failure behavior: a stopped Raft core must stop the node.
//!
//! The WAL returns `io::Error` on a failed append or fsync, and openraft turns
//! storage errors into a fatal core exit. `MatchEngineNode::run()` must observe
//! that instead of leaving HTTP and `/health` serving while the node can no
//! longer make progress.

use std::path::Path;
use std::path::PathBuf;
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
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
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

/// Four voters run with no WAL fsync, so the binary enforces its four-voter
/// floor. Kill the leader, simulate the unsynced-tail loss a power cut could
/// drop by truncating its last WAL segment, and prove the remaining three
/// voters still commit. Restart the killed node and wait for it to catch up.
#[test]
fn four_voter_no_fsync_survives_leader_kill_and_tail_loss() {
    C::run(async {
        let temp = TempDir::new("four-voter-tail");
        let root = temp.path().to_path_buf();
        let prefix = "four-voter-tail";
        let addrs: Vec<String> = (0..4)
            .map(|offset| format!("127.0.0.1:{}", 21926 + offset))
            .collect();

        let mut children: Vec<Option<ChildGuard>> = Vec::new();
        for id in 1..=4u64 {
            children.push(Some(ChildGuard(start_node_process_no_fsync(
                id,
                &addrs[(id - 1) as usize],
                &root,
                prefix,
                &[],
            ))));
        }
        for addr in &addrs {
            wait_for_health(&format!("http://{addr}")).await;
        }

        // `RAFT_WAL_FSYNC_MS` is unset, so the binary requires four voters and
        // `/init` must receive all four in one request.
        let membership: Vec<(u64, String)> = (1..=4u64)
            .map(|id| (id, addrs[(id - 1) as usize].clone()))
            .collect();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .post(format!("http://{}/init", addrs[0]))
            .json(&membership)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "init failed: {}",
            response.text().await.unwrap()
        );

        let clients: Vec<MatchEngineClient<C>> = (1..=4u64)
            .map(|id| MatchEngineClient::<C>::new(id, addrs[(id - 1) as usize].clone()))
            .collect();
        let deadline = Instant::now() + Duration::from_secs(20);
        wait_for_any_leader(&clients, None, deadline).await;

        for i in 0..12u64 {
            write_until_accepted(
                &clients,
                None,
                &MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 100.0 - i as f64, 1.0, 0),
                    format!("four-voter-{i}"),
                ),
                i,
                deadline,
            )
            .await;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for client in &clients {
            wait_for_sequence(client, "12", deadline).await;
        }

        // Identify the current leader after the initial writes; it is the one
        // whose WAL tail this test will remove.
        let leader = wait_for_any_leader(&clients, None, deadline).await;

        // SIGKILL alone leaves the OS page cache intact, so after the kill the
        // test drops complete tail records to model the unsynced tail a power
        // loss could lose. The helper returns the pre- and post-truncation last
        // indexes so the test proves a record was actually removed.
        let last_wal = last_wal_file(
            &StorageConfig::new(root.clone())
                .with_instance_prefix(prefix)
                .log_dir(leader),
        );

        let mut leader_child = children[(leader - 1) as usize]
            .take()
            .expect("leader child process");
        leader_child.0.kill().unwrap();
        leader_child.0.wait().unwrap();
        drop(leader_child);

        let (last_index, kept_index) = truncate_last_wal_records(&last_wal, 3);
        assert!(
            kept_index < Some(last_index),
            "tail truncation did not remove a record: last {last_index}, kept {kept_index:?}"
        );

        // Exactly three of four voters remain, which is the quorum. Elect a new
        // leader and commit three more orders through it.
        let deadline = Instant::now() + Duration::from_secs(20);
        let new_leader = wait_for_any_leader(&clients, Some(leader), deadline).await;
        assert_ne!(leader, new_leader);
        for (offset, i) in (12..15u64).enumerate() {
            write_until_accepted(
                &clients,
                Some(leader),
                &MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 100.0 - i as f64, 1.0, 0),
                    format!("four-voter-{i}"),
                ),
                12 + offset as u64,
                deadline,
            )
            .await;
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        for (id, client) in clients.iter().enumerate() {
            if id as u64 + 1 == leader {
                continue;
            }
            wait_for_sequence(client, "15", deadline).await;
            let orders: Vec<Order> =
                serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                    .unwrap();
            assert_eq!(15, orders.len(), "survivor missed committed orders");
        }

        // Restart the killed node on its truncated WAL. It must rejoin as a
        // voter and catch up from the surviving three.
        C::sleep(Duration::from_millis(100)).await;
        children[(leader - 1) as usize] = Some(ChildGuard(start_node_process_no_fsync(
            leader,
            &addrs[(leader - 1) as usize],
            &root,
            prefix,
            &[],
        )));
        wait_for_health(&format!("http://{}", addrs[(leader - 1) as usize])).await;

        let restarted = &clients[(leader - 1) as usize];
        let deadline = Instant::now() + Duration::from_secs(30);
        wait_for_sequence(restarted, "15", deadline).await;
        let orders: Vec<Order> = serde_json::from_str(
            &restarted
                .read(&"orderbook_orders".to_string())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(15, orders.len(), "restarted node did not catch up");

        // The restarted node is still a voter: a write through the current
        // leader reaches it.
        let request = MatchEngineRequest::place_with_client_id(
            Order::new(OrderSide::Buy, 80.0, 1.0, 0),
            "four-voter-post-restart",
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        write_until_accepted(&clients, None, &request, 15, deadline).await;
        wait_for_sequence(restarted, "16", deadline).await;
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
        // These tests assume every acknowledged write survives SIGKILL and use a
        // single-voter cluster; keep the durable fsync mode so the no-fsync
        // four-voter policy does not apply.
        .env("RAFT_WAL_FSYNC_MS", "0")
        .envs(envs.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start the matchengine-raft binary")
}

fn start_node_process_no_fsync(
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
        // Unset selects the no-fsync mode, which also turns on the binary's
        // four-voter floor. This test intentionally runs that production profile.
        .env_remove("RAFT_WAL_FSYNC_MS")
        .envs(envs.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start the matchengine-raft binary")
}

fn last_wal_file(log_dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    files.pop().expect("node has at least one WAL segment")
}

/// Drops the last `records` complete frames from a WAL segment and returns the
/// original and remaining last log indexes. Truncating at a frame boundary
/// keeps the remaining file decodable; a real crash can also tear the last
/// frame, which the storage recovery tests cover separately.
fn truncate_last_wal_records(path: &Path, records: usize) -> (u64, Option<u64>) {
    let original = read_wal_file::<C>(path).unwrap();
    let last_index = original.last().map(|(index, _)| *index).unwrap();
    let bytes = std::fs::read(path).unwrap();

    let mut starts = Vec::with_capacity(original.len());
    let mut offset = 8; // MRFT header
    while offset + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset + 8 + len;
        if end > bytes.len() {
            break;
        }
        starts.push(offset);
        offset = end;
    }
    assert_eq!(
        original.len(),
        starts.len(),
        "decoded WAL records and physical frames disagree"
    );

    let keep = original.len().saturating_sub(records);
    assert!(
        original[keep..]
            .iter()
            .any(|(_, record)| matches!(record, WalRecord::Entry(_))),
        "tail truncation removed only commit markers"
    );
    let new_len = starts.get(keep).copied().unwrap_or(8);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(new_len as u64)
        .unwrap();
    let kept = read_wal_file::<C>(path).unwrap();
    assert_eq!(keep, kept.len(), "truncation kept the wrong frame count");
    (last_index, kept.last().map(|(index, _)| *index))
}

async fn wait_for_any_leader(
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
        assert!(
            Instant::now() <= deadline,
            "no leader elected (excluded {exclude:?})"
        );
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn write_until_accepted(
    clients: &[MatchEngineClient<C>],
    exclude: Option<u64>,
    request: &MatchEngineRequest,
    expected: u64,
    deadline: Instant,
) {
    loop {
        let leader = wait_for_any_leader(clients, exclude, deadline).await;
        match clients[(leader - 1) as usize].write(request).await {
            Ok(Ok(response)) => {
                assert_eq!(Some(expected), response.data.sequence);
                return;
            }
            Ok(Err(error)) => println!("write retry (leader {leader}): {error}"),
            Err(error) => println!("transport retry (leader {leader}): {error}"),
        }
        assert!(
            Instant::now() <= deadline,
            "write for sequence {expected} was not accepted"
        );
        C::sleep(Duration::from_millis(100)).await;
    }
}

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
        C::sleep(Duration::from_millis(100)).await;
    }
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
