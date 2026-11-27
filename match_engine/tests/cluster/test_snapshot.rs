use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::state_machine::StateMachineData;
use matchengine_raft::store::state_machine::read_snapshot_file;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;
use serde_json::Value;

use crate::common::TempDir;
use crate::common::TestAddr;

/// A learner that joins after the leader purged its log must be caught up with a
/// streamed snapshot over gRPC.
#[test]
fn test_grpc_snapshot_install() {
    MatchEngineTypeConfig::run(test_grpc_snapshot_install_inner()).unwrap();
}

async fn test_grpc_snapshot_install_inner() -> anyhow::Result<()> {
    let addr1 = TestAddr::new(21101);
    let addr2 = TestAddr::new(21102);

    let config = || {
        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.heartbeat_min_interval = None;
        config.election_timeout_max = 1_000;
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
        // Purge every log already covered by a snapshot, so a node that joins
        // later cannot be caught up by log replication alone.
        config.max_in_snapshot_log_to_keep = 0;
        config
    };

    let temp = crate::common::TempDir::new("snapshot");
    let storage = StorageConfig::new(temp.path()).with_instance_prefix("snapshot");

    let node1 = build_match_engine_node(1, addr1.to_string(), config(), storage.clone()).await?;
    let node2 = build_match_engine_node(2, addr2.to_string(), config(), storage.clone()).await?;

    let client1 = MatchEngineClient::<MatchEngineTypeConfig>::new(1, addr1.to_string());
    client1.init().await??;

    // `init` only appends the initial membership; the first election still has to
    // happen before client writes can be accepted.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(metrics) = client1.metrics().await
            && metrics.current_leader == Some(1)
        {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!("node 1 never became leader after init");
        }
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }

    // 30 resting orders give the snapshot policy enough log entries to trigger.
    // Prices are distinct so nothing crosses and every order stays in the book.
    for i in 0..30 {
        client1
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0 - f64::from(i),
                1.0,
                0,
            )))
            .await??;
    }

    // Wait until a snapshot exists and the log it covers has been purged.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let metrics = client1.metrics().await?;
        let snapshot = metrics.snapshot.map(|log_id| log_id.index);
        let purged = metrics.purged.map(|log_id| log_id.index);

        if let (Some(snapshot), Some(purged)) = (snapshot, purged)
            && purged >= snapshot
        {
            break;
        }

        if Instant::now() > deadline {
            anyhow::bail!(
                "snapshot/purge did not happen: snapshot={:?}, purged={:?}",
                metrics.snapshot,
                metrics.purged
            );
        }

        MatchEngineTypeConfig::sleep(Duration::from_millis(100)).await;
    }

    // Node 2 has no log history of its own; this must go through InstallSnapshot.
    client1.add_learner((2, addr2.to_string())).await??;

    let client2 = MatchEngineClient::<MatchEngineTypeConfig>::new(2, addr2.to_string());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(value) = client2.read(&"orderbook_sequence".to_string()).await
            && value == "30"
        {
            break;
        }

        if Instant::now() > deadline {
            anyhow::bail!("node 2 did not install the snapshot and catch up");
        }

        MatchEngineTypeConfig::sleep(Duration::from_millis(100)).await;
    }

    // The snapshot, not log replay, carried the book: all 30 orders are present.
    let orders: Vec<Order> =
        serde_json::from_str(&client2.read(&"orderbook_orders".to_string()).await?)?;
    assert_eq!(30, orders.len());

    // The snapshot also carries the retained event window. Inspect node 2's
    // local state machine, not the leader's.
    let (events, _, _) = node2
        .state_machine
        .inner()
        .lock()
        .await
        .events_since(None, 100);
    assert_eq!(30, events.len(), "snapshot lost the retained event window");
    assert!(
        events
            .iter()
            .all(|event| event.kind == MatchEventKind::Rested),
        "unexpected events after snapshot install: {events:?}"
    );

    // The leader measured the send and the learner measured the receive.
    let http = reqwest::Client::builder().no_proxy().build()?;
    for (label, addr, sent) in [("leader", addr1, true), ("learner", addr2, false)] {
        let response = http.get(format!("http://{addr}/metrics")).send().await?;
        assert_eq!(StatusCode::OK, response.status());
        let body: Value = response.json().await?;
        let transport = &body["Ok"]["transport_metrics"];
        println!("{label} transport: {transport}");
        let count = if sent {
            transport["snapshot_sent"].as_u64().unwrap_or(0)
        } else {
            transport["snapshot_received"].as_u64().unwrap_or(0)
        };
        assert!(count >= 1, "{label} snapshot counter: {transport}");
        let bytes = if sent {
            transport["snapshot_sent_bytes"].as_u64().unwrap_or(0)
        } else {
            transport["snapshot_received_bytes"].as_u64().unwrap_or(0)
        };
        assert!(bytes > 0, "{label} snapshot bytes: {transport}");
        let average = if sent {
            transport["snapshot_sent_avg_micros"].as_u64().unwrap_or(0)
        } else {
            transport["snapshot_received_avg_micros"]
                .as_u64()
                .unwrap_or(0)
        };
        assert!(average > 0, "{label} snapshot average: {transport}");
    }

    node1.shutdown().await?;
    node2.shutdown().await?;

    Ok(())
}

/// Even with the log-count policy disabled, a timer must build a snapshot for
/// a node that keeps applying entries.
#[test]
fn test_periodic_snapshot_timer() {
    MatchEngineTypeConfig::run(test_periodic_snapshot_timer_inner()).unwrap();
}

async fn test_periodic_snapshot_timer_inner() -> anyhow::Result<()> {
    let addr = TestAddr::new(21103);
    let temp = crate::common::TempDir::new("periodic-snapshot");
    let storage = StorageConfig::new(temp.path())
        .with_instance_prefix("periodic-snapshot")
        .with_snapshot_interval(Duration::from_millis(50));

    let mut config = default_raft_config();
    config.snapshot_policy = SnapshotPolicy::Never;
    let node = build_match_engine_node(1, addr.to_string(), config, storage.clone()).await?;

    let client = MatchEngineClient::<MatchEngineTypeConfig>::new(1, addr.to_string());
    client.init().await??;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!("node 1 never became leader");
        }
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }

    for i in 0..5 {
        client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0 - f64::from(i),
                1.0,
                0,
            )))
            .await??;
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let metrics = client.metrics().await?;
        if metrics.snapshot.map(|id| id.index).unwrap_or(0) >= 5 {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "periodic snapshot did not cover the applied entries: snapshot={:?}, applied={:?}",
                metrics.snapshot,
                metrics.last_applied
            );
        }
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }

    let snapshot_path = storage.snapshot_dir(1).join("current.snap");
    assert!(
        snapshot_path.exists(),
        "the timer must persist the snapshot file"
    );

    node.shutdown().await?;
    Ok(())
}

const CURSOR_ADDRS: [TestAddr; 3] = [
    TestAddr::new(21104),
    TestAddr::new(21105),
    TestAddr::new(21106),
];
const CANCEL_LEADER_ADDR: TestAddr = TestAddr::new(21107);
const CANCEL_LEARNER_ADDR: TestAddr = TestAddr::new(21108);
const CANCEL_CHUNK_SIZE: u64 = 16 * 1024;

fn cursor_config() -> openraft::Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
    config.heartbeat_min_interval = None;
    config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
    config.max_in_snapshot_log_to_keep = 0;
    config
}

/// E09: a follower's pre-install `/events` cursor must remain valid after it
/// installs a snapshot over gRPC. The snapshot may move the retained window's
/// boundary, but the cursor must resume with no silent skip or duplicate.
#[test]
fn snapshot_install_preserves_a_followers_event_cursor_contract() {
    MatchEngineTypeConfig::run(async {
        let temp = TempDir::new("snapshot-cursor");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("snapshot-cursor");
        let mut nodes = Vec::new();
        for (index, addr) in CURSOR_ADDRS.iter().enumerate() {
            nodes.push(Some(
                build_match_engine_node(
                    (index + 1) as u64,
                    addr.to_string(),
                    cursor_config(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            ));
        }
        let clients: Vec<_> = CURSOR_ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| {
                MatchEngineClient::<MatchEngineTypeConfig>::new(
                    (index + 1) as u64,
                    addr.to_string(),
                )
            })
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            wait_for_leader_id(&clients[0], 1, deadline).await;
            clients[0]
                .add_learner((2, CURSOR_ADDRS[1].to_string()))
                .await
                .unwrap()
                .unwrap();
            clients[0]
                .add_learner((3, CURSOR_ADDRS[2].to_string()))
                .await
                .unwrap()
                .unwrap();
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3]))
                .await
                .unwrap()
                .unwrap();

            for i in 0..30u64 {
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
                wait_for_sequence_snapshot(client, "30", deadline).await;
            }

            // Take the cursor from the follower's own retained window, before
            // it falls behind and before any snapshot exists on that node.
            let (first_page, cursor, _) = nodes[1]
                .as_ref()
                .unwrap()
                .state_machine
                .inner()
                .lock()
                .await
                .events_since(None, 100);
            assert_eq!(30, first_page.len());
            let cursor = cursor.expect("the 30 committed events form a cursor");
            println!("snapshot-cursor: follower cursor before stop = {cursor:?}");

            // Keep node 2 in membership but stop it. The remaining two voters
            // commit 30 more writes, snapshot and purge past its tail.
            let follower_tail = clients[1]
                .metrics()
                .await
                .unwrap()
                .last_applied
                .map(|id| id.index)
                .unwrap_or(0);
            nodes[1].take().unwrap().shutdown().await.unwrap();
            for i in 0..30u64 {
                clients[0]
                    .write(&MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        70.0 - i as f64,
                        1.0,
                        0,
                    )))
                    .await
                    .unwrap()
                    .unwrap();
            }
            loop {
                let metrics = clients[0].metrics().await.unwrap();
                if let (Some(snapshot), Some(purged)) = (metrics.snapshot, metrics.purged)
                    && purged.index >= snapshot.index
                    && purged.index >= follower_tail
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "leader did not snapshot/purge past the follower tail {follower_tail}"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }

            // The snapshot is the authoritative retained event window; its
            // exact event count depends on where the log-count policy landed.
            let (_, leader_body) =
                read_snapshot_file::<MatchEngineTypeConfig>(&storage.snapshot_dir(1))
                    .unwrap()
                    .expect("leader has a snapshot");
            let snapshot_data: StateMachineData = postcard::from_bytes(&leader_body).unwrap();
            let snapshot_events = snapshot_data.events.len();
            assert!(snapshot_events > 30, "snapshot lost the first 30 events");
            println!("snapshot-cursor: leader snapshot retains {snapshot_events} events");

            // Restart on the original storage: the purged log forces a gRPC
            // snapshot install rather than log replication.
            nodes[1] = Some(
                build_match_engine_node(
                    2,
                    CURSOR_ADDRS[1].to_string(),
                    cursor_config(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            );
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            loop {
                let body: Value = http
                    .get(format!("{}/metrics", CURSOR_ADDRS[1].http_url()))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let received = body["Ok"]["transport_metrics"]["snapshot_received"]
                    .as_u64()
                    .unwrap_or(0);
                if received >= 1
                    && nodes[1]
                        .as_ref()
                        .unwrap()
                        .state_machine
                        .inner()
                        .lock()
                        .await
                        .retained_event_count()
                        == snapshot_events
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "follower did not install the snapshot with the retained event window"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }

            // The pre-install cursor resumes at the snapshot boundary: all
            // events committed after the cursor and present in the snapshot.
            let (resumed, _, has_more) = nodes[1]
                .as_ref()
                .unwrap()
                .state_machine
                .inner()
                .lock()
                .await
                .events_since(Some(cursor), 100);
            println!(
                "snapshot-cursor: resumed {} events, has_more={has_more}",
                resumed.len()
            );
            assert_eq!(
                snapshot_events - 30,
                resumed.len(),
                "cursor skipped or replayed events"
            );
            assert!(
                resumed.iter().all(|event| event.event_id > cursor),
                "cursor replayed an event it had already seen"
            );
            assert!(!has_more);

            let leader_status = clients[0].events_status().await.unwrap();
            let follower_status = clients[1].events_status().await.unwrap();
            assert_eq!(
                leader_status.oldest_event_id, follower_status.oldest_event_id,
                "leader and follower disagree on the retained window boundary"
            );
            assert_eq!(snapshot_events, follower_status.retained_events);
            assert_eq!(
                0, follower_status.evictions_total,
                "events were silently evicted"
            );

            let body: Value = http
                .get(format!("{}/metrics", CURSOR_ADDRS[1].http_url()))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let received = body["Ok"]["transport_metrics"]["snapshot_received"]
                .as_u64()
                .unwrap_or(0);
            assert!(received >= 1, "follower did not receive a snapshot: {body}");

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

/// E12: an aborted snapshot stream must leave no `current.snap` and no staging
/// file, and a later retry must install and CRC-validate the full snapshot.
#[cfg(debug_assertions)]
#[test]
fn cancelled_snapshot_install_removes_staging_and_retry_succeeds() {
    MatchEngineTypeConfig::run(async {
        let temp = TempDir::new("snapshot-cancel");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("snapshot-cancel");
        let mut config = cursor_config();
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(3);
        // A small chunk size makes a modest snapshot multi-chunk and keeps the
        // test fast, while still exercising the real chunk/CRC staging path.
        config.snapshot_max_chunk_size = CANCEL_CHUNK_SIZE;

        let leader =
            build_match_engine_node(1, CANCEL_LEADER_ADDR.to_string(), config, storage.clone())
                .await
                .unwrap();
        let client =
            MatchEngineClient::<MatchEngineTypeConfig>::new(1, CANCEL_LEADER_ADDR.to_string());
        let leader_metrics_url = format!("{}/metrics", CANCEL_LEADER_ADDR.http_url());
        let started = Instant::now();

        let result = std::panic::AssertUnwindSafe(async {
            client.init().await.unwrap().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            wait_for_leader_id(&client, 1, deadline).await;
            println!(
                "snapshot-cancel: leader elected after {:?}",
                started.elapsed()
            );

            // 4 batches of 64 resting orders (256 events) make a snapshot well
            // over one 16 KiB chunk.
            for batch in 0..4u64 {
                let requests: Vec<MatchEngineRequest> = (0..64)
                    .map(|i| {
                        MatchEngineRequest::place(Order::new(
                            OrderSide::Buy,
                            100.0 - (batch * 64 + i) as f64 * 0.01,
                            1.0,
                            0,
                        ))
                    })
                    .collect();
                let response = client.write_batch(&requests).await.unwrap().unwrap();
                assert_eq!(64, response.data.batch.len());
            }
            loop {
                let metrics = client.metrics().await.unwrap();
                if let (Some(snapshot), Some(purged)) = (metrics.snapshot, metrics.purged)
                    && purged.index >= snapshot.index
                    && snapshot.index >= 4
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "leader did not snapshot/purge 4 batches"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }

            let leader_snapshot = storage.snapshot_dir(1).join("current.snap");
            let bytes = std::fs::metadata(&leader_snapshot).unwrap().len();
            assert!(
                bytes > CANCEL_CHUNK_SIZE,
                "snapshot of {bytes} bytes is not multi-chunk"
            );
            println!(
                "snapshot-cancel: leader snapshot is {bytes} bytes after {:?}",
                started.elapsed()
            );

            // The learner's server hook delays each received chunk and writes a
            // marker, so the test can observe that staging actually started.
            let marker = temp.path().join("snapshot-install-started");
            let learner_data = temp.path().to_path_buf();
            let learner_prefix = "snapshot-cancel";
            let mut child = SnapshotChildGuard(start_snapshot_child(
                2,
                &CANCEL_LEARNER_ADDR.to_string(),
                &learner_data,
                learner_prefix,
                &[
                    ("MATCHENGINE_TEST_SNAPSHOT_INSTALL_DELAY_MS", "300"),
                    (
                        "MATCHENGINE_TEST_SNAPSHOT_INSTALL_MARKER",
                        marker.to_str().unwrap(),
                    ),
                ],
            ));
            wait_for_health_snapshot(&format!("{}/health", CANCEL_LEARNER_ADDR.http_url())).await;

            let cancel = SnapshotCancelEnv::install(&CANCEL_LEARNER_ADDR.to_string());
            let adder =
                MatchEngineClient::<MatchEngineTypeConfig>::new(1, CANCEL_LEADER_ADDR.to_string());
            let learner_addr = CANCEL_LEARNER_ADDR.to_string();
            let add = tokio::spawn(async move { adder.add_learner((2, learner_addr)).await });

            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "learner never started the snapshot install"
                );
                if marker.exists() {
                    println!(
                        "snapshot-cancel: learner started install after {:?}",
                        started.elapsed()
                    );
                    break;
                }
                MatchEngineTypeConfig::sleep(Duration::from_millis(20)).await;
            }
            loop {
                let body: Value = http
                    .get(&leader_metrics_url)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let failures = body["Ok"]["transport_metrics"]["snapshot_send_failures"]
                    .as_u64()
                    .unwrap_or(0);
                if failures >= 1 {
                    println!(
                        "snapshot-cancel: observed {failures} failed snapshot send(s) after {:?}",
                        started.elapsed()
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled snapshot send not observed"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }
            add.abort();
            let _ = add.await;
            // Keep the cancel hook armed while checking the learner's disk:
            // every retry is cancelled too, so no snapshot can be installed
            // behind our back while we inspect the staging directory.

            let learner_storage =
                StorageConfig::new(temp.path()).with_instance_prefix(learner_prefix);
            let snap_dir = learner_storage.snapshot_dir(2);
            let cleanup_deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let leftovers: Vec<_> = std::fs::read_dir(&snap_dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("snapshot-staging-"))
                    })
                    .collect();
                if leftovers.is_empty() && !snap_dir.join("current.snap").exists() {
                    break;
                }
                assert!(
                    Instant::now() < cleanup_deadline,
                    "aborted install left current.snap or staging files: {leftovers:?}"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }

            // Freeze the learner's disk state, then clear the hook so the
            // restarted learner can install the snapshot on the next attempt.
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            drop(cancel);

            // Restart the learner without the delay and let replication retry.
            MatchEngineTypeConfig::sleep(Duration::from_millis(100)).await;
            let _restarted_child = SnapshotChildGuard(start_snapshot_child(
                2,
                &CANCEL_LEARNER_ADDR.to_string(),
                &learner_data,
                learner_prefix,
                &[],
            ));
            wait_for_health_snapshot(&format!("{}/health", CANCEL_LEARNER_ADDR.http_url())).await;
            // The learner is already in membership; re-issuing is idempotent and
            // nudges replication if the leader had backed off.
            let _ = client
                .add_learner((2, CANCEL_LEARNER_ADDR.to_string()))
                .await;

            let learner_client =
                MatchEngineClient::<MatchEngineTypeConfig>::new(2, CANCEL_LEARNER_ADDR.to_string());
            loop {
                if let Ok(value) = learner_client.read(&"orderbook_sequence".to_string()).await
                    && value == "256"
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "learner did not retry the snapshot install"
                );
                MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
            }

            // The installed file must pass the CRC-validating reader, and its
            // contents must match the leader's snapshot exactly.
            let (_, learner_body) = read_snapshot_file::<MatchEngineTypeConfig>(&snap_dir)
                .unwrap()
                .expect("the learner installed a snapshot");
            let (_, leader_body) =
                read_snapshot_file::<MatchEngineTypeConfig>(&storage.snapshot_dir(1))
                    .unwrap()
                    .expect("the leader has a snapshot");
            let learner_data: StateMachineData = postcard::from_bytes(&learner_body).unwrap();
            let leader_data: StateMachineData = postcard::from_bytes(&leader_body).unwrap();
            assert_eq!(leader_data.sequence, learner_data.sequence);
            assert_eq!(leader_data.orders, learner_data.orders);
            assert_eq!(leader_data.events.len(), learner_data.events.len());
            assert_eq!(256, learner_data.orders.len());
            assert_eq!(256, learner_data.events.len());

            let body: Value = http
                .get(format!("{}/metrics", CANCEL_LEARNER_ADDR.http_url()))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let received = body["Ok"]["transport_metrics"]["snapshot_received"]
                .as_u64()
                .unwrap_or(0);
            assert!(
                received >= 1,
                "learner did not record a snapshot install: {body}"
            );

            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        if let Err(error) = leader.shutdown().await {
            eprintln!("node shutdown failed: {error}");
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}

async fn wait_for_leader_id(
    client: &MatchEngineClient<MatchEngineTypeConfig>,
    id: u64,
    deadline: Instant,
) {
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(id)
        {
            return;
        }
        assert!(Instant::now() < deadline, "node {id} never became leader");
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_sequence_snapshot(
    client: &MatchEngineClient<MatchEngineTypeConfig>,
    expected: &str,
    deadline: Instant,
) {
    loop {
        if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
            && value == expected
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node did not reach sequence {expected}"
        );
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_health_snapshot(url: &str) {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = http.get(url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(Instant::now() < deadline, "learner HTTP API did not start");
        MatchEngineTypeConfig::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(debug_assertions)]
fn start_snapshot_child(
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
        .env("RAFT_WAL_FSYNC_MS", "0")
        .envs(envs.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start the matchengine-raft binary")
}

#[cfg(debug_assertions)]
struct SnapshotChildGuard(Child);

#[cfg(debug_assertions)]
impl Drop for SnapshotChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Restores the debug-only snapshot cancel variables on drop. Scope is the
/// learner's address, so no other test's snapshot stream is affected.
#[cfg(debug_assertions)]
struct SnapshotCancelEnv(Option<OsString>, Option<OsString>);

#[cfg(debug_assertions)]
impl SnapshotCancelEnv {
    const CHUNKS_ENV: &'static str = "MATCHENGINE_TEST_SNAPSHOT_CANCEL_AFTER_CHUNKS";
    const TARGET_ENV: &'static str = "MATCHENGINE_TEST_SNAPSHOT_CANCEL_TARGET";

    fn install(target: &str) -> Self {
        let chunks = std::env::var_os(Self::CHUNKS_ENV);
        let scope = std::env::var_os(Self::TARGET_ENV);
        // SAFETY: edition 2024 marks process-wide env mutation unsafe. Both
        // variables are restored on drop; the target scope keeps the hook off
        // every other node in the test process.
        unsafe {
            std::env::set_var(Self::CHUNKS_ENV, "2");
            std::env::set_var(Self::TARGET_ENV, target);
        }
        Self(chunks, scope)
    }
}

#[cfg(debug_assertions)]
impl Drop for SnapshotCancelEnv {
    fn drop(&mut self) {
        // SAFETY: see `install`.
        unsafe {
            match self.0.take() {
                Some(value) => std::env::set_var(Self::CHUNKS_ENV, value),
                None => std::env::remove_var(Self::CHUNKS_ENV),
            }
            match self.1.take() {
                Some(value) => std::env::set_var(Self::TARGET_ENV, value),
                None => std::env::remove_var(Self::TARGET_ENV),
            }
        }
    }
}
