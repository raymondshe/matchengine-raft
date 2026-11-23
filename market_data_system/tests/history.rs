//! End-to-end historical bars against a real TDengine `taosAdapter`.
//!
//! Skipped unless `TDENGINE_URL` is set (for example
//! `TDENGINE_URL=http://127.0.0.1:16041`). Each run uses a fresh database and
//! drops it afterwards, so it never touches the lab's `market_bars`.

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_marketdata::Config;
use matchengine_marketdata::SymbolSpec;
use matchengine_marketdata::channel_source;
use matchengine_marketdata::history::tdengine::HistoryStore;
use matchengine_marketdata::start;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_TRADE;
use matchengine_raft::market_data::TradeRecord;
use matchengine_raft::order_book::FeeAsset;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchErrors;
use matchengine_raft::order_book::OrderSide;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

fn fixed(text: &str) -> Fixed {
    text.parse().unwrap()
}

fn trade(seq: u64, ts_ms: u64, price: &str, volume: &str) -> MarketMessage {
    MarketMessage {
        stream: STREAM_TRADE,
        flags: 0,
        market_seq: seq,
        sub_seq: 0,
        log_index: seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms,
        symbol_id: 0,
        body: MessageBody::Trade(vec![TradeRecord {
            log_index: seq + 1,
            ordinal: 0,
            ts_ms,
            price: fixed(price),
            volume: fixed(volume),
            quote_volume: fixed("0.200000000000000001"),
            taker_fee: Fixed::ZERO,
            taker_fee_asset: Some(FeeAsset::Quote),
            maker_fee: Fixed::ZERO,
            maker_fee_asset: Some(FeeAsset::Quote),
            error: MatchErrors::NONE,
            taker_side: OrderSide::Buy,
            taker_sequence: seq,
            maker_sequence: 0,
        }]),
    }
}

async fn get_json(addr: &str, path: &str) -> Option<Value> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n")?.1;
    serde_json::from_str(body).ok()
}

async fn wait_for_bars(addr: &str, path: &str, minimum: usize) -> Vec<Value> {
    for _ in 0..200 {
        if let Some(Value::Array(bars)) = get_json(addr, path).await
            && bars.len() >= minimum
        {
            return bars;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no historical bars appeared at {path}");
}

#[tokio::test]
async fn stores_and_serves_exact_klines_from_tdengine() {
    let Ok(url) = std::env::var("TDENGINE_URL") else {
        eprintln!("TDENGINE_URL unset; skipping TDengine integration test");
        return;
    };
    let _ = tracing_subscriber::fmt::try_init();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let database = format!("mdhist_{}_{}", std::process::id(), nanos % 1_000_000_000);

    let spec = SymbolSpec {
        symbol: "eurusd".to_string(),
        symbol_id: 0,
        base_asset: "EUR".to_string(),
        quote_asset: "USD".to_string(),
    };
    let mut config = Config::for_test("127.0.0.1:0");
    config.history.enabled = true;
    config.history.url = url;
    config.history.database = database.clone();
    config.history.auto_create = true;
    config.history.keep_days = 365;
    config.history.replica = 1;
    config.history.vgroups = 1;
    config.history.seal_lag_ms = 1_500;
    config.symbols = vec![spec.clone()];

    let store = HistoryStore::new(&config.history).unwrap();
    let _ = store.drop_database().await;
    let (source_tx, source) = channel_source(32);
    let service = start(config.clone(), vec![(spec, source)]).await.unwrap();
    let addr = service.addr.clone();

    // Use a recent minute so the write is inside TDengine's KEEP window.
    let base_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        / 60_000
        * 60_000;
    source_tx
        .send(Ok(trade(
            1,
            base_ms,
            "1.000000000000000001",
            "0.100000000000000002",
        )))
        .await
        .unwrap();
    source_tx
        .send(Ok(trade(
            2,
            base_ms + 500,
            "1.200000000000000003",
            "0.300000000000000004",
        )))
        .await
        .unwrap();
    source_tx
        .send(Ok(trade(
            3,
            base_ms + 1_000,
            "0.999999999999999999",
            "0.500000000000000005",
        )))
        .await
        .unwrap();

    let path = format!("/klines?symbol=eurusd&interval=1s&startTime={base_ms}&limit=4");
    let bars = wait_for_bars(&addr, &path, 3).await;
    assert_eq!(bars[0][0], base_ms);
    assert_eq!(bars[0][1], "1.000000000000000001");
    assert_eq!(bars[0][4], "1.200000000000000003");
    assert_eq!(bars[0][5], "0.400000000000000006");
    assert_eq!(bars[0][8], 2);
    assert_eq!(bars[1][0], base_ms + 1_000);
    assert_eq!(bars[1][1], "0.999999999999999999");

    let minute_path = format!("/klines?symbol=eurusd&interval=1m&startTime={base_ms}&limit=1");
    let minutes = wait_for_bars(&addr, &minute_path, 1).await;
    assert_eq!(minutes[0][1], "1.000000000000000001");
    assert_eq!(minutes[0][4], "0.999999999999999999");
    assert_eq!(minutes[0][5], "0.900000000000000011");
    assert_eq!(minutes[0][8], 3);

    service.shutdown().await;
    store.drop_database().await.expect("drop test database");
}
