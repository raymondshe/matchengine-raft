//! Public-API correctness evidence.
//!
//! `feature_matrix_reaches_engine` sends one order of every advertised type
//! through `/write` and checks the event kinds the engine returns.
//! `event_lifecycle_reconstructs_state` follows one order through rest, amend,
//! fill and cancel, then checks expiry and stop events are explicit rather than
//! overloaded rejections. `negative_api_inputs_are_rejected` covers malformed
//! bodies and unknown read keys.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::RejectReason;
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::MatchEngineResponse;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

async fn start_node(addr: &str, label: &str) -> (MatchEngineNode, MatchEngineClient<C>, TempDir) {
    let temp = TempDir::new(label);
    let storage = StorageConfig::new(temp.path()).with_instance_prefix(label);
    let node = build_match_engine_node(1, addr.to_string(), default_raft_config(), storage)
        .await
        .unwrap();
    let client = MatchEngineClient::<C>::new(1, addr.to_string());
    client.init().await.unwrap().unwrap();
    wait_for_leader(&client).await;
    (node, client, temp)
}

async fn apply(client: &MatchEngineClient<C>, request: MatchEngineRequest) -> MatchEngineResponse {
    client.write(&request).await.unwrap().unwrap().data
}

fn kinds(events: &[MatchEvent]) -> Vec<MatchEventKind> {
    events.iter().map(|event| event.kind).collect()
}

fn limit(side: OrderSide, price: impl Into<Fixed>, volume: impl Into<Fixed>) -> Order {
    Order::new(side, price, volume, 0)
}

fn place(order: Order) -> MatchEngineRequest {
    MatchEngineRequest::place(order)
}

fn cancel(
    side: OrderSide,
    price: impl Into<Fixed>,
    volume: impl Into<Fixed>,
    sequence: u64,
) -> MatchEngineRequest {
    let mut order = limit(side, price, volume);
    order.sequence = sequence;
    MatchEngineRequest::Cancel { order }
}

fn market(side: OrderSide, volume: impl Into<Fixed>) -> Order {
    let mut order = limit(side, 0.0, volume);
    order.live = OrderType::Market;
    order
}

fn stop_market(side: OrderSide, price: impl Into<Fixed>, volume: impl Into<Fixed>) -> Order {
    let mut order = market(side, volume);
    order.stop_price = price.into();
    order
}

fn stop_limit(
    side: OrderSide,
    price: impl Into<Fixed>,
    stop_price: impl Into<Fixed>,
    volume: impl Into<Fixed>,
) -> Order {
    let mut order = limit(side, price, volume);
    order.stop_price = stop_price.into();
    order
}

#[test]
fn feature_matrix_reaches_engine() {
    C::run(async {
        let (node, client, _temp) = start_node("127.0.0.1:21909", "feature-matrix").await;

        // GTC limit: rests in full.
        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 2.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        assert_eq!(0, response.events[0].order_sequence);
        assert_eq!(Fixed::from(2), response.events[0].volume);

        // IOC: fills what crosses and cancels the rest.
        let mut ioc = limit(OrderSide::Sell, 100.0, 3.0);
        ioc.tif = TimeInForce::Ioc;
        let response = apply(&client, place(ioc)).await;
        assert_eq!(
            vec![MatchEventKind::Fill, MatchEventKind::Cancelled],
            kinds(&response.events)
        );
        assert_eq!(Some(0), response.events[0].counterparty_sequence);
        assert_eq!(Fixed::from(2), response.events[0].volume);
        assert_eq!(Fixed::from(1), response.events[1].volume);

        // FOK: an unfillable order is rejected; a fillable one executes whole.
        let response = apply(&client, place(limit(OrderSide::Sell, 101.0, 2.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let mut fok = limit(OrderSide::Buy, 101.0, 3.0);
        fok.tif = TimeInForce::Fok;
        let response = apply(&client, place(fok)).await;
        assert_eq!(vec![MatchEventKind::Rejected], kinds(&response.events));
        assert_eq!(Some(RejectReason::FokUnfillable), response.events[0].reason);
        let mut fok = limit(OrderSide::Buy, 101.0, 2.0);
        fok.tif = TimeInForce::Fok;
        let response = apply(&client, place(fok)).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(2), response.events[0].counterparty_sequence);

        // Market: fills against the book, or cancels when there is no liquidity.
        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(market(OrderSide::Sell, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(5), response.events[0].counterparty_sequence);
        let response = apply(&client, place(market(OrderSide::Sell, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(Fixed::from(5), response.events[0].volume);

        // Post-only: rests when passive, rejects when it would cross.
        let mut post_only = limit(OrderSide::Buy, 100.0, 1.0);
        post_only.post_only = true;
        let response = apply(&client, place(post_only)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let mut post_only = limit(OrderSide::Sell, 100.0, 1.0);
        post_only.post_only = true;
        let response = apply(&client, place(post_only)).await;
        assert_eq!(vec![MatchEventKind::Rejected], kinds(&response.events));
        assert_eq!(
            Some(RejectReason::PostOnlyWouldCross),
            response.events[0].reason
        );
        let response = apply(&client, cancel(OrderSide::Buy, 100.0, 1.0, 8)).await;
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(8, response.events[0].order_sequence);

        // Iceberg: rests only its visible tranche and refills it.
        let mut iceberg = limit(OrderSide::Sell, 102.0, 6.0);
        iceberg.display_volume = Fixed::from(2);
        let response = apply(&client, place(iceberg)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        assert_eq!(Fixed::from(2), response.events[0].volume);
        let response = apply(&client, place(limit(OrderSide::Buy, 102.0, 3.0))).await;
        assert_eq!(
            vec![MatchEventKind::Fill, MatchEventKind::Fill],
            kinds(&response.events)
        );
        assert_eq!(Some(11), response.events[0].counterparty_sequence);
        assert_eq!(Fixed::from(2), response.events[0].volume);
        assert_eq!(Fixed::from(1), response.events[1].volume);
        let response = apply(&client, cancel(OrderSide::Sell, 102.0, 6.0, 11)).await;
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(Fixed::from(3), response.events[0].volume);

        // GTD: rests until the explicit deadline purge.
        let mut gtd = limit(OrderSide::Buy, 99.0, 1.0);
        gtd.tif = TimeInForce::Gtd(4_102_444_800_000);
        let response = apply(&client, place(gtd)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 4_102_444_799_999,
                day_close_ms: None,
            },
        )
        .await;
        assert!(response.events.is_empty());
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 4_102_444_800_000,
                day_close_ms: None,
            },
        )
        .await;
        assert_eq!(vec![MatchEventKind::Expired], kinds(&response.events));
        assert_eq!(14, response.events[0].order_sequence);
        assert_eq!(Fixed::from(1), response.events[0].volume);

        // Day: survives a purge without a close, expires when one is supplied.
        let mut day = limit(OrderSide::Buy, 98.0, 1.0);
        day.tif = TimeInForce::Day;
        let response = apply(&client, place(day)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 1_000,
                day_close_ms: None,
            },
        )
        .await;
        assert!(response.events.is_empty());
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 1_000,
                day_close_ms: Some(1_000),
            },
        )
        .await;
        assert_eq!(vec![MatchEventKind::Expired], kinds(&response.events));
        assert_eq!(17, response.events[0].order_sequence);

        // Stop-market: parks silently, then triggers a child that fills and
        // cancels its remainder.
        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Buy, 95.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Buy, 94.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(stop_market(OrderSide::Sell, 95.0, 10.0))).await;
        assert!(response.events.is_empty(), "parking emits no event");
        let response = apply(&client, place(limit(OrderSide::Sell, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(20), response.events[0].counterparty_sequence);
        let response = apply(&client, place(limit(OrderSide::Sell, 95.0, 3.0))).await;
        assert_eq!(
            vec![
                MatchEventKind::Fill,
                MatchEventKind::Triggered,
                MatchEventKind::Fill,
                MatchEventKind::Fill,
                MatchEventKind::Cancelled,
            ],
            kinds(&response.events)
        );
        assert_eq!(25, response.events[0].order_sequence);
        assert_eq!(Some(21), response.events[0].counterparty_sequence);
        assert_eq!(23, response.events[1].order_sequence);
        assert_eq!(Fixed::from(95), response.events[1].price);
        assert_eq!(Fixed::from(10), response.events[1].volume);
        assert_eq!(Some(21), response.events[2].counterparty_sequence);
        assert_eq!(Fixed::from(2), response.events[2].volume);
        assert_eq!(Some(22), response.events[3].counterparty_sequence);
        assert_eq!(Fixed::from(5), response.events[3].volume);
        assert_eq!(Fixed::from(3), response.events[4].volume);

        // Stop-limit: the child is bounded by its limit price.
        let response = apply(&client, place(limit(OrderSide::Buy, 110.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Buy, 109.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Sell, 110.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(26), response.events[0].counterparty_sequence);
        let response = apply(
            &client,
            place(stop_limit(OrderSide::Sell, 108.0, 109.0, 5.0)),
        )
        .await;
        assert!(response.events.is_empty(), "parking emits no event");
        let response = apply(&client, place(limit(OrderSide::Sell, 109.0, 2.0))).await;
        assert_eq!(
            vec![
                MatchEventKind::Fill,
                MatchEventKind::Triggered,
                MatchEventKind::Fill,
                MatchEventKind::Cancelled,
            ],
            kinds(&response.events)
        );
        assert_eq!(Some(27), response.events[0].counterparty_sequence);
        assert_eq!(Fixed::from(2), response.events[0].volume);
        assert_eq!(29, response.events[1].order_sequence);
        assert_eq!(Fixed::from(109), response.events[1].price);
        assert_eq!(Fixed::from(5), response.events[1].volume);
        assert_eq!(Some(27), response.events[2].counterparty_sequence);
        assert_eq!(Fixed::from(3), response.events[2].volume);
        assert_eq!(Fixed::from(2), response.events[3].volume);

        // Amend and cancel keep explicit identities.
        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let mut amend_key = limit(OrderSide::Buy, 100.0, 5.0);
        amend_key.sequence = 31;
        let response = apply(
            &client,
            MatchEngineRequest::Amend {
                order: amend_key,
                price: None,
                volume: Some(Fixed::from(3)),
            },
        )
        .await;
        assert_eq!(vec![MatchEventKind::Amended], kinds(&response.events));
        assert_eq!(31, response.events[0].order_sequence);
        assert_eq!(Some(31), response.events[0].previous_sequence);
        assert_eq!(Fixed::from(3), response.events[0].volume);
        let response = apply(&client, cancel(OrderSide::Buy, 100.0, 3.0, 31)).await;
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(31, response.events[0].order_sequence);
        assert_eq!(Fixed::from(3), response.events[0].volume);

        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Sell, 100.0, 2.0))).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(34), response.events[0].counterparty_sequence);
        let response = apply(&client, cancel(OrderSide::Buy, 100.0, 3.0, 34)).await;
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(34, response.events[0].order_sequence);
        assert_eq!(Fixed::from(3), response.events[0].volume);

        node.shutdown().await.unwrap();
    });
}

#[test]
fn event_lifecycle_reconstructs_state() {
    C::run(async {
        let (node, client, _temp) = start_node("127.0.0.1:21910", "event-lifecycle").await;

        let mut lifecycle = Vec::new();

        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        lifecycle.extend(response.events.iter().copied());
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        assert_eq!(Fixed::from(5), response.events[0].volume);

        let response = apply(
            &client,
            MatchEngineRequest::Amend {
                order: limit(OrderSide::Buy, 100.0, 5.0),
                price: None,
                volume: Some(Fixed::from(3)),
            },
        )
        .await;
        lifecycle.extend(response.events.iter().copied());
        assert_eq!(vec![MatchEventKind::Amended], kinds(&response.events));
        assert_eq!(0, response.events[0].order_sequence);
        assert_eq!(Some(0), response.events[0].previous_sequence);
        assert_eq!(Fixed::from(3), response.events[0].volume);

        let response = apply(&client, place(limit(OrderSide::Sell, 100.0, 2.0))).await;
        lifecycle.extend(response.events.iter().copied());
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        assert_eq!(Some(0), response.events[0].counterparty_sequence);
        assert_eq!(Fixed::from(2), response.events[0].volume);

        let response = apply(&client, cancel(OrderSide::Buy, 100.0, 3.0, 0)).await;
        lifecycle.extend(response.events.iter().copied());
        assert_eq!(vec![MatchEventKind::Cancelled], kinds(&response.events));
        assert_eq!(0, response.events[0].order_sequence);
        assert_eq!(Fixed::from(1), response.events[0].volume);

        assert_eq!(
            vec![
                MatchEventKind::Rested,
                MatchEventKind::Amended,
                MatchEventKind::Fill,
                MatchEventKind::Cancelled,
            ],
            kinds(&lifecycle)
        );
        assert!(
            lifecycle
                .iter()
                .all(|event| event.kind != MatchEventKind::Rejected),
            "accepted lifecycle must not use a rejection"
        );

        // GTD and Day expiry are explicit expiry events.
        let mut gtd = limit(OrderSide::Sell, 101.0, 1.0);
        gtd.tif = TimeInForce::Gtd(4_102_444_800_000);
        let response = apply(&client, place(gtd)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 4_102_444_800_000,
                day_close_ms: None,
            },
        )
        .await;
        assert_eq!(vec![MatchEventKind::Expired], kinds(&response.events));
        assert_eq!(4, response.events[0].order_sequence);
        assert_eq!(Fixed::from(1), response.events[0].volume);

        let mut day = limit(OrderSide::Sell, 102.0, 1.0);
        day.tif = TimeInForce::Day;
        let response = apply(&client, place(day)).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(
            &client,
            MatchEngineRequest::PurgeExpired {
                now_ms: 1_000,
                day_close_ms: Some(1_000),
            },
        )
        .await;
        assert_eq!(vec![MatchEventKind::Expired], kinds(&response.events));
        assert_eq!(6, response.events[0].order_sequence);

        // A stop parks silently and then emits Triggered plus its child events.
        let response = apply(&client, place(limit(OrderSide::Buy, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Buy, 95.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Buy, 94.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Rested], kinds(&response.events));
        let response = apply(&client, place(stop_market(OrderSide::Sell, 95.0, 5.0))).await;
        assert!(response.events.is_empty());
        let response = apply(&client, place(limit(OrderSide::Sell, 100.0, 5.0))).await;
        assert_eq!(vec![MatchEventKind::Fill], kinds(&response.events));
        let response = apply(&client, place(limit(OrderSide::Sell, 95.0, 3.0))).await;
        assert_eq!(
            vec![
                MatchEventKind::Fill,
                MatchEventKind::Triggered,
                MatchEventKind::Fill,
                MatchEventKind::Fill,
            ],
            kinds(&response.events)
        );
        assert_eq!(11, response.events[1].order_sequence);
        assert_eq!(11, response.events[2].order_sequence);
        assert_eq!(11, response.events[3].order_sequence);
        assert!(
            response
                .events
                .iter()
                .all(|event| event.kind != MatchEventKind::Rejected),
            "an accepted stop must not be reported as a rejection"
        );

        // An order the last trade already crosses is the one legitimate stop
        // rejection; it is distinct from lifecycle events.
        let response = apply(&client, place(stop_market(OrderSide::Sell, 96.0, 1.0))).await;
        assert_eq!(vec![MatchEventKind::Rejected], kinds(&response.events));
        assert_eq!(
            Some(RejectReason::StopWouldTrigger),
            response.events[0].reason
        );

        node.shutdown().await.unwrap();
    });
}

#[test]
fn negative_api_inputs_are_rejected() {
    C::run(async {
        let (node, client, _temp) = start_node("127.0.0.1:21911", "negative-api").await;

        assert_eq!(
            "",
            client.read(&"does_not_exist".to_string()).await.unwrap()
        );

        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let write = "http://127.0.0.1:21911/write";
        let read = "http://127.0.0.1:21911/read";

        let response = http.post(read).json(&123).send().await.unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let bare_order = serde_json::json!({
            "live": "Limit",
            "side": "Buy",
            "price": 100.0,
            "volume": 1.0,
            "id": 1,
            "sequence": 0
        });
        let response = http.post(write).json(&bare_order).send().await.unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let missing_price = serde_json::json!({
            "Place": {"order": {
                "live": "Limit",
                "side": "Buy",
                "volume": 1.0,
                "id": 1,
                "sequence": 0
            }}
        });
        let response = http.post(write).json(&missing_price).send().await.unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let unknown_tif = serde_json::json!({
            "Place": {"order": {
                "live": "Limit",
                "side": "Buy",
                "price": 100.0,
                "volume": 1.0,
                "tif": "XYZ",
                "id": 1,
                "sequence": 0
            }}
        });
        let response = http.post(write).json(&unknown_tif).send().await.unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, response.status());

        let valid = serde_json::json!({
            "Place": {"order": {
                "live": "Limit",
                "side": "Buy",
                "price": 100.0,
                "volume": 1.0,
                "id": 1,
                "sequence": 0
            }}
        });
        let response = http.post(write).json(&valid).send().await.unwrap();
        assert_eq!(StatusCode::OK, response.status());
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(1, orders.as_array().unwrap().len());

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
