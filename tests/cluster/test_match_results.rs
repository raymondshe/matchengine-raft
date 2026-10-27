//! End-to-end test for match-result processing.
//!
//! Boots a single-node cluster with the file sink enabled and checks that every applied
//! rest / fill / cancel shows up in the JSONL file, in Raft log order. This covers the whole
//! path: `build_match_engine_node` -> `StateMachineStore::apply` -> `MatchResultSink`.

use std::path::Path;
use std::time::Duration;
use std::time::Instant;

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
            .write(&MatchEngineRequest::Place {
                order: Order::new(OrderSide::Buy, 100.0, 10.0, 0),
            })
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
            .write(&MatchEngineRequest::Place {
                order: Order::new(OrderSide::Sell, 100.0, 4.0, 0),
            })
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
