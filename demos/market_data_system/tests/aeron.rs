//! End-to-end Aeron transport test: publish engine wire frames over an embedded
//! media driver and consume them through the service's WebSocket API.

#![cfg(feature = "aeron")]

use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use matchengine_marketdata::AeronSpec;
use matchengine_marketdata::Config;
use matchengine_marketdata::aeron_source;
use matchengine_marketdata::start;
use matchengine_raft::market_data::BboRecord;
use matchengine_raft::market_data::DepthSnapshotRecord;
use matchengine_raft::market_data::LevelUpdate;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_BBO;
use matchengine_raft::market_data::STREAM_DEPTH_DELTA;
use matchengine_raft::market_data::STREAM_DEPTH_SNAPSHOT;
use matchengine_raft::market_data::flags;
use matchengine_raft::market_data::wire;
use matchengine_raft::order_book::DepthLevel;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::OrderSide;
use rusteron_client::Aeron;
use rusteron_client::AeronContext;
use rusteron_client::AeronPublication;
use rusteron_client::cformat;
use rusteron_media_driver::testing::EmbeddedDriver;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}

fn message(stream: u16, flags: u16, seq: u64, body: MessageBody) -> MarketMessage {
    MarketMessage {
        stream,
        flags,
        market_seq: seq,
        sub_seq: 0,
        log_index: seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms: 1_000 + seq,
        symbol_id: 0,
        body,
    }
}

fn snapshot() -> MarketMessage {
    message(
        STREAM_DEPTH_SNAPSHOT,
        flags::SNAPSHOT | flags::RESET,
        0,
        MessageBody::DepthSnapshot(DepthSnapshotRecord {
            bids: vec![DepthLevel {
                price: fixed("1.085"),
                volume: fixed("10"),
                orders: 1,
            }],
            asks: vec![DepthLevel {
                price: fixed("1.086"),
                volume: fixed("5"),
                orders: 1,
            }],
        }),
    )
}

fn bbo() -> MarketMessage {
    message(
        STREAM_BBO,
        0,
        1,
        MessageBody::Bbo(BboRecord {
            last_trade: Some(fixed("1.085")),
            bid: fixed("1.085"),
            bid_volume: fixed("10"),
            ask: fixed("1.086"),
            ask_volume: fixed("5"),
            mid: Some(fixed("1.0855")),
            spread: Some(fixed("0.001")),
        }),
    )
}

fn delta() -> MarketMessage {
    message(
        STREAM_DEPTH_DELTA,
        0,
        2,
        MessageBody::DepthDelta(vec![LevelUpdate {
            side: OrderSide::Buy,
            level: DepthLevel {
                price: fixed("1.084"),
                volume: fixed("3"),
                orders: 1,
            },
        }]),
    )
}

fn publication(aeron: &Aeron, stream: u16) -> AeronPublication {
    let poller = aeron
        .async_add_publication(&cformat!("aeron:ipc"), i32::from(stream))
        .expect("create publication");
    poller
        .poll_blocking(Duration::from_secs(5))
        .expect("resolve publication")
}

/// Waits until a subscriber image is connected to the publication.
fn wait_connected(publication: &AeronPublication) {
    for _ in 0..1_000 {
        if publication.is_connected() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Aeron publication never connected");
}

/// Offers until the subscription image is connected.
fn publish(publication: &AeronPublication, message: &MarketMessage) {
    let payload = wire::encode(message);
    for _ in 0..2_000 {
        match publication.offer(&payload) {
            Ok(_) => return,
            Err(error) if error.is_retryable() => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("Aeron offer failed: {error}"),
        }
    }
    panic!("Aeron publication never connected");
}

async fn next_json(ws: &mut Ws) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("websocket frame timed out")
        .expect("websocket closed")
        .expect("websocket error");
    match frame {
        Message::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aeron_channel_feeds_the_service() {
    let driver = EmbeddedDriver::launch().expect("embedded driver");
    let dir = driver.dir().to_string();

    // The service connects to the same driver through `aeron_dir` (no embedded
    // driver of its own) and subscribes to the seven feed streams.
    let config = Config::for_test("127.0.0.1:0");
    let aeron_spec = AeronSpec {
        channel: "aeron:ipc".to_string(),
        stream_base: 2001,
        dir: Some(dir.clone()),
        embedded: false,
    };
    let source = aeron_source(&aeron_spec).await.expect("Aeron source");
    let spec = config.symbols[0].clone();
    let running = start(config, vec![(spec, source)]).await.expect("service");

    // Publisher client on the same driver.
    let context = AeronContext::new().expect("context");
    context.set_dir(&cformat!("{dir}")).expect("set dir");
    let aeron = Aeron::new(&context).expect("client");
    aeron.start().expect("start");
    let snapshot_publication = publication(&aeron, STREAM_DEPTH_SNAPSHOT);
    let bbo_publication = publication(&aeron, STREAM_BBO);
    let delta_publication = publication(&aeron, STREAM_DEPTH_DELTA);
    wait_connected(&snapshot_publication);
    wait_connected(&bbo_publication);
    wait_connected(&delta_publication);

    // Seed book state, then subscribe and verify snapshot-on-subscribe.
    publish(&snapshot_publication, &snapshot());
    publish(&bbo_publication, &bbo());
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@depth@100ms","eurusd@bookTicker"],"id":1}"#,
    ))
    .await
    .unwrap();
    let mut saw_depth = false;
    let mut saw_bbo = false;
    for _ in 0..4 {
        let frame = next_json(&mut ws).await;
        if frame["e"] == "depthSnapshot" {
            assert_eq!("1.085", frame["bids"][0][0], "{frame}");
            saw_depth = true;
        }
        if frame["b"] == "1.085" && frame["a"].is_string() && frame["e"].is_null() {
            saw_bbo = true;
        }
        if saw_depth && saw_bbo {
            break;
        }
    }
    assert!(saw_depth, "no Aeron depth snapshot reached the socket");
    assert!(saw_bbo, "no Aeron bookTicker reached the socket");

    // A delta published over Aeron is conflated and forwarded.
    publish(&delta_publication, &delta());
    let update = loop {
        let frame = next_json(&mut ws).await;
        if frame["e"] == "depthUpdate" {
            break frame;
        }
    };
    assert!(
        update["b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|level| level[0] == "1.084"),
        "unexpected Aeron depth update: {update}"
    );

    running.shutdown().await;
    drop(driver);
}
