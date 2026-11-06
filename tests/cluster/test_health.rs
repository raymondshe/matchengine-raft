//! Health and readiness reporting.
//!
//! `/health` is liveness (listener, Raft core and storage are usable) and
//! `/ready` additionally requires a known cluster leader. Both expose the
//! durable WAL and state-machine frontier for an internal orchestrator.

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
use reqwest::StatusCode;
use serde_json::Value;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21905";
const RAFT_ADDR: &str = "127.0.0.1:22905";

#[test]
fn health_reports_grpc_leadership_and_storage() {
    C::run(async {
        let temp = TempDir::new("health");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("health");
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();

        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{ADDR}");

        // Before initialization there is no leader: liveness is OK, readiness is not.
        let (status, body) = get_json(&http, &format!("{base}/health")).await;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["healthy"], true);
        assert_eq!(body["ready"], false);
        assert_eq!(body["node_id"], 1);
        assert_eq!(body["http_addr"], ADDR);
        assert_eq!(body["grpc_addr"], RAFT_ADDR);
        assert_eq!(body["grpc_listening"], true);
        assert_eq!(body["recovered"], false);
        assert!(body["storage"]["wal"].is_object());
        assert!(body["storage"]["state_machine"].is_object());
        assert!(body["leadership"]["leader_id"].is_null());

        let (status, body) = get_json(&http, &format!("{base}/ready")).await;
        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, status);
        assert_eq!(body["ready"], false);

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;
        wait_for_ready(&http, &base).await;

        let (status, body) = get_json(&http, &format!("{base}/ready")).await;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["ready"], true);
        assert_eq!(body["recovered"], true);
        assert_eq!(body["leadership"]["state"], "Leader");
        assert_eq!(body["leadership"]["is_leader"], true);
        assert_eq!(body["leadership"]["leader_id"], 1);

        // A write advances both the WAL tail and the applied state.
        let request = MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0));
        let response = http
            .post(format!("{base}/write"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::OK, response.status());

        let (status, body) = get_json(&http, &format!("{base}/health")).await;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(body["healthy"], true);
        assert!(
            body["storage"]["wal"]["last_log_id"]["index"]
                .as_u64()
                .unwrap()
                >= 1
        );
        assert!(
            body["storage"]["state_machine"]["last_applied"]["index"]
                .as_u64()
                .unwrap()
                >= 1
        );

        node.shutdown().await.unwrap();
    });
}

async fn get_json(http: &reqwest::Client, url: &str) -> (StatusCode, Value) {
    let response = http.get(url).send().await.unwrap();
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
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

async fn wait_for_ready(http: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, _) = get_json(http, &format!("{base}/ready")).await;
        if status == StatusCode::OK {
            return;
        }
        assert!(Instant::now() <= deadline, "node 1 never became ready");
        C::sleep(Duration::from_millis(50)).await;
    }
}
