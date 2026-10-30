//! End-to-end tests for the replicated match-event log.
//!
//! A single node writes orders with client idempotency keys and reads the
//! events back through `/events`: the same events appear in the `/write`
//! response, retries do not place a second order, and the cursor pages through
//! the retained window.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21304";

#[test]
fn events_are_returned_deduped_paged_and_bounded() {
    C::run(async {
        let temp = TempDir::new("events");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("events")
            .with_max_events(8);
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        // The write response is read-your-writes and carries the first events.
        let placed = client
            .write(&MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 10.0, 0),
                "client-1",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(0), placed.data.sequence);
        assert_eq!(Some("0".to_string()), placed.data.value);
        assert_eq!(1, placed.data.events.len());
        assert_eq!(MatchEventKind::Rested, placed.data.events[0].kind);
        assert!(!placed.data.events_truncated);

        let page = client
            .events(&EventsRequest {
                after: None,
                limit: Some(10),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(placed.data.events, page.events);
        assert!(!page.has_more);
        assert_eq!(page.events[0].event_id, page.oldest_event_id.unwrap());
        assert_eq!(page.events[0].event_id, page.last_event_id.unwrap());
        let cursor = page.next_after.expect("first cursor");

        // A retry with the same key returns the original sequence and events,
        // and does not place the different order it carries.
        let duplicate = client
            .write(&MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Sell, 50.0, 1.0, 0),
                "client-1",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(0), duplicate.data.sequence);
        assert_eq!(placed.data.events, duplicate.data.events);
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .expect("orders json");
        assert_eq!(1, orders.len(), "the duplicate did not place");
        assert_eq!(
            "1",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // A real second order appends events; the cursor isolates them.
        let filled = client
            .write(&MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Sell, 100.0, 4.0, 0),
                "client-2",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(1), filled.data.sequence);
        assert!(
            filled
                .data
                .events
                .iter()
                .any(|event| event.kind == MatchEventKind::Fill)
        );

        let page = client
            .events(&EventsRequest {
                after: Some(cursor),
                limit: Some(10),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(1, page.events.len());
        assert_eq!(MatchEventKind::Fill, page.events[0].kind);
        assert!(!page.has_more);

        // One-by-one paging sees both events exactly once.
        let mut after = None;
        let mut seen = Vec::new();
        loop {
            let page = client
                .events(&EventsRequest {
                    after,
                    limit: Some(1),
                })
                .await
                .unwrap()
                .unwrap();
            seen.extend(page.events.iter().map(|event| event.event_id));
            if !page.has_more {
                break;
            }
            after = page.next_after;
        }
        assert_eq!(2, seen.len());

        // Retention is bounded by --max-events; old events are reported as evicted.
        for i in 0..10u64 {
            client
                .write(&MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 90.0 - i as f64, 1.0, 0),
                    format!("retention-{i}"),
                ))
                .await
                .unwrap()
                .unwrap();
        }
        let status = client.events_status().await.unwrap();
        assert_eq!(8, status.retention_limit);
        assert_eq!(8, status.retained_events);
        assert_eq!(262_144, status.submission_retention_limit);
        assert!(status.evictions_total >= 4);
        assert!(status.oldest_event_id.unwrap() > page.oldest_event_id.unwrap());

        node.shutdown().await.unwrap();
    });
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let metrics = client.metrics().await.unwrap();
        if metrics.current_leader == Some(1) {
            return;
        }
        assert!(Instant::now() <= deadline, "cluster did not elect a leader");
        C::sleep(Duration::from_millis(50)).await;
    }
}
