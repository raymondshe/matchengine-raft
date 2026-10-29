//! End-to-end maker/taker fees.
//!
//! The gateway supplies user-level fee rates with each order. The matching
//! engine records exact fees on every fill and exposes them through `/write`,
//! `/events`, the JSONL sink and the market-data feed.

use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::market_data::MarketDataConfig;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21901";

fn fixed(value: &str) -> Fixed {
    value.parse().expect("valid exact decimal")
}

#[test]
fn maker_and_taker_fees_survive_events_feed_and_snapshot_restart() {
    C::run(async {
        let temp = TempDir::new("fees");
        let market_log = temp.path().join("market-data.jsonl");
        let event_log = temp.path().join("match-events.jsonl");
        let mut market_data = MarketDataConfig::jsonl(&market_log);
        market_data.bar_intervals_ms = vec![1_000];
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("fees")
            .with_match_event_log(&event_log)
            .with_market_data(market_data);

        let node = build_match_engine_node(
            1,
            ADDR.to_string(),
            Transport::Grpc,
            default_raft_config(),
            storage.clone(),
        )
        .await
        .unwrap();

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;
        wait_for_publishing(&client).await;

        // Maker pays 10 bps, taker pays 40 bps. Maker's taker rate is not used
        // for this fill, and the taker's maker rate is only for a future rest.
        let maker =
            Order::new(OrderSide::Sell, 100, 2, 0).with_fee_rates(fixed("0.001"), fixed("0.002"));
        let taker =
            Order::new(OrderSide::Buy, 100, 1, 0).with_fee_rates(fixed("0.003"), fixed("0.004"));

        let rested = client
            .write(&MatchEngineRequest::place(maker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(MatchEventKind::Rested, rested.data.events[0].kind);
        assert_eq!(Fixed::ZERO, rested.data.events[0].taker_fee);
        assert_eq!(Fixed::ZERO, rested.data.events[0].maker_fee);

        let filled = client
            .write(&MatchEngineRequest::place(taker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(1, filled.data.events.len());
        assert_eq!(MatchEventKind::Fill, filled.data.events[0].kind);
        assert_eq!(fixed("0.004"), filled.data.events[0].taker_fee);
        assert_eq!(fixed("0.1"), filled.data.events[0].maker_fee);
        assert!(!filled.data.events[0].fee_overflow);

        let page = client
            .events(&EventsRequest {
                after: None,
                limit: Some(100),
            })
            .await
            .unwrap()
            .unwrap();
        let fill = page
            .events
            .iter()
            .find(|event| event.kind == MatchEventKind::Fill)
            .expect("the fill is retained");
        assert_eq!(fixed("0.004"), fill.taker_fee);
        assert_eq!(fixed("0.1"), fill.maker_fee);
        assert!(!fill.fee_overflow);

        // The order keeps its rates for the next maker fill.
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("0.001", orders[0]["maker_fee_rate"]);
        assert_eq!("0.002", orders[0]["taker_fee_rate"]);

        // The file sink carries the same fill fees.
        let file_events = wait_for_file(&event_log, |events| {
            events.iter().any(|event| {
                event["kind"] == "fill"
                    && event["taker_fee"] == "0.004"
                    && event["maker_fee"] == "0.1"
            })
        })
        .await;
        assert!(file_events.iter().any(|event| event["kind"] == "fill"));

        // The market-data TRADE and LIFECYCLE records expose the fees.
        let messages = wait_for_market(&market_log, |messages| {
            let trade = messages.iter().any(|message| {
                message["body"]["trade"].as_array().is_some_and(|records| {
                    records.iter().any(|record| {
                        record["taker_fee"] == "0.004"
                            && record["maker_fee"] == "0.1"
                            && record["taker_fee_asset"] == "base"
                            && record["maker_fee_asset"] == "quote"
                    })
                })
            });
            let lifecycle = messages.iter().any(|message| {
                message["body"]["lifecycle"]
                    .as_array()
                    .is_some_and(|records| {
                        records.iter().any(|record| {
                            record["kind"] == "fill"
                                && record["taker_fee"] == "0.004"
                                && record["maker_fee"] == "0.1"
                                && record["taker_fee_asset"] == "base"
                                && record["maker_fee_asset"] == "quote"
                        })
                    })
            });
            trade && lifecycle
        })
        .await;
        assert!(
            messages
                .iter()
                .any(|message| message["body"].get("trade").is_some())
        );

        let snapshot = node.state_machine.persist_snapshot().await.unwrap();
        assert!(snapshot.last_log_id.is_some());
        let bytes = std::fs::read(storage.snapshot_dir(1).join("current.snap")).unwrap();
        assert_eq!(
            9,
            u32::from_le_bytes(bytes[7..11].try_into().unwrap()),
            "snapshot must be v9"
        );

        node.shutdown().await.unwrap();

        // The maker rate survives the snapshot: the next fill still uses 10 bps.
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
        wait_for_leader(&client).await;

        let taker =
            Order::new(OrderSide::Buy, 100, 1, 0).with_fee_rates(fixed("0.003"), fixed("0.004"));
        let filled = client
            .write(&MatchEngineRequest::place(taker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fixed("0.1"), filled.data.events[0].maker_fee);
        assert_eq!(fixed("0.004"), filled.data.events[0].taker_fee);

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

async fn wait_for_publishing(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = client.read(&"marketdata_status".to_string()).await.unwrap();
        if let Ok(status) = serde_json::from_str::<serde_json::Value>(&status)
            && status["publishing"] == true
            && status["depth_snapshots"].as_u64().unwrap_or(0) >= 1
        {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "the feed never started publishing: {status}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_market(
    path: &Path,
    ready: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    wait_for_file(path, ready).await
}

async fn wait_for_file(
    path: &Path,
    ready: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = read_jsonl(path);
        if ready(&events) {
            return events;
        }
        assert!(
            Instant::now() <= deadline,
            "file condition not met, {} lines: {events:?}",
            events.len()
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect()
}
