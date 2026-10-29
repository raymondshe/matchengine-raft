//! Hostile-input and request-size limits for the public HTTP API.
//!
//! Oversized bodies, unbounded idempotency keys, oversized read keys and
//! out-of-range event page sizes must fail at the edge, before Raft proposes
//! anything. Snapshot caps are covered by the transport unit tests.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::MAX_CLIENT_ORDER_ID_BYTES;
use matchengine_raft::network::api::MAX_EVENTS_PAGE;
use matchengine_raft::network::api::MAX_READ_KEY_BYTES;
use matchengine_raft::network::api::MAX_REQUEST_BODY_BYTES;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21903";

#[test]
fn hostile_requests_are_rejected_before_raft() {
    C::run(async {
        let temp = TempDir::new("request-limits");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("request-limits");
        let node = build_match_engine_node(
            1,
            ADDR.to_string(),
            Transport::Grpc,
            default_raft_config(),
            storage,
        )
        .await
        .unwrap();

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{ADDR}");

        // The longest accepted key works and places exactly one order.
        let max_id = "a".repeat(MAX_CLIENT_ORDER_ID_BYTES);
        let valid = MatchEngineRequest::place_with_client_id(
            Order::new(OrderSide::Buy, 100.0, 1.0, 0),
            max_id,
        );
        let response = http
            .post(format!("{base}/write"))
            .json(&valid)
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            "1",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // Empty, oversized and non-printable keys are edge rejections.
        for id in [
            String::new(),
            "a".repeat(MAX_CLIENT_ORDER_ID_BYTES + 1),
            "has space".to_string(),
            "slash/not-allowed".to_string(),
            "café".to_string(),
        ] {
            let request = MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                id,
            );
            let response = http
                .post(format!("{base}/write"))
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(StatusCode::BAD_REQUEST, response.status());
        }

        // Malformed JSON is rejected by the extractor.
        let response = http
            .post(format!("{base}/write"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{not-json")
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        // Oversized request bodies are rejected before deserialization.
        let response = http
            .post(format!("{base}/write"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(vec![b'a'; MAX_REQUEST_BODY_BYTES + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::PAYLOAD_TOO_LARGE, response.status());

        // Read keys and event page sizes are explicitly bounded.
        let response = http
            .post(format!("{base}/read"))
            .json(&"x".repeat(MAX_READ_KEY_BYTES + 1))
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let response = http
            .post(format!("{base}/events"))
            .json(&serde_json::json!({ "after": null, "limit": MAX_EVENTS_PAGE + 1 }))
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let response = http
            .post(format!("{base}/events"))
            .json(&serde_json::json!({ "after": null, "limit": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        // None of the rejected requests reached Raft.
        assert_eq!(
            "1",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.as_array().unwrap().len());

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
