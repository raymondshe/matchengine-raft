//! Operational-console integration coverage.
//!
//! Starts a three-node cluster, inspects the stable console API, and transfers
//! leadership through the console action.

use std::collections::BTreeSet;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;
use serde_json::Value;
use serde_json::json;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDRS: [TestAddr; 3] = [
    TestAddr::new(21906),
    TestAddr::new(21907),
    TestAddr::new(21908),
];

#[test]
fn console_reports_cluster_and_transfers_leader() {
    C::run(async {
        let temp = TempDir::new("console");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("console");
        let mut config = default_raft_config();
        config.heartbeat_interval = 200;
        config.election_timeout_min = 500;
        config.heartbeat_min_interval = None;
        config.election_timeout_max = 1_000;

        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (index, addr) in ADDRS.iter().enumerate() {
            nodes.push(Some(
                build_match_engine_node(
                    index as u64 + 1,
                    addr.to_string(),
                    config.clone(),
                    storage.clone(),
                )
                .await
                .unwrap(),
            ));
        }
        C::sleep(Duration::from_millis(200)).await;

        let client = MatchEngineClient::<C>::new(1, ADDRS[0].to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client, 1).await;

        client
            .add_learner((2, ADDRS[1].to_string()))
            .await
            .unwrap()
            .unwrap();
        client
            .add_learner((3, ADDRS[2].to_string()))
            .await
            .unwrap()
            .unwrap();
        client
            .change_membership(&BTreeSet::from([1, 2, 3]))
            .await
            .unwrap()
            .unwrap();
        wait_for_leader(&client, 1).await;
        wait_for_replication(&client, &[2, 3]).await;

        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{}", ADDRS[0]);

        let response = http.get(format!("{base}/console/")).send().await.unwrap();
        assert_eq!(StatusCode::OK, response.status());
        assert!(response.headers().get("content-security-policy").is_some());
        let html = response.text().await.unwrap();
        assert!(html.contains("Operational console"));
        assert!(html.contains("/console/app.js"));

        let (status, node) = get_json(&http, &format!("{base}/console/api/node")).await;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(node["node_id"], 1);
        assert_eq!(node["schema_version"], 1);
        assert!(node["raft"]["voters"].as_array().unwrap().len() >= 3);
        assert!(node["storage"]["wal"].is_object());

        let (status, cluster) = get_json(&http, &format!("{base}/console/api/cluster")).await;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(cluster["leader_id"], 1);
        assert_eq!(cluster["nodes"].as_array().unwrap().len(), 3);
        assert!(
            cluster["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|member| member["reachable"] == true)
        );

        // State-changing requests need the custom header.
        let response = http
            .post(format!("{base}/console/api/actions/transfer-leader"))
            .json(&json!({ "target": 2 }))
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        // Self and learner/unknown targets are rejected before Raft is touched.
        let response = post_action(&http, &base, "transfer-leader", json!({ "target": 1 })).await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let response = post_action(&http, &base, "transfer-leader", json!({ "target": 99 })).await;
        assert_eq!(StatusCode::CONFLICT, response.status());

        let response = post_action(&http, &base, "transfer-leader", json!({ "target": 2 })).await;
        assert_eq!(StatusCode::ACCEPTED, response.status());
        wait_for_leader(&client, 2).await;

        // The old leader is no longer allowed to transfer.
        let response = post_action(&http, &base, "transfer-leader", json!({ "target": 3 })).await;
        assert_eq!(StatusCode::CONFLICT, response.status());

        // Writes still work after the transfer.
        let leader = MatchEngineClient::<C>::new(2, ADDRS[1].to_string());
        leader
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            "1",
            leader
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        let (status, actions) = get_json(&http, &format!("{base}/console/api/actions")).await;
        assert_eq!(StatusCode::OK, status);
        assert!(
            actions
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["action"] == "transfer-leader")
        );

        // Stop the leader first so its replication streams close before the
        // followers' gRPC servers begin graceful shutdown.
        nodes[1].take().unwrap().shutdown().await.unwrap();
        nodes[0].take().unwrap().shutdown().await.unwrap();
        nodes[2].take().unwrap().shutdown().await.unwrap();
    });
}

async fn get_json(http: &reqwest::Client, url: &str) -> (StatusCode, Value) {
    let response = http.get(url).send().await.unwrap();
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

async fn post_action(
    http: &reqwest::Client,
    base: &str,
    action: &str,
    body: Value,
) -> reqwest::Response {
    http.post(format!("{base}/console/api/actions/{action}"))
        .header("X-Console-Action", "1")
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn wait_for_leader(client: &MatchEngineClient<C>, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(expected)
        {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "node {expected} never became leader"
        );
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_replication(client: &MatchEngineClient<C>, targets: &[u64]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await {
            let last = metrics.last_log_index.unwrap_or(0);
            let caught_up = metrics.replication.as_ref().is_some_and(|replication| {
                targets.iter().all(|target| {
                    replication
                        .get(target)
                        .and_then(|matched| matched.as_ref())
                        .is_some_and(|matched| matched.index >= last)
                })
            });
            if caught_up {
                return;
            }
        }
        assert!(
            Instant::now() <= deadline,
            "replication targets {targets:?} did not catch up"
        );
        C::sleep(Duration::from_millis(100)).await;
    }
}
