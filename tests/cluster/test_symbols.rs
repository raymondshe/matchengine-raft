//! One process = one symbol.
//!
//! The symbol and its price/quantity precision are replicated and snapshotted.
//! On the leader, the HTTP edge rejects a wrong-symbol order with `400` before
//! it reaches Raft; the state machine re-checks the symbol as defense in depth.
//! The configuration can only change while the book is empty.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::RejectReason;
use matchengine_raft::order_book::SymbolConfig;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21801";
const ADDR_2: &str = "127.0.0.1:21802";

fn fixed(value: &str) -> Fixed {
    value.parse().expect("valid exact decimal")
}

#[test]
fn symbol_configuration_validates_orders_and_survives_restart() {
    C::run(async {
        let temp = TempDir::new("symbols");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("symbols");

        let node =
            build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage.clone())
                .await
                .unwrap();

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        // The HTTP edge rejects a wrong-symbol order before it reaches Raft.
        let response = raw_write(
            ADDR,
            &MatchEngineRequest::place(
                Order::new(OrderSide::Buy, fixed("100"), fixed("1"), 0).with_symbol(1),
            ),
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let body = response.text().await.unwrap();
        assert!(
            body.contains("does not match the configured symbol 0"),
            "{body}"
        );
        assert_eq!(
            "0",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        assert_eq!("0", client.read(&"symbol_id".to_string()).await.unwrap());
        let config: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(SymbolConfig::default(), config);

        let configured = client
            .write(&MatchEngineRequest::ConfigureSymbol {
                config: SymbolConfig::new(0, 2, 2).unwrap(),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(configured.data.error.is_none());
        let config: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(SymbolConfig::new(0, 2, 2).unwrap(), config);

        // Three fractional digits exceed the configured price precision.
        let rejected = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                fixed("100.255"),
                fixed("1"),
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(1, rejected.data.events.len());
        assert_eq!(MatchEventKind::Rejected, rejected.data.events[0].kind);
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            rejected.data.events[0].reason
        );
        assert_eq!(
            "[]",
            client.read(&"orderbook_orders".to_string()).await.unwrap()
        );

        // The same order at two decimals is accepted.
        let accepted = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                fixed("100.25"),
                fixed("1.5"),
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(MatchEventKind::Rested, accepted.data.events[0].kind);
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("100.25", orders[0]["price"]);
        assert_eq!("1.5", orders[0]["volume"]);

        // Changing the symbol configuration under an open order is refused.
        let refused = client
            .write(&MatchEngineRequest::ConfigureSymbol {
                config: SymbolConfig::new(0, 3, 3).unwrap(),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(refused.data.error.is_some());
        let config: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(SymbolConfig::new(0, 2, 2).unwrap(), config);

        let sequence_after_accept = client
            .read(&"orderbook_sequence".to_string())
            .await
            .unwrap();

        // The HTTP edge rejects symbol-1 orders before Raft; the book and the
        // sequence counter must not change.
        let response = raw_write(
            ADDR,
            &MatchEngineRequest::place(
                Order::new(OrderSide::Sell, fixed("200.12"), fixed("1"), 0).with_symbol(1),
            ),
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let body = response.text().await.unwrap();
        assert!(
            body.contains("does not match the configured symbol 0"),
            "{body}"
        );
        assert_eq!(
            sequence_after_accept,
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // A cancel or amend keyed for the wrong symbol is also rejected at the
        // edge and must not touch the symbol-0 order.
        let wrong_key = Order::new(OrderSide::Buy, fixed("100.25"), fixed("1.5"), 0).with_symbol(1);
        let response = raw_write(ADDR, &MatchEngineRequest::Cancel { order: wrong_key }).await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let response = raw_write(
            ADDR,
            &MatchEngineRequest::Amend {
                order: wrong_key,
                price: Some(fixed("100.20")),
                volume: Some(fixed("1")),
            },
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        assert_eq!(
            sequence_after_accept,
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.as_array().unwrap().len());

        // One bad item rejects the whole batch before Raft, so the valid item
        // before it is not partially applied either.
        let response = raw_write_batch(
            ADDR,
            &[
                MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    fixed("99.00"),
                    fixed("1"),
                    0,
                )),
                MatchEngineRequest::place(
                    Order::new(OrderSide::Sell, fixed("200.12"), fixed("1"), 0).with_symbol(1),
                ),
            ],
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let body = response.text().await.unwrap();
        assert!(body.contains("batch request 1:"), "{body}");
        assert_eq!(
            sequence_after_accept,
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.as_array().unwrap().len());

        let snapshot = node.state_machine.persist_snapshot().await.unwrap();
        assert!(snapshot.last_log_id.is_some());
        let bytes = std::fs::read(storage.snapshot_dir(1).join("current.snap")).unwrap();
        assert_eq!(
            9,
            u32::from_le_bytes(bytes[7..11].try_into().unwrap()),
            "snapshot must be v9"
        );

        node.shutdown().await.unwrap();

        // The symbol configuration and book survive the snapshot restart.
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        wait_for_leader(&client).await;

        assert_eq!("0", client.read(&"symbol_id".to_string()).await.unwrap());
        let config: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(SymbolConfig::new(0, 2, 2).unwrap(), config);
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("100.25", orders[0]["price"]);
        assert_eq!("1.5", orders[0]["volume"]);
        let refused = client
            .write(&MatchEngineRequest::ConfigureSymbol {
                config: SymbolConfig::new(0, 3, 3).unwrap(),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            refused.data.error.is_some(),
            "the symbol lock survives restart"
        );

        node.shutdown().await.unwrap();
    });
}

#[test]
fn gateway_symbol_guard_allows_configure_and_place_batch() {
    C::run(async {
        let temp = TempDir::new("symbol-guard-batch");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("symbol-guard-batch");

        let node = build_match_engine_node(1, ADDR_2.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR_2.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        // A ConfigureSymbol earlier in the batch changes the symbol for the
        // following order, so the edge guard must not reject it against the
        // old symbol.
        let config = SymbolConfig::new(1, 2, 2).unwrap();
        let order = Order::new(OrderSide::Buy, fixed("100.25"), fixed("1.5"), 0).with_symbol(1);
        let response = raw_write_batch(
            ADDR_2,
            &[
                MatchEngineRequest::ConfigureSymbol {
                    config: config.clone(),
                },
                MatchEngineRequest::place(order),
            ],
        )
        .await;
        assert_eq!(StatusCode::OK, response.status());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            0,
            body["Ok"]["data"]["batch"][1]["sequence"].as_u64().unwrap()
        );
        assert_eq!(
            "rested",
            body["Ok"]["data"]["batch"][1]["events"][0]["kind"]
        );
        assert_eq!("1", client.read(&"symbol_id".to_string()).await.unwrap());
        assert_eq!(
            "1",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // The guard now uses the new symbol, so a default-symbol order is
        // rejected without touching the applied state.
        let response = raw_write(
            ADDR_2,
            &MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                fixed("100.25"),
                fixed("1.5"),
                0,
            )),
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        assert_eq!(
            "1",
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );

        // Wrong-symbol cancel and amend keys do not reach the book.
        let wrong_key = Order::new(OrderSide::Buy, fixed("100.25"), fixed("1.5"), 0);
        let response = raw_write(ADDR_2, &MatchEngineRequest::Cancel { order: wrong_key }).await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        let response = raw_write(
            ADDR_2,
            &MatchEngineRequest::Amend {
                order: wrong_key,
                price: Some(fixed("100.20")),
                volume: Some(fixed("1")),
            },
        )
        .await;
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.as_array().unwrap().len());
        assert_eq!("100.25", orders[0]["price"]);
        assert_eq!("1.5", orders[0]["volume"]);

        node.shutdown().await.unwrap();
    });
}

async fn raw_write(addr: &str, request: &MatchEngineRequest) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{addr}/write"))
        .json(request)
        .send()
        .await
        .expect("send /write")
}

async fn raw_write_batch(addr: &str, requests: &[MatchEngineRequest]) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{addr}/write_batch"))
        .json(requests)
        .send()
        .await
        .expect("send /write_batch")
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
