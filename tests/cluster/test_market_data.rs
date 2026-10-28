//! End-to-end test for the market-data feed.
//!
//! Boots a single node with the JSONL sink and checks that applied entries
//! produce the documented messages: a reset snapshot on leadership, trades,
//! BBO, coalesced depth and OHLC bars, plus the `/read` status counters.

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
use matchengine_raft::matchengine::Order;
use matchengine_raft::matchengine::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21303";

#[test]
fn market_data_feed_publishes_trades_bbo_depth_and_bars() {
    C::run(async {
        let temp = TempDir::new("market-data");
        let event_log = temp.path().join("market-data.jsonl");
        let mut market_data = MarketDataConfig::jsonl(&event_log);
        market_data.bar_intervals_ms = vec![1_000];
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("market-data")
            .with_market_data(market_data);

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
        wait_for_publishing(&client).await;

        // Leadership gain must reset consumers with a snapshot before any delta.
        wait_for_events(&event_log, |events| {
            events
                .iter()
                .any(|event| event["body"].get("depth_snapshot").is_some())
        })
        .await;

        let place = |order: Order| MatchEngineRequest::Place { order };

        // Two bids rest, then a sell fills four of the best bid.
        client
            .write(&place(Order::new(OrderSide::Buy, 100.0, 10.0, 0)))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&place(Order::new(OrderSide::Buy, 99.0, 5.0, 0)))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&place(Order::new(OrderSide::Sell, 100.0, 4.0, 0)))
            .await
            .unwrap()
            .unwrap();
        wait_for_events(&event_log, |events| {
            events
                .iter()
                .any(|event| event["body"].get("trade").is_some())
        })
        .await;

        // A second print after a bar window closes the first second-bar.
        C::sleep(Duration::from_millis(1_100)).await;
        client
            .write(&place(Order::new(OrderSide::Sell, 100.0, 1.0, 0)))
            .await
            .unwrap()
            .unwrap();
        client
            .write(&place(Order::new(OrderSide::Sell, 101.0, 1.0, 0)))
            .await
            .unwrap()
            .unwrap();

        // Amend the remaining 100-bid down by two, then cancel the resting ask.
        let mut bid_key = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        bid_key.sequence = 0;
        client
            .write(&MatchEngineRequest::Amend {
                order: bid_key,
                price: None,
                volume: Some(3.0),
            })
            .await
            .unwrap()
            .unwrap();
        let mut ask_key = Order::new(OrderSide::Sell, 101.0, 1.0, 0);
        ask_key.sequence = 3;
        client
            .write(&MatchEngineRequest::Cancel { order: ask_key })
            .await
            .unwrap()
            .unwrap();

        let amended_level = |events: &[serde_json::Value]| {
            events
                .iter()
                .filter_map(|event| event["body"]["depth_delta"].as_array())
                .flatten()
                .any(|level| {
                    level["side"] == "Buy"
                        && level["level"]["price"] == 100.0
                        && level["level"]["volume"] == 3.0
                })
        };
        let events = wait_for_events(&event_log, |events| {
            let trades = events
                .iter()
                .filter(|event| event["body"].get("trade").is_some())
                .count();
            let bar = events
                .iter()
                .any(|event| event["body"].get("bar").is_some());
            trades >= 2 && bar && amended_level(events)
        })
        .await;

        // Trades are batched, complete and carry the taker side.
        let first_trade = events
            .iter()
            .filter_map(|event| event["body"]["trade"].as_array())
            .flatten()
            .next()
            .expect("at least one fill is published");
        assert_eq!(100.0, first_trade["price"]);
        assert_eq!(4.0, first_trade["volume"]);
        assert_eq!("Sell", first_trade["taker_side"]);
        assert!(first_trade["log_index"].as_u64().unwrap() > 0);

        // BBO eventually shows the resting ask and the last trade.
        let bbo = events
            .iter()
            .find(|event| event["body"]["bbo"]["ask"] == 101.0)
            .expect("a BBO with the resting ask");
        assert_eq!(100.0, bbo["body"]["bbo"]["bid"]);
        assert_eq!(100.0, bbo["body"]["bbo"]["last_trade"]);
        assert_eq!(1.0, bbo["body"]["bbo"]["spread"]);

        // The first second-bar aggregates the first fill.
        let bar = events
            .iter()
            .find(|event| event["body"].get("bar").is_some())
            .expect("a closed bar");
        assert_eq!(1_000, bar["body"]["bar"]["interval_ms"]);
        assert_eq!(100.0, bar["body"]["bar"]["open"]);
        assert_eq!(100.0, bar["body"]["bar"]["close"]);
        assert!(bar["body"]["bar"]["trade_count"].as_u64().unwrap() >= 1);

        // The quantity amendment shows up as a post-batch depth total.
        let amended = events
            .iter()
            .filter_map(|event| event["body"]["depth_delta"].as_array())
            .flatten()
            .find(|level| {
                level["side"] == "Buy"
                    && level["level"]["price"] == 100.0
                    && level["level"]["volume"] == 3.0
            })
            .expect("the amendment's post-batch total");
        assert_eq!(1, amended["level"]["orders"]);

        // `/read marketdata_status` reflects the running feed.
        let status = client.read(&"marketdata_status".to_string()).await.unwrap();
        let status: serde_json::Value = serde_json::from_str(&status).expect("status json");
        assert_eq!(Some(true), status["publishing"].as_bool());
        assert!(status["trades"].as_u64().unwrap() >= 2);
        assert!(status["depth_snapshots"].as_u64().unwrap() >= 1);
        assert!(status["bars"].as_u64().unwrap() >= 1);

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

/// Waits until the leadership watcher has opened the gate and installed a snapshot.
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

fn read_events(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect()
}

async fn wait_for_events(
    path: &Path,
    ready: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = read_events(path);
        if ready(&events) {
            return events;
        }
        assert!(
            Instant::now() <= deadline,
            "market-data condition not met, {} lines: {:?}",
            events.len(),
            events
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}
