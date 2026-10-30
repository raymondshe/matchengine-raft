//! Batch-write API: one Raft entry for multiple requests.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::MAX_BATCH_REQUESTS;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21904";

#[test]
fn write_batch_applies_multiple_requests_in_one_entry() {
    C::run(async {
        let temp = TempDir::new("batch-write");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("batch-write");
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let requests = vec![
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                "batch-0",
            ),
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 99.0, 2.0, 0),
                "batch-1",
            ),
        ];
        let response = client.write_batch(&requests).await.unwrap().unwrap();
        let batch = response.data.batch;
        assert_eq!(2, batch.len());
        assert_eq!(Some(0), batch[0].sequence);
        assert_eq!(Some(1), batch[1].sequence);
        assert_eq!(0, batch[0].events[0].event_id.ordinal);
        assert_eq!(1, batch[1].events[0].event_id.ordinal);
        assert_eq!(
            batch[0].events[0].event_id.log_index,
            batch[1].events[0].event_id.log_index
        );

        assert_eq!(
            "2",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // Retrying one batch item returns only that item's sequence and events,
        // not the events of the other sub-request sharing the log index.
        let retry = client
            .write(&MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Sell, 50.0, 1.0, 0),
                "batch-1",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(1), retry.data.sequence);
        assert_eq!(1, retry.data.events.len());
        assert_eq!(1, retry.data.events[0].event_id.ordinal);
        assert_eq!(
            "2",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        let page = client
            .events(&matchengine_raft::network::api::EventsRequest {
                after: None,
                limit: Some(10),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(2, page.events.len());

        // Invalid batches are rejected before Raft.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .post(format!("http://{ADDR}/write_batch"))
            .json(&Vec::<MatchEngineRequest>::new())
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let duplicate_ids = vec![
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 98.0, 1.0, 0),
                "duplicate",
            ),
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 97.0, 1.0, 0),
                "duplicate",
            ),
        ];
        let response = http
            .post(format!("http://{ADDR}/write_batch"))
            .json(&duplicate_ids)
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let nested = vec![MatchEngineRequest::Batch {
            requests: vec![requests[0].clone()],
        }];
        let response = http
            .post(format!("http://{ADDR}/write_batch"))
            .json(&nested)
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let oversized = vec![requests[0].clone(); MAX_BATCH_REQUESTS + 1];
        let response = http
            .post(format!("http://{ADDR}/write_batch"))
            .json(&oversized)
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

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
