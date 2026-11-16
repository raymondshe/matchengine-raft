//! End-to-end WebSocket tests driven by a channel source.

use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use matchengine_marketdata::BoxedSource;
use matchengine_marketdata::Config;
use matchengine_marketdata::RunningService;
use matchengine_marketdata::SymbolSpec;
use matchengine_marketdata::channel_source;
use matchengine_marketdata::start;
use matchengine_raft::market_data::BboRecord;
use matchengine_raft::market_data::DepthSnapshotRecord;
use matchengine_raft::market_data::HEARTBEAT_SUB_SEQ;
use matchengine_raft::market_data::LevelUpdate;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_BBO;
use matchengine_raft::market_data::STREAM_DEPTH_DELTA;
use matchengine_raft::market_data::STREAM_DEPTH_SNAPSHOT;
use matchengine_raft::market_data::STREAM_STATUS;
use matchengine_raft::market_data::STREAM_TRADE;
use matchengine_raft::market_data::StatusRecord;
use matchengine_raft::market_data::TradeRecord;
use matchengine_raft::market_data::flags;
use matchengine_raft::order_book::DepthLevel;
use matchengine_raft::order_book::FeeAsset;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchErrors;
use matchengine_raft::order_book::OrderSide;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}

fn message(
    stream: u16,
    flags: u16,
    market_seq: u64,
    sub_seq: u16,
    ts_ms: u64,
    body: MessageBody,
) -> MarketMessage {
    MarketMessage {
        stream,
        flags,
        market_seq,
        sub_seq,
        log_index: market_seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms,
        symbol_id: 0,
        body,
    }
}

fn heartbeat(seq: u64) -> MarketMessage {
    message(
        STREAM_STATUS,
        0,
        seq,
        HEARTBEAT_SUB_SEQ,
        1_500,
        MessageBody::Status(StatusRecord {
            last_log_index: seq + 1,
            last_ordinal: 0,
            last_market_seq: seq,
            last_sub_seq: 0,
            applied_index: seq + 1,
            book_sequence: seq,
            pending_stops: 0,
        }),
    )
}

fn snapshot(boundary: u64) -> MarketMessage {
    message(
        STREAM_DEPTH_SNAPSHOT,
        flags::SNAPSHOT | flags::RESET,
        boundary,
        0,
        1_000,
        MessageBody::DepthSnapshot(DepthSnapshotRecord {
            bids: vec![DepthLevel {
                price: fixed("1.085"),
                volume: fixed("10"),
                orders: 1,
            }],
            asks: vec![DepthLevel {
                price: fixed("1.086"),
                volume: fixed("5"),
                orders: 2,
            }],
        }),
    )
}

fn bbo(seq: u64) -> MarketMessage {
    message(
        STREAM_BBO,
        0,
        seq,
        0,
        1_100,
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

fn delta(seq: u64, price: &str, volume: &str) -> MarketMessage {
    message(
        STREAM_DEPTH_DELTA,
        0,
        seq,
        0,
        1_200,
        MessageBody::DepthDelta(vec![LevelUpdate {
            side: OrderSide::Buy,
            level: DepthLevel {
                price: fixed(price),
                volume: fixed(volume),
                orders: 1,
            },
        }]),
    )
}

fn trade(seq: u64) -> MarketMessage {
    message(
        STREAM_TRADE,
        0,
        seq,
        0,
        1_300,
        MessageBody::Trade(vec![TradeRecord {
            log_index: seq + 1,
            ordinal: 0,
            ts_ms: 1_300,
            price: fixed("1.085"),
            volume: fixed("4"),
            quote_volume: fixed("4.34"),
            taker_fee: fixed("0.00868"),
            taker_fee_asset: Some(FeeAsset::Quote),
            maker_fee: fixed("0.004"),
            maker_fee_asset: Some(FeeAsset::Base),
            error: MatchErrors::NONE,
            taker_side: OrderSide::Buy,
            taker_sequence: 1,
            maker_sequence: 0,
        }]),
    )
}

async fn start_single(config: Config, source: BoxedSource) -> RunningService {
    let spec = config.symbols[0].clone();
    start(config, vec![(spec, source)]).await.unwrap()
}

async fn next_json(ws: &mut Ws) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("websocket frame timed out")
        .expect("websocket closed")
        .expect("websocket error");
    match frame {
        Message::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

async fn wait_ready(addr: &str) {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(addr).await {
            let _ = stream
                .write_all(b"GET /ready HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                .await;
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response).await;
            if String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200") {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("service did not become ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_snapshots_streams_and_conflated_depth() {
    let config = Config::for_test("127.0.0.1:0");
    let (tx, source) = channel_source(64);
    tx.send(Ok(snapshot(0))).await.unwrap();
    tx.send(Ok(bbo(1))).await.unwrap();
    let running = start_single(config, source).await;
    wait_ready(&running.addr).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@bookTicker","eurusd@depth@100ms","eurusd@trade"],"id":7}"#,
    ))
    .await
    .unwrap();

    let mut frames = Vec::new();
    for _ in 0..3 {
        frames.push(next_json(&mut ws).await);
    }
    assert!(
        frames
            .iter()
            .any(|frame| frame["result"].is_null() && frame["id"] == 7),
        "missing subscription result: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| frame["b"] == "1.085"
            && frame["a"].is_string()
            && frame["u"].is_number()
            && frame["e"].is_null()),
        "missing bookTicker snapshot: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame["e"] == "depthSnapshot" && frame["bids"][0][0] == "1.085"),
        "missing depth snapshot: {frames:?}"
    );

    // A depth delta is conflated into one depthUpdate within ~100 ms.
    tx.send(Ok(delta(2, "1.084", "3"))).await.unwrap();
    let update = loop {
        let frame = next_json(&mut ws).await;
        if frame["e"] == "depthUpdate" {
            break frame;
        }
    };
    assert_eq!("eurusd", update["s"]);
    assert!(update["u"].as_u64().unwrap() >= update["U"].as_u64().unwrap());
    assert!(
        update["b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|level| level[0] == "1.084" && level[1] == "3"),
        "unexpected depth levels: {update}"
    );

    // Trades use the public Binance spot trade shape: no fees or execution
    // details, just the fill and the buyer-maker flag.
    tx.send(Ok(trade(3))).await.unwrap();
    let fill = loop {
        let frame = next_json(&mut ws).await;
        if frame["e"] == "trade" {
            break frame;
        }
    };
    assert_eq!("1.085", fill["p"]);
    assert_eq!("4", fill["q"]);
    assert_eq!(false, fill["m"]);
    assert!(fill["t"].as_u64().unwrap() > 0, "missing trade id: {fill}");
    assert!(fill["tf"].is_null(), "fee must not be public: {fill}");
    assert!(
        fill["S"].is_null(),
        "internal symbol id must not be public: {fill}"
    );

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combined_streams_wrap_payloads() {
    let config = Config::for_test("127.0.0.1:0");
    let (tx, source) = channel_source(64);
    let running = start_single(config, source).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/stream?streams=eurusd@bookTicker",
        running.addr
    ))
    .await
    .expect("connect");
    tx.send(Ok(bbo(2))).await.unwrap();
    let frame = loop {
        let frame = next_json(&mut ws).await;
        if frame["stream"].is_string() {
            break frame;
        }
    };
    assert_eq!("eurusd@bookTicker", frame["stream"]);
    assert_eq!("1.085", frame["data"]["b"]);
    assert_eq!("1.086", frame["data"]["a"]);

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_streams_are_rejected() {
    let config = Config::for_test("127.0.0.1:0");
    let (_tx, source) = channel_source(8);
    let running = start_single(config, source).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@nope"],"id":9}"#,
    ))
    .await
    .unwrap();
    let frame = next_json(&mut ws).await;
    assert_eq!(0, frame["error"]["code"]);
    assert_eq!(9, frame["id"]);
    running.shutdown().await;
}

async fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("http connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_the_test_page_and_info() {
    let config = Config::for_test("127.0.0.1:0");
    let (_tx, source) = channel_source(8);
    let running = start_single(config, source).await;

    let (status, page) = http_get(&running.addr, "/").await;
    assert_eq!(200, status);
    assert!(
        page.contains("WebSocket test page"),
        "unexpected page: {page}"
    );
    assert!(
        page.contains("/lightweight-charts.js") && page.contains("kline-chart"),
        "page is missing the kline chart: {page}"
    );

    let (status, js) = http_get(&running.addr, "/app.js").await;
    assert_eq!(200, status);
    assert!(js.contains("new WebSocket"));
    assert!(
        js.contains("CandlestickSeries"),
        "kline chart wiring missing"
    );

    let (status, chart) = http_get(&running.addr, "/lightweight-charts.js").await;
    assert_eq!(200, status);
    assert!(
        chart.contains("TradingView Lightweight Charts"),
        "unexpected chart bundle"
    );

    let (status, css) = http_get(&running.addr, "/app.css").await;
    assert_eq!(200, status);
    assert!(css.contains(".topbar"));

    let (status, info) = http_get(&running.addr, "/info").await;
    assert_eq!(200, status);
    let info: Value = serde_json::from_str(&info).expect("info json");
    assert_eq!("eurusd", info["symbols"][0]["symbol"]);
    assert_eq!(true, info["extensions"]);
    let streams = info["streams"].as_array().expect("stream list");
    for expected in ["trade", "bookTicker", "depth@100ms", "kline_1s", "sweep"] {
        assert!(
            streams.iter().any(|stream| stream == expected),
            "missing {expected} in {streams:?}"
        );
    }
    assert_eq!(0, info["symbols"][0]["symbol_id"]);
    assert_eq!("EUR", info["symbols"][0]["base_asset"]);
    assert_eq!("USD", info["symbols"][0]["quote_asset"]);
    assert!(
        info["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "kline_1s")
    );

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_info_hides_extension_streams() {
    let mut config = Config::for_test("127.0.0.1:0");
    config.extensions = false;
    config.lifecycle = false;
    let (_tx, source) = channel_source(8);
    let running = start_single(config, source).await;

    let (status, info) = http_get(&running.addr, "/info").await;
    assert_eq!(200, status);
    let info: Value = serde_json::from_str(&info).expect("info json");
    assert_eq!(false, info["extensions"]);
    let streams = info["streams"].as_array().expect("stream list");
    assert!(streams.iter().any(|stream| stream == "trade"));
    assert!(streams.iter().any(|stream| stream == "bookTicker"));
    for hidden in ["sweep", "status", "lifecycle"] {
        assert!(
            !streams.iter().any(|stream| stream == hidden),
            "public catalogue must not expose {hidden}: {streams:?}"
        );
    }

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_connection_switches_between_two_symbols() {
    let mut config = Config::for_test("127.0.0.1:0");
    config.symbols.push(SymbolSpec {
        symbol: "btcusdt".to_string(),
        symbol_id: 1,
        base_asset: "BTC".to_string(),
        quote_asset: "USDT".to_string(),
    });
    let (eur_tx, eur_source) = channel_source(16);
    let (btc_tx, btc_source) = channel_source(16);
    let running = start(
        config.clone(),
        vec![
            (config.symbols[0].clone(), eur_source),
            (config.symbols[1].clone(), btc_source),
        ],
    )
    .await
    .unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@bookTicker"],"id":1}"#,
    ))
    .await
    .unwrap();
    // Only the control result is sent: there is no BBO snapshot yet.
    assert_eq!(1, wait_result(&mut ws, 1).await["id"]);

    eur_tx.send(Ok(bbo(2))).await.unwrap();
    let eur = wait_symbol_frame(&mut ws, "eurusd").await;
    assert_eq!("eurusd", eur["s"]);

    // Switch symbols on the same socket: unsubscribe the old topics, subscribe
    // the new ones. No reconnect.
    ws.send(Message::text(
        r#"{"method":"UNSUBSCRIBE","params":["eurusd@bookTicker"],"id":2}"#,
    ))
    .await
    .unwrap();
    assert_eq!(2, wait_result(&mut ws, 2).await["id"]);
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["btcusdt@bookTicker"],"id":3}"#,
    ))
    .await
    .unwrap();
    assert_eq!(3, wait_result(&mut ws, 3).await["id"]);

    let mut btc = bbo(3);
    btc.symbol_id = 1;
    btc_tx.send(Ok(btc)).await.unwrap();
    let frame = wait_symbol_frame(&mut ws, "btcusdt").await;
    assert_eq!("btcusdt", frame["s"]);

    // Binance-style LIST_SUBSCRIPTIONS reflects the switch on the same socket.
    ws.send(Message::text(r#"{"method":"LIST_SUBSCRIPTIONS","id":4}"#))
        .await
        .unwrap();
    let list = wait_result(&mut ws, 4).await;
    assert_eq!(json!(["btcusdt@bookTicker"]), list["result"]);

    // One connection served both symbols; only the new stream remains.
    let (_, metrics) = http_get(&running.addr, "/metrics").await;
    let metrics: Value = serde_json::from_str(&metrics).unwrap();
    assert_eq!(1, metrics["connections"]);
    assert_eq!(1, metrics["streams"]);

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_connection_holds_two_symbols_at_once() {
    let mut config = Config::for_test("127.0.0.1:0");
    config.symbols.push(SymbolSpec {
        symbol: "btcusdt".to_string(),
        symbol_id: 1,
        base_asset: "BTC".to_string(),
        quote_asset: "USDT".to_string(),
    });
    let (eur_tx, eur_source) = channel_source(16);
    let (btc_tx, btc_source) = channel_source(16);
    let running = start(
        config.clone(),
        vec![
            (config.symbols[0].clone(), eur_source),
            (config.symbols[1].clone(), btc_source),
        ],
    )
    .await
    .unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@bookTicker","btcusdt@bookTicker"],"id":1}"#,
    ))
    .await
    .unwrap();
    assert_eq!(1, wait_result(&mut ws, 1).await["id"]);

    eur_tx.send(Ok(bbo(2))).await.unwrap();
    let mut btc = bbo(3);
    btc.symbol_id = 1;
    btc_tx.send(Ok(btc)).await.unwrap();

    let mut seen = std::collections::BTreeMap::new();
    while seen.len() < 2 {
        let frame = next_json(&mut ws).await;
        if let Some(symbol) = frame["s"].as_str() {
            seen.insert(symbol.to_string(), frame);
        }
    }
    assert_eq!("eurusd", seen["eurusd"]["s"]);
    assert_eq!(3, seen["eurusd"]["u"]);
    assert_eq!("btcusdt", seen["btcusdt"]["s"]);
    assert_eq!(4, seen["btcusdt"]["u"]);

    ws.send(Message::text(r#"{"method":"LIST_SUBSCRIPTIONS","id":2}"#))
        .await
        .unwrap();
    let list = wait_result(&mut ws, 2).await;
    assert_eq!(
        json!(["btcusdt@bookTicker", "eurusd@bookTicker"]),
        list["result"]
    );

    let (_, metrics) = http_get(&running.addr, "/metrics").await;
    let metrics: Value = serde_json::from_str(&metrics).unwrap();
    assert_eq!(1, metrics["connections"]);
    assert_eq!(2, metrics["streams"]);

    running.shutdown().await;
}

async fn wait_result(ws: &mut Ws, id: u64) -> Value {
    loop {
        let frame = next_json(ws).await;
        if frame["id"] == id {
            return frame;
        }
    }
}

async fn wait_symbol_frame(ws: &mut Ws, symbol: &str) -> Value {
    loop {
        let frame = next_json(ws).await;
        if frame["s"] == symbol && frame["e"].is_null() {
            return frame;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_requires_a_fresh_frame_or_heartbeat() {
    let mut config = Config::for_test("127.0.0.1:0");
    config.heartbeat_timeout = Duration::from_millis(150);
    let (tx, source) = channel_source(8);
    let running = start_single(config, source).await;

    let (status, _) = http_get(&running.addr, "/ready").await;
    assert_eq!(503, status, "no snapshot yet");

    tx.send(Ok(snapshot(0))).await.unwrap();
    wait_ready(&running.addr).await;

    // Silence beyond the timeout drains the instance.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (status, body) = http_get(&running.addr, "/ready").await;
    assert_eq!(503, status, "stale heartbeat: {body}");
    assert_eq!(
        false,
        serde_json::from_str::<Value>(&body).unwrap()["symbols"]["eurusd"]["heartbeat_fresh"]
    );

    // The next frame (including an idle STATUS heartbeat) restores readiness.
    tx.send(Ok(bbo(1))).await.unwrap();
    wait_ready(&running.addr).await;
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_idle_heartbeats_are_forwarded() {
    let config = Config::for_test("127.0.0.1:0");
    let (tx, source) = channel_source(8);
    let running = start_single(config, source).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", running.addr))
        .await
        .expect("connect");
    ws.send(Message::text(
        r#"{"method":"SUBSCRIBE","params":["eurusd@status"],"id":1}"#,
    ))
    .await
    .unwrap();
    loop {
        let frame = next_json(&mut ws).await;
        if frame["id"] == 1 {
            break;
        }
    }

    // The same `(market_seq, sub_seq)` heartbeat must reach @status each time;
    // the assembler dedupes deterministic tape entries, not liveness frames.
    for _ in 0..2 {
        tx.send(Ok(heartbeat(10))).await.unwrap();
        loop {
            let frame = next_json(&mut ws).await;
            if frame["e"] == "status" {
                assert!(frame["ready"].as_bool().is_some());
                break;
            }
        }
    }
    running.shutdown().await;
}
