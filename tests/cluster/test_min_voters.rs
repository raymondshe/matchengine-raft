//! The no-fsync durability policy requires at least four voters.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::ClientConfig;
use matchengine_raft::client::ClientError;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::BasicNode;
use openraft::async_runtime::WatchReceiver;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21925";

#[test]
fn no_fsync_requires_four_voters() {
    C::run(async {
        let temp = TempDir::new("min-voters");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("min-voters")
            .with_snapshot_interval(Duration::ZERO)
            .with_min_voters(4);
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::with_config(
            1,
            ADDR.to_string(),
            ClientConfig {
                max_retries: 0,
                max_forward_retries: 0,
                ..ClientConfig::default()
            },
        );

        // An empty init creates a one-voter cluster, below the minimum.
        let error = client.init().await.unwrap_err();
        assert!(
            matches!(error, ClientError::Api { status: 400, .. }),
            "{error:?}"
        );

        // Bypass the HTTP init guard to create a one-voter cluster, then prove
        // that writes are refused until the voter count reaches four.
        let nodes = BTreeMap::from([(
            1,
            BasicNode {
                addr: ADDR.to_string(),
            },
        )]);
        node.raft.initialize(nodes).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let metrics = node.raft.metrics();
            if metrics.borrow_watched().current_leader == Some(1) {
                break;
            }
            if Instant::now() > deadline {
                panic!("node 1 never became leader");
            }
            C::sleep(Duration::from_millis(50)).await;
        }

        let result = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            )))
            .await;
        assert!(
            matches!(result, Err(ClientError::Api { status: 503, .. })),
            "{result:?}"
        );

        let error = client
            .change_membership(&BTreeSet::from([1]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ClientError::Api { status: 400, .. }),
            "{error:?}"
        );

        node.shutdown().await.unwrap();
    });
}
