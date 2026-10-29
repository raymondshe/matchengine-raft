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
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
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

        // Leadership gain must reset consumers with a snapshot before any
        // trade, BBO or delta is published in that session.
        let events = wait_for_events(&event_log, |events| {
            events
                .iter()
                .any(|event| event["body"].get("depth_snapshot").is_some())
        })
        .await;
        let first_snapshot = events
            .iter()
            .position(|event| event["body"].get("depth_snapshot").is_some())
            .expect("snapshot present");
        assert!(
            events[..first_snapshot].iter().all(|event| {
                event["body"].get("trade").is_none()
                    && event["body"].get("bbo").is_none()
                    && event["body"].get("depth_delta").is_none()
            }),
            "market data published before the reset snapshot: {events:?}"
        );

        let place = |order: Order| MatchEngineRequest::place(order);

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
        let sell_seq = client
            .write(&place(Order::new(OrderSide::Sell, 100.0, 4.0, 0)))
            .await
            .unwrap()
            .unwrap()
            .data
            .value
            .expect("sell sequence")
            .parse::<u64>()
            .expect("numeric sequence");
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
        let amend_seq = client
            .write(&MatchEngineRequest::Amend {
                order: bid_key,
                price: None,
                volume: Some(3.0.into()),
            })
            .await
            .unwrap()
            .unwrap()
            .data
            .value
            .expect("amend sequence")
            .parse::<u64>()
            .expect("numeric sequence");
        let mut ask_key = Order::new(OrderSide::Sell, 101.0, 1.0, 0);
        ask_key.sequence = 4; // the resting ask was the fourth placement
        let cancel_seq = client
            .write(&MatchEngineRequest::Cancel { order: ask_key })
            .await
            .unwrap()
            .unwrap()
            .data
            .value
            .expect("cancel sequence")
            .parse::<u64>()
            .expect("numeric sequence");
        assert!(cancel_seq > amend_seq);

        let amended_level = |events: &[serde_json::Value]| {
            events
                .iter()
                .filter_map(|event| event["body"]["depth_delta"].as_array())
                .flatten()
                .any(|level| {
                    level["side"] == "Buy"
                        && level["level"]["price"] == "100"
                        && level["level"]["volume"] == "3"
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
            let status = events
                .iter()
                .any(|event| event["body"].get("status").is_some());
            trades >= 2 && bar && status && amended_level(events)
        })
        .await;

        // Trades are complete, carry the taker side and are anchored to the
        // taker order's replicated sequence.
        let first_trade_message = events
            .iter()
            .find(|event| {
                event["body"]["trade"]
                    .as_array()
                    .is_some_and(|records| records.iter().any(|r| r["volume"] == "4"))
            })
            .expect("the four-fill trade");
        assert_eq!(sell_seq, first_trade_message["market_seq"]);
        assert_eq!(0, first_trade_message["sub_seq"]);
        let first_trade = first_trade_message["body"]["trade"]
            .as_array()
            .and_then(|records| records.first())
            .expect("at least one fill is published");
        assert_eq!("100", first_trade["price"]);
        assert_eq!("4", first_trade["volume"]);
        assert_eq!("Sell", first_trade["taker_side"]);
        assert!(
            first_trade["ts_ms"].as_u64().unwrap() > 1_000,
            "the leader stamps intake time, not the client placeholder"
        );
        assert!(first_trade["log_index"].as_u64().unwrap() > 0);

        // BBO eventually shows the resting ask and the last trade.
        let bbo = events
            .iter()
            .find(|event| event["body"]["bbo"]["ask"] == "101")
            .expect("a BBO with the resting ask");
        assert_eq!("100", bbo["body"]["bbo"]["bid"]);
        assert_eq!("100", bbo["body"]["bbo"]["last_trade"]);
        assert_eq!("1", bbo["body"]["bbo"]["spread"]);

        // The first second-bar aggregates the first fill.
        let bar = events
            .iter()
            .find(|event| event["body"].get("bar").is_some())
            .expect("a closed bar");
        assert_eq!(1_000, bar["body"]["bar"]["interval_ms"]);
        assert_eq!("100", bar["body"]["bar"]["open"]);
        assert_eq!("100", bar["body"]["bar"]["close"]);
        assert!(bar["body"]["bar"]["trade_count"].as_u64().unwrap() >= 1);

        // The quantity amendment shows up as a post-batch depth total.
        let amended = events
            .iter()
            .filter_map(|event| event["body"]["depth_delta"].as_array())
            .flatten()
            .find(|level| {
                level["side"] == "Buy"
                    && level["level"]["price"] == "100"
                    && level["level"]["volume"] == "3"
            })
            .expect("the amendment's post-batch total");
        assert_eq!(1, amended["level"]["orders"]);
        let amended_message = events
            .iter()
            .find(|event| {
                event["body"]["depth_delta"]
                    .as_array()
                    .is_some_and(|levels| {
                        levels.iter().any(|level| {
                            level["side"] == "Buy"
                                && level["level"]["price"] == "100"
                                && level["level"]["volume"] == "3"
                        })
                    })
            })
            .expect("the amendment's depth message");
        assert_eq!(amend_seq, amended_message["market_seq"]);

        // `/read marketdata_status` reflects the running feed once the tap
        // has caught up with the last write.
        let status = wait_for_market_seq(&client, cancel_seq).await;
        assert_eq!(Some(true), status["publishing"].as_bool());
        assert_eq!(Some("jsonl"), status["sink"].as_str());
        assert!(status["trades"].as_u64().unwrap() >= 2);
        assert!(status["depth_snapshots"].as_u64().unwrap() >= 1);
        assert!(status["bars"].as_u64().unwrap() >= 1);
        assert!(status["status"].as_u64().unwrap() >= 1);
        assert!(status["last_market_seq"].as_u64().unwrap() >= cancel_seq);

        node.shutdown().await.unwrap();
    });
}

async fn wait_for_market_seq(client: &MatchEngineClient<C>, minimum: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = client.read(&"marketdata_status".to_string()).await.unwrap();
        let status: serde_json::Value = serde_json::from_str(&status).expect("status json");
        if status["last_market_seq"].as_u64().unwrap_or(0) >= minimum {
            return status;
        }
        assert!(
            Instant::now() <= deadline,
            "feed never caught up to sequence {minimum}: {status}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
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
