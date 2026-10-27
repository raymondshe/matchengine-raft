//! Full-node restart recovery tests.
//!
//! Each test boots a single-node cluster on its own port, writes data, stops the
//! node through [`MatchEngineNode::shutdown`], then builds a new node on the same
//! storage directory and verifies that the KV state, the order book and the
//! sequence counter came back.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::matchengine::Order;
use matchengine_raft::matchengine::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;

type C = MatchEngineTypeConfig;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "matchengine-restart-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config(snapshot_policy: SnapshotPolicy) -> Config {
    Config {
        heartbeat_interval: 150,
        election_timeout_min: 400,
        election_timeout_max: 800,
        snapshot_policy,
        max_in_snapshot_log_to_keep: 0,
        ..default_raft_config()
    }
}

async fn wait_for_leader(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("node 1 never became leader");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn start(addr: &str, storage: &StorageConfig, config: Config) -> anyhow::Result<MatchEngineNode> {
    let node = build_match_engine_node(1, addr.to_string(), Transport::Grpc, config, storage.clone()).await?;
    Ok(node)
}

fn make_client(addr: &str) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(1, addr.to_string())
}

async fn write_kv(client: &MatchEngineClient<C>, key: &str, value: &str) -> anyhow::Result<()> {
    client
        .write(&MatchEngineRequest::Set {
            key: key.to_string(),
            value: value.to_string(),
        })
        .await??;
    Ok(())
}

/// Writes three non-crossing orders so the book keeps all of them and the
/// sequence counter advances deterministically.
async fn place_orders(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
    let orders = [
        Order::new(OrderSide::Buy, 10.0, 2.0, 0),
        Order::new(OrderSide::Buy, 9.0, 3.0, 0),
        Order::new(OrderSide::Sell, 20.0, 4.0, 0),
    ];

    for order in orders {
        let response = client.write(&MatchEngineRequest::Place { order }).await??;
        assert!(response.data.value.is_some());
    }
    Ok(())
}

async fn assert_application_state(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
    for (key, value) in [("alpha", "one"), ("beta", "two"), ("gamma", "three")] {
        let got = client.read(&key.to_string()).await?;
        assert_eq!(value, got, "key {key} lost across restart");
    }

    let orders_json = client.read(&"orderbook_orders".to_string()).await?;
    let orders: Vec<Order> = serde_json::from_str(&orders_json)?;
    assert_eq!(3, orders.len(), "order book lost orders across restart: {orders_json}");

    let sequence = client.read(&"orderbook_sequence".to_string()).await?;
    assert_eq!("3", sequence, "sequence counter not recovered");
    Ok(())
}

/// Log-replay recovery: no snapshot is taken, so the restarted node must rebuild
/// its state by replaying the durable WAL.
#[test]
fn restart_recovers_from_log_replay() {
    C::run(async {
        let addr = "127.0.0.1:21201";
        let temp = TempDir::new("replay");
        let storage = StorageConfig::new(temp.0.clone()).with_instance_prefix("replay");

        // A snapshot policy high enough that this test never creates one.
        let config = test_config(SnapshotPolicy::LogsSinceLast(1_000_000));

        let node = start(addr, &storage, config.clone()).await.unwrap();
        let client = make_client(addr);
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await.unwrap();

        write_kv(&client, "alpha", "one").await.unwrap();
        write_kv(&client, "beta", "two").await.unwrap();
        write_kv(&client, "gamma", "three").await.unwrap();
        place_orders(&client).await.unwrap();
        assert_application_state(&client).await.unwrap();

        node.shutdown().await.unwrap();

        // Nothing should have been snapshotted, so recovery is pure WAL replay.
        assert!(
            !storage.snapshot_dir(1).join("current.snap").exists(),
            "test must not rely on a snapshot"
        );

        let node = start(addr, &storage, config).await.unwrap();
        let client = make_client(addr);
        wait_for_leader(&client).await.unwrap();
        assert_application_state(&client).await.unwrap();

        // The recovered node is still writable.
        write_kv(&client, "delta", "four").await.unwrap();
        assert_eq!("four", client.read(&"delta".to_string()).await.unwrap());

        node.shutdown().await.unwrap();
    });
}

/// Snapshot recovery: the log covered by the snapshot is purged, so the restarted
/// node must boot from `current.snap` and only replay the tail.
#[test]
fn restart_recovers_from_snapshot() {
    C::run(async {
        let addr = "127.0.0.1:21202";
        let temp = TempDir::new("snapshot");
        let storage = StorageConfig::new(temp.0.clone()).with_instance_prefix("snapshot");

        let config = test_config(SnapshotPolicy::LogsSinceLast(5));

        let node = start(addr, &storage, config.clone()).await.unwrap();
        let client = make_client(addr);
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await.unwrap();

        for i in 0..30 {
            write_kv(&client, &format!("k{i}"), &format!("v{i}")).await.unwrap();
        }
        write_kv(&client, "alpha", "one").await.unwrap();
        write_kv(&client, "beta", "two").await.unwrap();
        write_kv(&client, "gamma", "three").await.unwrap();
        place_orders(&client).await.unwrap();
        assert_application_state(&client).await.unwrap();

        // Wait until a snapshot exists and the log it covers has been purged.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let metrics = client.metrics().await.unwrap();
            let snapshot = metrics.snapshot.map(|log_id| log_id.index);
            let purged = metrics.purged.map(|log_id| log_id.index);

            if let (Some(snapshot), Some(purged)) = (snapshot, purged)
                && purged >= snapshot
            {
                break;
            }
            if Instant::now() > deadline {
                panic!("snapshot/purge did not happen: snapshot={snapshot:?}, purged={purged:?}");
            }
            C::sleep(Duration::from_millis(100)).await;
        }

        let snapshot_file = storage.snapshot_dir(1).join("current.snap");
        assert!(snapshot_file.exists(), "snapshot file was not persisted");
        let snapshot_size = std::fs::metadata(&snapshot_file).unwrap().len();
        assert!(snapshot_size > 0);

        node.shutdown().await.unwrap();

        let node = start(addr, &storage, config).await.unwrap();
        let client = make_client(addr);
        wait_for_leader(&client).await.unwrap();

        // The snapshot carried the KV tail plus the order book.
        for i in 0..30 {
            assert_eq!(format!("v{i}"), client.read(&format!("k{i}")).await.unwrap());
        }
        assert_application_state(&client).await.unwrap();

        node.shutdown().await.unwrap();
    });
}
