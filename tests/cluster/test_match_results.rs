//! End-to-end test for match-result processing.
//!
//! Boots a single-node cluster with the file sink enabled and checks that every applied
//! rest / fill / cancel shows up in the JSONL file, in Raft log order. This covers the whole
//! path: `build_match_engine_node` -> `StateMachineStore::apply` -> `MatchResultSink`.

use std::path::Path;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::matchengine::Order;
use matchengine_raft::matchengine::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21301";

#[test]
fn file_sink_records_rests_fills_and_cancels() {
    C::run(async {
        let temp = TempDir::new("match-results");
        let event_log = temp.path().join("match-events.jsonl");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("match-results")
            .with_match_event_log(&event_log);

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

        // A buy that cannot match rests in full (sequence 0).
        let response = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                10.0,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Some("0".to_string()),
            response.data.value,
            "write response must stay compatible"
        );

        // A sell of 4 crosses it: 4 fill, 6 stay on the bid (sequence 1).
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

        let events = wait_for_events(&event_log, 2).await;
        assert_eq!("rested", events[0]["kind"]);
        assert_eq!(0, events[0]["order_sequence"]);
        assert_eq!(10.0, events[0]["volume"]);
        assert_eq!(1, events[0]["node_id"]);

        assert_eq!("fill", events[1]["kind"]);
        assert_eq!(1, events[1]["order_sequence"]);
        assert_eq!(0, events[1]["counterparty_sequence"]);
        assert_eq!(100.0, events[1]["price"]);
        assert_eq!(4.0, events[1]["volume"]);
        assert_eq!(1, events[1]["node_id"]);

        // The fill is a later entry than the rest it matched against.
        let rest_index = events[0]["event_id"]["log_index"].as_u64().unwrap();
        let fill_index = events[1]["event_id"]["log_index"].as_u64().unwrap();
        assert!(fill_index > rest_index, "events must follow log order");

        // Cancelling the remainder reports the 6 that were still open.
        client
            .write(&MatchEngineRequest::Cancel {
                order: Order::new(OrderSide::Buy, 100.0, 6.0, 0),
            })
            .await
            .unwrap()
            .unwrap();

        let events = wait_for_events(&event_log, 3).await;
        let cancelled = &events[2];
        assert_eq!("cancelled", cancelled["kind"]);
        assert_eq!(0, cancelled["order_sequence"]);
        assert_eq!(6.0, cancelled["volume"]);
        assert_eq!(1, cancelled["node_id"]);
        assert!(
            cancelled["event_id"]["log_index"].as_u64().unwrap() > fill_index,
            "cancel must be ordered after the fill"
        );

        // The order book is empty: the cancel removed the only remaining order.
        let orders = client.read(&"orderbook_orders".to_string()).await.unwrap();
        assert_eq!("[]", orders);

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

/// The file is flushed after every batch, but the apply is asynchronous with respect to the
/// client response, so poll briefly instead of assuming the line is there.
async fn wait_for_events(path: &Path, expected: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = read_events(path);
        if events.len() >= expected {
            return events;
        }
        assert!(
            Instant::now() <= deadline,
            "expected {expected} match events, got {}: {events:?}",
            events.len()
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

fn read_events(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("the sink must write valid JSON lines"))
        .collect()
}

/// Drives the new order types through `/write` and checks the file sink:
/// a parked stop elected by a trade (trigger + child fill), a GTD order
/// evicted by `PurgeExpired`, and the new `/read` keys.
#[test]
fn file_sink_records_stop_triggers_and_expiries() {
    C::run(async {
        let temp = TempDir::new("match-results-types");
        let event_log = temp.path().join("match-events.jsonl");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("match-results-types")
            .with_match_event_log(&event_log);

        let node = build_match_engine_node(
            1,
            "127.0.0.1:21302".to_string(),
            Transport::Grpc,
            default_raft_config(),
            storage,
        )
        .await
        .unwrap();

        let client = MatchEngineClient::<C>::new(1, "127.0.0.1:21302".to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let place = |order: Order| MatchEngineRequest::place(order);

        // Two bids park liquidity at 100 and 95 (sequences 0 and 1).
        client
            .write(&place(Order::new(OrderSide::Buy, 100.0, 10.0, 0)))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&place(Order::new(OrderSide::Buy, 95.0, 10.0, 0)))
            .await
            .unwrap()
            .unwrap();

        // A sell stop at 95 parks off the book (sequence 2); no event yet.
        let mut stop = Order::new(OrderSide::Sell, 0.0, 5.0, 0);
        stop.live = matchengine_raft::matchengine::OrderType::Market;
        stop.stop_price = 95.0.into();
        client.write(&place(stop)).await.unwrap().unwrap();

        wait_for_events(&event_log, 2).await;
        assert_eq!(
            2,
            read_events(&event_log).len(),
            "parking publishes nothing"
        );

        // A print at 100 does not reach the stop; the next one at 95 does.
        client
            .write(&place(Order::new(OrderSide::Sell, 100.0, 10.0, 0)))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&place(Order::new(OrderSide::Sell, 95.0, 3.0, 0)))
            .await
            .unwrap()
            .unwrap();

        let events = wait_for_events(&event_log, 6).await;
        let triggered = events
            .iter()
            .find(|event| event["kind"] == "triggered")
            .expect("the stop must be elected");
        assert_eq!(2, triggered["order_sequence"]);
        assert_eq!(95.0, triggered["price"]);
        assert_eq!(5.0, triggered["volume"]);

        // The child executes as a market order and its fill follows the trigger
        // inside the same log entry.
        let child = events
            .iter()
            .find(|event| {
                event["kind"] == "fill"
                    && event["order_sequence"] == 2
                    && event["event_id"]["log_index"] == triggered["event_id"]["log_index"]
            })
            .expect("the stop's child must print");
        assert_eq!(5.0, child["volume"]);
        assert_eq!(
            triggered["event_id"]["ordinal"].as_u64().unwrap() + 1,
            child["event_id"]["ordinal"].as_u64().unwrap()
        );

        // A GTD order that is still live, then purged at its deadline. The
        // leader stamps intake time, so the deadline is relative to the clock.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let expiry = now_ms + 60_000;
        let mut gtd = Order::new(OrderSide::Buy, 90.0, 1.0, 0);
        gtd.tif = matchengine_raft::matchengine::TimeInForce::Gtd(expiry);
        gtd.timestamp = 1_000; // overwritten by the leader at intake
        client.write(&place(gtd)).await.unwrap().unwrap();
        wait_for_events(&event_log, 7).await;

        let response = client
            .write(&MatchEngineRequest::PurgeExpired {
                now_ms,
                day_close_ms: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.data.value.is_some(),
            "purge returns its assigned sequence"
        );

        let response = client
            .write(&MatchEngineRequest::PurgeExpired {
                now_ms: expiry,
                day_close_ms: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.data.value.is_some(),
            "purge returns its assigned sequence"
        );

        let events = wait_for_events(&event_log, 8).await;
        let expired = events
            .iter()
            .find(|event| event["kind"] == "expired")
            .expect("the GTD order expires");
        assert_eq!(5, expired["order_sequence"]);
        assert_eq!(1.0, expired["volume"]);

        // New read keys: the stop store is empty and the last trade printed 95.
        let stops = client.read(&"orderbook_stops".to_string()).await.unwrap();
        assert_eq!("[]", stops);
        let last = client
            .read(&"orderbook_last_trade_price".to_string())
            .await
            .unwrap();
        assert_eq!("95.0", last);

        node.shutdown().await.unwrap();
    });
}
