//! End-to-end exactness tests for the fixed-point migration.
//!
//! A price and quantity with all 18 fractional digits must survive the public
//! API, match events, the market-data feed and a snapshot restart without ever
//! passing through `f64`.

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

const ADDR: &str = "127.0.0.1:21701";

fn fixed(value: &str) -> Fixed {
    value.parse().expect("valid exact decimal")
}

#[test]
fn exact_18_decimal_values_survive_api_events_feed_and_snapshot_restart() {
    C::run(async {
        let temp = TempDir::new("exactness");
        let market_log = temp.path().join("market-data.jsonl");
        let mut market_data = MarketDataConfig::jsonl(&market_log);
        market_data.bar_intervals_ms = vec![1_000];
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("exactness")
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

        // All 18 fractional digits are significant here.
        let maker_price = fixed("100.000000000000000001");
        let maker_volume = fixed("2.000000000000000003");
        let taker_price = fixed("100.000000000000000002");
        let taker_volume = fixed("1.000000000000000001");

        let rested = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Sell,
                maker_price,
                maker_volume,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(1, rested.data.events.len());
        assert_eq!(MatchEventKind::Rested, rested.data.events[0].kind);
        assert_eq!(maker_price, rested.data.events[0].price);
        assert_eq!(maker_volume, rested.data.events[0].volume);

        let filled = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                taker_price,
                taker_volume,
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(1, filled.data.events.len());
        assert_eq!(MatchEventKind::Fill, filled.data.events[0].kind);
        assert_eq!(maker_price, filled.data.events[0].price);
        assert_eq!(taker_volume, filled.data.events[0].volume);

        // `/read` values are decimal strings, not JSON numbers.
        assert_eq!(
            "100.000000000000000001",
            client
                .read(&"orderbook_last_trade_price".to_string())
                .await
                .unwrap()
        );
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("100.000000000000000001", orders[0]["price"]);
        assert_eq!("1.000000000000000002", orders[0]["volume"]);

        let depth: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_depth:1".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("100.000000000000000001", depth["asks"][0]["price"]);
        assert_eq!("1.000000000000000002", depth["asks"][0]["volume"]);

        // The replicated event window carries the same exact values.
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
        assert_eq!(maker_price, fill.price);
        assert_eq!(taker_volume, fill.volume);
        let rest = page
            .events
            .iter()
            .find(|event| event.kind == MatchEventKind::Rested)
            .expect("the rest is retained");
        assert_eq!(maker_price, rest.price);
        assert_eq!(maker_volume, rest.volume);

        // The market-data JSONL feed also uses decimal strings.
        let messages = wait_for_market_trade(&market_log, |message| {
            message["body"]["trade"].as_array().is_some_and(|records| {
                records.iter().any(|record| {
                    record["price"] == "100.000000000000000001"
                        && record["volume"] == "1.000000000000000001"
                })
            })
        })
        .await;
        let bbo = messages
            .iter()
            .find(|message| message["body"]["bbo"]["last_trade"] == "100.000000000000000001")
            .and_then(|message| message["body"]["bbo"].as_object())
            .expect("the feed publishes a post-fill BBO");
        assert_eq!("100.000000000000000001", bbo["ask"]);
        assert_eq!("1.000000000000000002", bbo["ask_volume"]);
        assert_eq!("100.000000000000000001", bbo["last_trade"]);

        // Write a snapshot and prove it is the fixed-point v9 format before
        // restarting from it.
        let snapshot_meta = node.state_machine.persist_snapshot().await.unwrap();
        assert!(snapshot_meta.last_log_id.is_some());
        let snapshot_path = storage.snapshot_dir(1).join("current.snap");
        let bytes = std::fs::read(&snapshot_path).unwrap();
        assert_eq!(b"MRFSNAP", &bytes[..7]);
        assert_eq!(
            9,
            u32::from_le_bytes(bytes[7..11].try_into().unwrap()),
            "snapshot must be v9"
        );

        node.shutdown().await.unwrap();

        // Restart from the same storage and check the exact state again.
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

        assert_eq!(
            "100.000000000000000001",
            client
                .read(&"orderbook_last_trade_price".to_string())
                .await
                .unwrap()
        );
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("100.000000000000000001", orders[0]["price"]);
        assert_eq!("1.000000000000000002", orders[0]["volume"]);

        let page = client
            .events(&EventsRequest {
                after: None,
                limit: Some(100),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(page.events.iter().any(|event| {
            event.kind == MatchEventKind::Fill
                && event.price == maker_price
                && event.volume == taker_volume
        }));
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

async fn wait_for_market_trade(
    path: &Path,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let messages = read_jsonl(path);
        if messages.iter().any(&ready) {
            return messages;
        }
        assert!(
            Instant::now() <= deadline,
            "market-data condition not met, {} lines: {messages:?}",
            messages.len()
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
