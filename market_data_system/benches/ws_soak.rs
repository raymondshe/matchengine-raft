//! Market-data fan-out and slow-consumer soak driver.
//!
//! Modes:
//!   --mode local   start the reference service in-process on channel sources
//!   --mode remote  drive a deployed service through its WebSocket URL
//!
//! One invocation runs one phase (`steady`, `churn`, `storm`, `slow`,
//! `reconnect`) for `--duration` seconds, samples the service `/metrics` JSON
//! once a second and writes JSONL samples plus a final summary. See
//! `docs/MARKET_DATA_SOAK_DESIGN.md`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
use serde_json::Value;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Result as WsResult;

// `status` requires `--extensions` and is not enabled on the deployed global
// service, so the default mix uses only the Binance-compatible public streams.
// One depth stream per symbol: the raw `/ws` frame does not carry the stream
// key, so mixing two depth speeds would make `u`-contiguity checks meaningless.
const STREAM_PARTS: [&str; 4] = ["bookTicker", "trade", "depth@100ms", "kline_1s"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Steady,
    Churn,
    Storm,
    Slow,
    Reconnect,
}

impl Phase {
    fn parse(value: &str) -> Self {
        match value {
            "steady" => Self::Steady,
            "churn" => Self::Churn,
            "storm" => Self::Storm,
            "slow" => Self::Slow,
            "reconnect" => Self::Reconnect,
            other => panic!("unknown phase {other:?}; use steady|churn|storm|slow|reconnect"),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Churn => "churn",
            Self::Storm => "storm",
            Self::Slow => "slow",
            Self::Reconnect => "reconnect",
        }
    }
}

struct Options {
    mode: String,
    phase: Phase,
    clients: usize,
    streams: usize,
    symbols: Vec<String>,
    duration: u64,
    rate: u64,
    slow_fraction: f64,
    ws_url: String,
    metrics_url: String,
    extra_urls: Vec<String>,
    label: String,
    out: String,
}

fn value_of<I: Iterator<Item = String>>(args: &mut I, arg: &str) -> String {
    args.next()
        .unwrap_or_else(|| panic!("missing value for {arg}"))
}

fn options() -> Options {
    let default_symbols = "eurusd:1:EUR:USD,gbpusd:2:GBP:USD,btcusdt:3:BTC:USDT";
    let mut o = Options {
        mode: "local".to_string(),
        phase: Phase::Steady,
        clients: 64,
        streams: 32,
        symbols: default_symbols.split(',').map(str::to_string).collect(),
        duration: 60,
        rate: 5_000,
        slow_fraction: 0.25,
        ws_url: "http://127.0.0.1:24804/ws".to_string(),
        metrics_url: "http://127.0.0.1:24804/metrics".to_string(),
        extra_urls: Vec::new(),
        label: "local".to_string(),
        out: "dist/soak/local.jsonl".to_string(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => o.mode = value_of(&mut args, &arg),
            "--phase" => o.phase = Phase::parse(&value_of(&mut args, &arg)),
            "--clients" => o.clients = value_of(&mut args, &arg).parse().expect("clients"),
            "--streams" => o.streams = value_of(&mut args, &arg).parse().expect("streams"),
            "--symbols" => {
                o.symbols = value_of(&mut args, &arg)
                    .split(',')
                    .map(str::to_string)
                    .collect()
            }
            "--duration" => o.duration = value_of(&mut args, &arg).parse().expect("duration"),
            "--rate" => o.rate = value_of(&mut args, &arg).parse().expect("rate"),
            "--slow-fraction" => {
                o.slow_fraction = value_of(&mut args, &arg).parse().expect("slow fraction")
            }
            "--ws-url" => o.ws_url = value_of(&mut args, &arg),
            "--metrics-url" => o.metrics_url = value_of(&mut args, &arg),
            "--extra-urls" => {
                let raw = value_of(&mut args, &arg);
                o.extra_urls = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "--label" => o.label = value_of(&mut args, &arg),
            "--out" => o.out = value_of(&mut args, &arg),
            "--bench" => {}
            other => panic!("unknown argument {other:?}"),
        }
    }
    assert!(o.streams > 0 && o.duration > 0 && o.rate > 0);
    assert!((0.0..=1.0).contains(&o.slow_fraction));
    assert!(
        o.mode == "local" || o.mode == "remote",
        "mode must be local or remote"
    );
    o
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn symbol_names(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|entry| entry.split(':').next().unwrap_or(entry).to_string())
        .collect()
}

fn symbol_specs(raw: &[String]) -> Vec<SymbolSpec> {
    raw.iter()
        .map(|entry| {
            let parts: Vec<&str> = entry.split(':').collect();
            assert_eq!(parts.len(), 4, "symbol entry must be name:id:base:quote");
            SymbolSpec {
                symbol: parts[0].to_string(),
                symbol_id: parts[1].parse().expect("symbol id"),
                base_asset: parts[2].to_string(),
                quote_asset: parts[3].to_string(),
            }
        })
        .collect()
}

fn bbo(symbol_id: u32, seq: u64, ts_ms: u64) -> MarketMessage {
    let base = 1_085_000_000_000_000_000i128 + i128::from(symbol_id) * 10_000_000_000_000_000;
    let drift = (seq % 7) as i128 * 1_000_000_000_000;
    let price = Fixed::from_units(base + drift);
    let ask = Fixed::from_units(base + 1_000_000_000_000_000 + drift);
    MarketMessage {
        stream: STREAM_BBO,
        flags: 0,
        market_seq: seq,
        sub_seq: 0,
        log_index: seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms,
        symbol_id,
        body: MessageBody::Bbo(BboRecord {
            last_trade: Some(price),
            bid: price,
            bid_volume: Fixed::from_units(10 * 1_000_000_000_000_000_000),
            ask,
            ask_volume: Fixed::from_units(5 * 1_000_000_000_000_000_000),
            mid: Some(Fixed::from_units(base + 500_000_000_000_000)),
            spread: Some(Fixed::from_units(1_000_000_000_000_000)),
        }),
    }
}

fn stream_names(symbols: &[String], streams: usize, rotation: usize) -> Vec<String> {
    (0..streams)
        .map(|i| {
            let symbol = &symbols[(i + rotation) % symbols.len()];
            format!("{}@{}", symbol, STREAM_PARTS[i % STREAM_PARTS.len()])
        })
        .collect()
}

#[derive(Default)]
struct ClientStats {
    frames: AtomicU64,
    close_1013: AtomicU64,
    close_other: AtomicU64,
    errors: AtomicU64,
    gaps: AtomicU64,
    gap_examples: Mutex<Vec<String>>,
    reconnects: AtomicU64,
}

fn handle_frame(
    frame: Option<WsResult<Message>>,
    stats: &ClientStats,
    last_u: &mut HashMap<String, u64>,
) -> bool {
    match frame {
        Some(Ok(Message::Text(text))) => {
            stats.frames.fetch_add(1, Ordering::Relaxed);
            // Only sequenced depth updates have a contiguous `u`. bookTicker's
            // `u` is an update id for a different stream and must not be mixed
            // into the depth sequence check.
            if let Ok(value) = serde_json::from_str::<Value>(&text)
                && value["e"].as_str() == Some("depthUpdate")
                && let Some(symbol) = value["s"].as_str()
                && let Some(u) = value["u"].as_u64()
            {
                let first = value["U"].as_u64().unwrap_or(u);
                if let Some(prev) = last_u.get(symbol)
                    && first != *prev + 1
                {
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    let mut examples = stats.gap_examples.lock().unwrap();
                    if examples.len() < 5 {
                        examples.push(format!("{symbol}: {prev}+1 != {first} (u={u})"));
                    }
                }
                last_u.insert(symbol.to_string(), u);
            }
            true
        }
        Some(Ok(Message::Close(_))) | None => false,
        Some(Ok(_)) => true,
        Some(Err(_)) => {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

async fn sample_metrics(url: &str) -> Option<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let text = client.get(url).send().await.ok()?.text().await.ok()?;
    serde_json::from_str(&text).ok()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let o = options();

    let (ws_url, metrics_url, producer) = if o.mode == "local" {
        let specs = symbol_specs(&o.symbols);
        let mut config = Config::new("127.0.0.1:0", specs.clone());
        config.max_connections = o.clients + 4;
        config.max_streams = o.streams.max(8);
        config.fast_max_streams = o.streams.max(8);
        config.queue = 1_024;
        config.broadcast_capacity = 4_096;
        config.snapshot_levels = 20;
        config.terminal_conflation_ms = 100;
        config.terminal_max_frames_per_second = 2_000;

        let mut txs = Vec::with_capacity(specs.len());
        let mut sources: Vec<(SymbolSpec, BoxedSource)> = Vec::with_capacity(specs.len());
        for spec in &specs {
            let (tx, source): (_, BoxedSource) = channel_source(8_192);
            txs.push(tx);
            sources.push((spec.clone(), source));
        }
        let service = start(config, sources).await.expect("start local service");
        for (i, (tx, spec)) in txs.iter().zip(specs.iter()).enumerate() {
            tx.send(Ok(bbo(spec.symbol_id, 0, now_ms() as u64 + i as u64)))
                .await
                .expect("seed BBO");
        }
        let producer = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / o.rate as f64));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            for seq in 1..=o.rate * o.duration {
                tick.tick().await;
                let index = seq as usize % txs.len();
                let spec = &specs[index];
                if txs[index]
                    .send(Ok(bbo(spec.symbol_id, seq, now_ms() as u64)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        (
            format!("ws://{}/ws", service.addr),
            format!("http://{}/metrics", service.addr),
            Some(producer),
        )
    } else {
        (o.ws_url.clone(), o.metrics_url.clone(), None)
    };

    let client_stats = Arc::new(ClientStats::default());
    let names_by_symbol = symbol_names(&o.symbols);
    let mut tasks = Vec::with_capacity(o.clients);
    for client_id in 0..o.clients {
        let names = stream_names(
            &names_by_symbol,
            o.streams,
            client_id % names_by_symbol.len(),
        );
        let stats = client_stats.clone();
        let url = ws_url.clone();
        let phase = o.phase;
        let duration = o.duration;
        let slow = o.slow_fraction > 0.0 && (client_id as f64 / o.clients as f64) < o.slow_fraction;
        tasks.push(tokio::spawn(async move {
            let mut ws = match tokio_tungstenite::connect_async(&url).await {
                Ok((ws, _)) => ws,
                Err(_) => {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            let subscribe =
                Message::text(json!({"method":"SUBSCRIBE","params":names,"id":1}).to_string());
            if ws.send(subscribe).await.is_err() {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let started = Instant::now();
            let mut last_u: HashMap<String, u64> = HashMap::new();
            let mut reconnect_at = started + Duration::from_secs(duration / 2);
            let mut churn_at = started + Duration::from_secs(2);
            loop {
                if started.elapsed() >= Duration::from_secs(duration) {
                    break;
                }
                if phase == Phase::Reconnect && Instant::now() >= reconnect_at {
                    let _ = ws.close(None).await;
                    match tokio_tungstenite::connect_async(&url).await {
                        Ok((new_ws, _)) => ws = new_ws,
                        Err(_) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                    let _ = ws
                        .send(Message::text(
                            json!({"method":"SUBSCRIBE","params":names,"id":2}).to_string(),
                        ))
                        .await;
                    stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    // A new connection starts a new snapshot boundary; old
                    // sequence numbers no longer apply.
                    last_u.clear();
                    reconnect_at = Instant::now() + Duration::from_secs(duration);
                    continue;
                }
                if (phase == Phase::Churn || phase == Phase::Storm) && Instant::now() >= churn_at {
                    let key = &names[client_id % names.len()];
                    let method = if phase == Phase::Storm {
                        "SUBSCRIBE"
                    } else {
                        "UNSUBSCRIBE"
                    };
                    let _ = ws
                        .send(Message::text(
                            json!({"method":method,"params":[key],"id":3}).to_string(),
                        ))
                        .await;
                    churn_at = Instant::now()
                        + if phase == Phase::Storm {
                            Duration::from_millis(500)
                        } else {
                            Duration::from_secs(2)
                        };
                }
                if slow {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                        frame = ws.next() => {
                            if !handle_frame(frame, &stats, &mut last_u) {
                                stats.close_other.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                } else if !handle_frame(ws.next().await, &stats, &mut last_u) {
                    stats.close_other.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }));
    }

    if let Some(parent) = Path::new(&o.out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = File::create(&o.out).expect("create soak output");
    let started = Instant::now();
    let ticker = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(ticker);
    while started.elapsed() < Duration::from_secs(o.duration) {
        ticker.as_mut().tick().await;
        let mut extra = serde_json::Map::new();
        for url in &o.extra_urls {
            if let Some(value) = sample_metrics(url).await {
                extra.insert(url.clone(), value);
            }
        }
        let line = json!({
            "ts_ms": now_ms(),
            "run": o.label,
            "phase": o.phase.label(),
            "clients": o.clients,
            "streams_per_client": o.streams,
            "metrics": sample_metrics(&metrics_url).await,
            "extra": extra,
        });
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
    for task in tasks {
        let _ = task.await;
    }
    if let Some(producer) = producer {
        producer.abort();
    }
    let summary = json!({
        "ts_ms": now_ms(),
        "run": o.label,
        "phase": o.phase.label(),
        "summary": {
            "frames": client_stats.frames.load(Ordering::Relaxed),
            "close_1013": client_stats.close_1013.load(Ordering::Relaxed),
            "close_other": client_stats.close_other.load(Ordering::Relaxed),
            "errors": client_stats.errors.load(Ordering::Relaxed),
            "sequence_gaps": client_stats.gaps.load(Ordering::Relaxed),
            "gap_examples": client_stats.gap_examples.lock().unwrap().clone(),
            "reconnects": client_stats.reconnects.load(Ordering::Relaxed),
            "elapsed_s": started.elapsed().as_secs(),
        }
    });
    let _ = writeln!(file, "{summary}");
    println!(
        "ws_soak [{}]: {} frames, {} gaps, {} reconnects, {} errors -> {}",
        o.phase.label(),
        client_stats.frames.load(Ordering::Relaxed),
        client_stats.gaps.load(Ordering::Relaxed),
        client_stats.reconnects.load(Ordering::Relaxed),
        client_stats.errors.load(Ordering::Relaxed),
        o.out
    );
}
