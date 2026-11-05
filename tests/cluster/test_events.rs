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
                wait_ms: None,
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
                wait_ms: None,
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
                    wait_ms: None,
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

/// Boots a single node on `addr`, initializes it and waits for its leadership.
///
/// The returned `TempDir` must stay alive for the node's lifetime.
async fn boot_node(
    addr: &str,
    prefix: &str,
) -> (
    matchengine_raft::MatchEngineNode,
    MatchEngineClient<C>,
    TempDir,
) {
    let temp = TempDir::new(prefix);
    let storage = StorageConfig::new(temp.path())
        .with_instance_prefix(prefix)
        .with_max_events(64);
    let node = build_match_engine_node(1, addr.to_string(), default_raft_config(), storage)
        .await
        .unwrap();
    let client = MatchEngineClient::<C>::new(1, addr.to_string());
    client.init().await.unwrap().unwrap();
    wait_for_leader(&client).await;
    (node, client, temp)
}

/// Places one resting order and returns the cursor at the current tail.
async fn seed_and_cursor(client: &MatchEngineClient<C>) -> matchengine_raft::match_result::EventId {
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            100.0,
            10.0,
            0,
        )))
        .await
        .unwrap()
        .unwrap();
    let page = client
        .events(&EventsRequest {
            after: None,
            limit: Some(10),
            wait_ms: None,
        })
        .await
        .unwrap()
        .unwrap();
    page.next_after.expect("tail cursor")
}

#[test]
fn events_long_poll_returns_new_events_or_an_empty_page_after_the_wait() {
    C::run(async {
        const LONG_POLL_ADDR: &str = "127.0.0.1:21305";
        let (node, client, _temp) = boot_node(LONG_POLL_ADDR, "events-long-poll").await;
        let cursor = seed_and_cursor(&client).await;

        // Idle at the tail: the request waits about `wait_ms`, then returns an
        // empty page instead of blocking forever.
        let started = Instant::now();
        let idle = client
            .events(&EventsRequest {
                after: Some(cursor),
                limit: Some(10),
                wait_ms: Some(100),
            })
            .await
            .unwrap()
            .unwrap();
        let idle_elapsed = started.elapsed();
        assert!(idle.events.is_empty(), "unexpected events: {idle:?}");
        assert!(
            idle_elapsed >= Duration::from_millis(50),
            "long poll returned before the wait: {idle_elapsed:?}"
        );
        assert!(idle_elapsed < Duration::from_secs(2), "long poll overran");

        // A write racing the long poll wakes it promptly, well before the
        // 1000 ms budget.
        let writer_client = MatchEngineClient::<C>::new(1, LONG_POLL_ADDR.to_string());
        let writer = C::spawn(async move {
            C::sleep(Duration::from_millis(150)).await;
            writer_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    100.0,
                    4.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        });
        let started = Instant::now();
        let page = client
            .events(&EventsRequest {
                after: Some(cursor),
                limit: Some(10),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap()
            .unwrap();
        let wake_elapsed = started.elapsed();
        writer.await.unwrap();
        assert!(
            page.events
                .iter()
                .any(|event| event.kind == MatchEventKind::Fill),
            "long poll missed the fill: {page:?}"
        );
        assert!(
            wake_elapsed < Duration::from_millis(900),
            "long poll did not wake promptly: {wake_elapsed:?}"
        );

        node.shutdown().await.unwrap();
    });
}

#[test]
fn event_service_streams_pages_with_an_idle_heartbeat() {
    use matchengine_raft::network::api::EventsResponse;
    use matchengine_raft::network::grpc::pb::StreamEventsRequest;
    use matchengine_raft::network::grpc::pb::event_service_client::EventServiceClient;

    C::run(async {
        const STREAM_ADDR: &str = "127.0.0.1:21306";
        const STREAM_GRPC: &str = "http://127.0.0.1:22306";
        let (node, client, _temp) = boot_node(STREAM_ADDR, "events-stream").await;
        let cursor = seed_and_cursor(&client).await;

        let mut grpc = EventServiceClient::connect(STREAM_GRPC.to_string())
            .await
            .expect("connect event service");

        // Idle stream: one empty page after `wait_ms`, then `max_pages` ends it.
        let started = Instant::now();
        let mut stream = grpc
            .stream_events(StreamEventsRequest {
                after_log_index: Some(cursor.log_index),
                after_ordinal: Some(cursor.ordinal),
                limit: Some(10),
                wait_ms: Some(100),
                max_pages: Some(1),
            })
            .await
            .unwrap()
            .into_inner();
        let frame = stream.message().await.unwrap().expect("heartbeat page");
        let page: EventsResponse = serde_json::from_slice(&frame.payload).unwrap_or_else(|error| {
            panic!("decode {} payload bytes: {error}", frame.payload.len())
        });
        assert!(
            page.events.is_empty(),
            "unexpected heartbeat events: {page:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "heartbeat returned too early: {:?}",
            started.elapsed()
        );
        assert!(
            stream.message().await.unwrap().is_none(),
            "max_pages did not end the stream"
        );

        // A write after the cursor is streamed as a fill page.
        let writer_client = MatchEngineClient::<C>::new(1, STREAM_ADDR.to_string());
        let writer = C::spawn(async move {
            C::sleep(Duration::from_millis(150)).await;
            writer_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    100.0,
                    4.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
        });
        let mut stream = grpc
            .stream_events(StreamEventsRequest {
                after_log_index: Some(cursor.log_index),
                after_ordinal: Some(cursor.ordinal),
                limit: Some(10),
                wait_ms: Some(500),
                max_pages: Some(10),
            })
            .await
            .unwrap()
            .into_inner();
        let mut seen_fill = false;
        while let Some(frame) = stream.message().await.unwrap() {
            let page: EventsResponse =
                serde_json::from_slice(&frame.payload).unwrap_or_else(|error| {
                    panic!("decode {} payload bytes: {error}", frame.payload.len())
                });
            if page
                .events
                .iter()
                .any(|event| event.kind == MatchEventKind::Fill)
            {
                seen_fill = true;
                break;
            }
        }
        writer.await.unwrap();
        assert!(seen_fill, "StreamEvents did not deliver the fill");

        // A partial cursor is a malformed request, not a silent default.
        let error = grpc
            .stream_events(StreamEventsRequest {
                after_log_index: Some(cursor.log_index),
                after_ordinal: None,
                limit: None,
                wait_ms: None,
                max_pages: Some(1),
            })
            .await
            .unwrap_err();
        assert_eq!(tonic::Code::InvalidArgument, error.code());

        node.shutdown().await.unwrap();
    });
}

#[test]
fn bridge_sidecar_publishes_events_and_checkpoints_the_cursor() {
    C::run(async {
        const BRIDGE_ADDR: &str = "127.0.0.1:21307";
        let (node, client, temp) = boot_node(BRIDGE_ADDR, "events-bridge").await;

        // Two orders produce a rested event and a fill.
        client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                10.0,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Sell,
                100.0,
                4.0,
                0,
            )))
            .await
            .unwrap()
            .unwrap();

        let out = temp.path().join("events.jsonl");
        let cursor = temp.path().join("events.cursor");
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_events_bridge"))
            .args([
                "--engine",
                BRIDGE_ADDR,
                "--engine-id",
                "1",
                "--out",
                out.to_str().unwrap(),
                "--cursor",
                cursor.to_str().unwrap(),
                "--limit",
                "10",
                "--wait-ms",
                "0",
                "--max-pages",
                "1",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "bridge exited with {status}");

        let published: Vec<matchengine_raft::match_result::MatchEvent> =
            std::fs::read_to_string(&out)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(2, published.len(), "expected rest + fill: {published:?}");
        assert_eq!(MatchEventKind::Rested, published[0].kind);
        assert_eq!(MatchEventKind::Fill, published[1].kind);

        let checkpoint: matchengine_raft::match_result::EventId =
            serde_json::from_slice(&std::fs::read(&cursor).unwrap()).unwrap();
        let page = client
            .events(&EventsRequest {
                after: None,
                limit: Some(10),
                wait_ms: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.last_event_id, Some(checkpoint));

        node.shutdown().await.unwrap();
    });
}
