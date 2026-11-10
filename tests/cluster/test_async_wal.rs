//! Raft path for the prototype async WAL.

#![cfg(feature = "async-wal")]

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
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR: TestAddr = TestAddr::new(21931);

#[test]
fn async_wal_node_commits_and_recovers() {
    C::run(async {
        let temp = TempDir::new("async-wal-node");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("async-wal-node")
            .with_wal_fsync(Some(Duration::ZERO))
            .with_async_wal(true)
            .with_min_voters(1);

        let node =
            build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage.clone())
                .await
                .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        for index in 0..10 {
            client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0 + index as f64,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        }
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(10, orders.len());
        node.shutdown().await.unwrap();

        // Reopen the same durable state with the async worker enabled again.
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        wait_for_leader(&client).await;
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(10, orders.len());
        node.shutdown().await.unwrap();
    });
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
