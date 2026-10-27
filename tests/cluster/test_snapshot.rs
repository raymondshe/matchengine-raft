use std::thread;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::matchengine::Order;
use matchengine_raft::matchengine::OrderSide;
use matchengine_raft::start_match_engine_node_with_storage;
use matchengine_raft::store::MatchEngineRequest;
use openraft::SnapshotPolicy;
use openraft::async_runtime::AsyncRuntime;
use openraft::type_config::TypeConfigExt;
use openraft::type_config::alias::AsyncRuntimeOf;

/// A learner that joins after the leader purged its log must be caught up with a
/// streamed snapshot over gRPC.
#[test]
fn test_grpc_snapshot_install() {
    MatchEngineTypeConfig::run(test_grpc_snapshot_install_inner()).unwrap();
}

async fn test_grpc_snapshot_install_inner() -> anyhow::Result<()> {
    let addr1 = "127.0.0.1:21101".to_string();
    let addr2 = "127.0.0.1:21102".to_string();

    let config = || {
        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.election_timeout_max = 1_000;
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
        // Purge every log already covered by a snapshot, so a node that joins
        // later cannot be caught up by log replication alone.
        config.max_in_snapshot_log_to_keep = 0;
        config
    };

    let temp = crate::common::TempDir::new("snapshot");
    let storage = StorageConfig::new(temp.path()).with_instance_prefix("snapshot");

    let _h1 = {
        let storage = storage.clone();
        thread::spawn(move || {
            let mut rt = AsyncRuntimeOf::<MatchEngineTypeConfig>::new(1);
            let x = rt.block_on(start_match_engine_node_with_storage(
                1,
                "127.0.0.1:21101".to_string(),
                Transport::Grpc,
                config(),
                storage,
            ));
            println!("node1: {:?}", x);
        })
    };

    let _h2 = {
        let storage = storage.clone();
        thread::spawn(move || {
            let mut rt = AsyncRuntimeOf::<MatchEngineTypeConfig>::new(1);
            let x = rt.block_on(start_match_engine_node_with_storage(
                2,
                "127.0.0.1:21102".to_string(),
                Transport::Grpc,
                config(),
                storage,
            ));
            println!("node2: {:?}", x);
        })
    };

    MatchEngineTypeConfig::sleep(Duration::from_millis(300)).await;

    let client1 = MatchEngineClient::<MatchEngineTypeConfig>::new(1, addr1);
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
            .write(&MatchEngineRequest::Place {
                order: Order::new(OrderSide::Buy, 100.0 - f64::from(i), 1.0, 0),
            })
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
    client1.add_learner((2, addr2)).await??;

    let client2 = MatchEngineClient::<MatchEngineTypeConfig>::new(2, "127.0.0.1:21102".to_string());
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
    let orders: Vec<Order> = serde_json::from_str(&client2.read(&"orderbook_orders".to_string()).await?)?;
    assert_eq!(30, orders.len());

    Ok(())
}
