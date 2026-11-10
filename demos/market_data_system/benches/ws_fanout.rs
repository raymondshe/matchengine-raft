//! WebSocket fan-out microbenchmark.
//!
//! Starts the reference market-data service on a channel source, connects N
//! WebSocket clients to `eurusd@bookTicker`, and publishes one BBO update per
//! tick. Reports aggregate frames/s and per-client frames/s. This measures the
//! shared-hub serialize-once + per-connection send cost; it does not include
//! Aeron ingestion.
//!
//! ```text
//! cargo run --manifest-path demos/market_data_system/Cargo.toml \
//!   --release --bench ws_fanout -- --clients 64 --updates 10000
//! ```

use std::time::Duration;
use std::time::Instant;

use futures_util::SinkExt;
use futures_util::StreamExt;
use matchengine_marketdata::BoxedSource;
use matchengine_marketdata::Config;
use matchengine_marketdata::SymbolSpec;
use matchengine_marketdata::channel_source;
use matchengine_marketdata::start;
use matchengine_raft::market_data::BboRecord;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_BBO;
use matchengine_raft::order_book::Fixed;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}

fn bbo(seq: u64) -> MarketMessage {
    MarketMessage {
        stream: STREAM_BBO,
        flags: 0,
        market_seq: seq,
        sub_seq: 0,
        log_index: seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms: 1_000 + seq,
        symbol_id: 0,
        body: MessageBody::Bbo(BboRecord {
            last_trade: Some(fixed("1.085")),
            bid: fixed("1.085"),
            bid_volume: fixed("10"),
            ask: fixed("1.086"),
            ask_volume: fixed("5"),
            mid: Some(fixed("1.0855")),
            spread: Some(fixed("0.001")),
        }),
    }
}

struct Options {
    clients: usize,
    updates: u64,
    rate: u64,
}

fn options() -> Options {
    let mut options = Options {
        clients: 64,
        updates: 10_000,
        rate: 50_000,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| panic!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--clients" => options.clients = value().parse().expect("clients"),
            "--updates" => options.updates = value().parse().expect("updates"),
            "--rate" => options.rate = value().parse().expect("rate"),
            "--bench" => {}
            other => panic!("unknown argument {other:?}; use --clients, --updates, --rate"),
        }
    }
    assert!(options.clients > 0, "clients must be positive");
    assert!(options.updates > 0, "updates must be positive");
    assert!(options.rate > 0, "rate must be positive");
    options
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let options = options();
    let (tx, source): (_, BoxedSource) = channel_source(options.clients + options.updates as usize);
    let mut config = Config::for_test("127.0.0.1:0");
    config.max_connections = options.clients + 2;
    config.queue = 4_096;
    config.broadcast_capacity = 4_096;
    config.snapshot_levels = 20;
    let spec: SymbolSpec = config.symbols[0].clone();
    let running = start(config, vec![(spec, source)])
        .await
        .expect("start service");

    // Seed a cached BBO so subscribe-time snapshots do not race the first tick.
    tx.send(Ok(bbo(0))).await.expect("seed BBO");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (stop_tx, stop_rx) = watch::channel(false);
    let mut clients = Vec::with_capacity(options.clients);
    for _ in 0..options.clients {
        let url = format!("ws://{}/ws", running.addr);
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect websocket");
        ws.send(Message::text(
            r#"{"method":"SUBSCRIBE","params":["eurusd@bookTicker"],"id":1}"#,
        ))
        .await
        .expect("subscribe");
        let stop_rx = stop_rx.clone();
        clients.push(tokio::spawn(async move {
            let mut stop_rx = stop_rx;
            let mut frames = 0u64;
            loop {
                tokio::select! {
                    changed = stop_rx.changed() => {
                        if changed.is_err() || *stop_rx.borrow() {
                            break;
                        }
                    }
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            if serde_json::from_str::<serde_json::Value>(&text)
                                .ok()
                                .and_then(|value| value["u"].as_u64())
                                .is_some_and(|u| u > 1)
                            {
                                frames += 1;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => panic!("websocket error: {error}"),
                        None => break,
                    }
                }
            }
            frames
        }));
    }

    // Let subscriptions attach before starting the timed feed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let period = Duration::from_secs_f64(1.0 / options.rate as f64);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let started = Instant::now();
    for seq in 1..=options.updates {
        tick.tick().await;
        tx.send(Ok(bbo(seq))).await.expect("feed BBO");
    }
    let elapsed = started.elapsed();
    drop(tx);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _ = stop_tx.send(true);

    let mut received = 0u64;
    for client in clients {
        received += client.await.expect("client task");
    }
    let per_client = received as f64 / options.clients as f64;
    let frame_rate = received as f64 / elapsed.as_secs_f64();
    let offered = options.updates as f64 / elapsed.as_secs_f64();
    println!(
        "ws_fanout: {} clients, {} updates, {:?}, offered {:.0}/s",
        options.clients, options.updates, elapsed, offered
    );
    println!("  received {received} frames ({per_client:.1}/client), {frame_rate:.0} frames/s");

    running.shutdown().await;
}
