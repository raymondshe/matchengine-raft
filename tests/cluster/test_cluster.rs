use std::backtrace::Backtrace;
use std::collections::BTreeMap;
use std::panic::PanicHookInfo;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use maplit::btreemap;
use maplit::btreeset;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::BasicNode;
use openraft::type_config::TypeConfigExt;
use tracing_subscriber::EnvFilter;

pub fn log_panic(panic: &PanicHookInfo) {
    let backtrace = { format!("{:?}", Backtrace::force_capture()) };

    eprintln!("{}", panic);

    if let Some(location) = panic.location() {
        tracing::error!(
            message = %panic,
            backtrace = %backtrace,
            panic.file = location.file(),
            panic.line = location.line(),
            panic.column = location.column(),
        );
        eprintln!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    } else {
        tracing::error!(message = %panic, backtrace = %backtrace);
    }

    eprintln!("{}", backtrace);
}

/// Setup a cluster of 3 nodes.
/// Write to it and read from it.
#[test]
fn test_cluster() {
    MatchEngineTypeConfig::run(test_cluster_inner()).unwrap();
}

async fn test_cluster_inner() -> anyhow::Result<()> {
    // --- The client itself does not store addresses for all nodes, but just node id.
    //     Thus we need a supporting component to provide mapping from node id to node address.
    //     This is only used by the client. A raft node stores node addresses in its
    // store.

    std::panic::set_hook(Box::new(|panic| {
        log_panic(panic);
    }));

    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_level(true)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let get_addr = |node_id| {
        let addr = match node_id {
            1 => "127.0.0.1:21001".to_string(),
            2 => "127.0.0.1:21002".to_string(),
            3 => "127.0.0.1:21003".to_string(),
            _ => {
                return Err(anyhow::anyhow!("node {} not found", node_id));
            }
        };
        Ok(addr)
    };

    // --- Start 3 raft nodes in this test's runtime.
    //     `MATCHENGINE_TEST_TRANSPORT=http` runs the same suite over the legacy
    //     HTTP/JSON transport; the default is gRPC.

    let transport = match std::env::var("MATCHENGINE_TEST_TRANSPORT").as_deref() {
        Ok("http") => Transport::Http,
        _ => Transport::Grpc,
    };

    // A fresh data dir per run: persisted membership would make `init` fail on
    // the second run. Each node gets its own subdirectory below it.
    let temp = crate::common::TempDir::new("cluster");
    let storage = StorageConfig::new(temp.path()).with_instance_prefix("cluster");

    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;

    let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
    for id in 1..=3u64 {
        let addr = get_addr(id)?;
        nodes.push(Some(
            build_match_engine_node(id, addr, transport, config.clone(), storage.clone()).await?,
        ));
    }

    // Wait for server to start up.
    MatchEngineTypeConfig::sleep(Duration::from_millis(200)).await;

    let result = std::panic::AssertUnwindSafe(async {
        // --- Create a client to the first node, as a control handle to the cluster.

        let client = MatchEngineClient::<MatchEngineTypeConfig>::new(1, get_addr(1)?);

        // --- 1. Initialize the target node as a cluster of only one node.
        //        After init(), the single node cluster will be fully functional.

        println!("=== init single node cluster");
        client.init().await??;

        // Wait for the initial membership log to commit after election
        MatchEngineTypeConfig::sleep(Duration::from_millis(200)).await;

        println!("=== metrics after init");
        client.metrics().await?;

        // --- 2. Add node 2 and 3 to the cluster as `Learner`, to let them start to receive log replication
        // from the        leader.

        println!("=== add-learner 2");
        client.add_learner((2, get_addr(2)?)).await??;

        println!("=== add-learner 3");
        client.add_learner((3, get_addr(3)?)).await??;

        println!("=== metrics after add-learner");
        let x = client.metrics().await?;

        assert_eq!(
            &vec![btreeset![1]],
            x.membership_config.membership().get_joint_config()
        );

        let nodes_in_cluster = x
            .membership_config
            .nodes()
            .map(|(nid, node)| (*nid, node.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            btreemap! {
                1 => BasicNode::new("127.0.0.1:21001"),
                2 => BasicNode::new("127.0.0.1:21002"),
                3 => BasicNode::new("127.0.0.1:21003"),
            },
            nodes_in_cluster
        );

        // --- 3. Turn the two learners to members. A member node can vote or elect itself as leader.

        println!("=== change-membership to 1,2,3");
        client.change_membership(&btreeset! {1,2,3}).await??;

        // --- After change-membership, some cluster state will be seen in the metrics.

        println!("=== metrics after change-member");
        let x = client.metrics().await?;
        assert_eq!(
            &vec![btreeset![1, 2, 3]],
            x.membership_config.membership().get_joint_config()
        );

        // --- Try to write some application data through the leader.

        println!("=== write a resting buy order");
        let response = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                10.0,
                0,
            )))
            .await??;
        assert_eq!(Some("0".to_string()), response.data.value);

        // --- Wait for a while to let the replication get done.

        MatchEngineTypeConfig::sleep(Duration::from_millis(1_000)).await;

        // --- Read it on every node.

        println!("=== read `orderbook_sequence` on node 1");
        assert_eq!("1", client.read(&"orderbook_sequence".to_string()).await?);

        println!("=== read `orderbook_sequence` on node 2");
        let client2 = MatchEngineClient::<MatchEngineTypeConfig>::new(2, get_addr(2)?);
        assert_eq!("1", client2.read(&"orderbook_sequence".to_string()).await?);

        println!("=== read `orderbook_sequence` on node 3");
        let client3 = MatchEngineClient::<MatchEngineTypeConfig>::new(3, get_addr(3)?);
        assert_eq!("1", client3.read(&"orderbook_sequence".to_string()).await?);

        // --- A write to non-leader will be automatically forwarded to a known leader.
        //     The sell crosses the resting bid, so it also proves matching replicated.

        println!("=== write a crossing sell to node 2");
        let response = client2
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Sell,
                100.0,
                4.0,
                0,
            )))
            .await??;
        assert_eq!(Some("1".to_string()), response.data.value);

        MatchEngineTypeConfig::sleep(Duration::from_millis(1_000)).await;

        // --- The book holds the 6 that did not trade, on every node.

        for (name, node_client) in [
            ("node 1", &client),
            ("node 2", &client2),
            ("node 3", &client3),
        ] {
            println!("=== read `orderbook_orders` on {name}");
            let orders: Vec<Order> =
                serde_json::from_str(&node_client.read(&"orderbook_orders".to_string()).await?)?;
            assert_eq!(1, orders.len(), "unexpected book on {name}: {orders:?}");
            assert_eq!(6.0, orders[0].volume);
        }

        println!("=== linearizable_read `orderbook_sequence` on node 1");
        assert_eq!(
            "2",
            client
                .linearizable_read(&"orderbook_sequence".to_string())
                .await??
        );

        // A fresh client bound to node 2: the previous one learned the leader while forwarding,
        // and this assertion is about what the follower itself answers.
        let client2 = MatchEngineClient::<MatchEngineTypeConfig>::new(2, get_addr(2)?);

        println!(
            "=== linearizable_read `orderbook_sequence` on node 2 MUST return LinearizableReadError"
        );
        let x = client2
            .linearizable_read(&"orderbook_sequence".to_string())
            .await?;
        match x {
            Err(e) => {
                let s = e.to_string();
                let expect_err: String =
                "has to forward request to: Some(1), Some(BasicNode { addr: \"127.0.0.1:21001\" })"
                    .to_string();

                assert_eq!(s, expect_err);
            }
            Ok(_) => panic!("MUST return LinearizableReadError"),
        }

        // --- Remove node 1,2 from the cluster.

        println!("=== change-membership to 3, ");
        client.change_membership(&btreeset! {3}).await??;

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let metrics = client3.metrics().await?;
            if metrics.membership_config.membership().get_joint_config() == &vec![btreeset![3]] {
                break;
            }
            if Instant::now() > deadline {
                anyhow::bail!(
                    "membership did not shrink to {{3}}: {:?}",
                    metrics.membership_config.membership().get_joint_config()
                );
            }
            MatchEngineTypeConfig::sleep(Duration::from_millis(100)).await;
        }

        println!("=== metrics after change-membership to {{3}}");
        let x = client3.metrics().await?;
        assert_eq!(
            &vec![btreeset![3]],
            x.membership_config.membership().get_joint_config()
        );

        // The shrunk single-node cluster still has to elect node 3 before it
        // can accept a write.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let metrics = client3.metrics().await?;
            if metrics.current_leader == Some(3) {
                break;
            }
            if Instant::now() > deadline {
                anyhow::bail!("node 3 did not become leader after the shrink");
            }
            MatchEngineTypeConfig::sleep(Duration::from_millis(100)).await;
        }

        println!("=== write a resting buy to node-3");
        client3
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                99.0,
                1.0,
                0,
            )))
            .await??;

        println!("=== read `orderbook_orders` on node-3");
        let orders: Vec<Order> =
            serde_json::from_str(&client3.read(&"orderbook_orders".to_string()).await?)?;
        assert_eq!(
            2,
            orders.len(),
            "the shrunk cluster lost an order: {orders:?}"
        );
        Ok(())
    })
    .catch_unwind()
    .await;

    // The test owns every node: stop HTTP, gRPC, market data and Raft even when
    // an assertion panics, so the fixed ports are released for the next run.
    for node in nodes.into_iter().flatten() {
        if let Err(error) = node.shutdown().await {
            eprintln!("node shutdown failed: {error}");
        }
    }
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
