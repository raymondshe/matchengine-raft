//! Replay an orders JSONL file through an in-process node and compare the
//! generated match-result JSONL with an expected file.
//!
//! Orders file: one JSON object per line, either a `MatchEngineRequest` or a
//! bare `Order` (wrapped as `Place`). Blank lines and lines starting with `#`
//! are ignored. `Batch` lines are submitted as one entry; individual requests
//! are grouped by `--batch-size`.
//!
//! The node writes its match-event log to `--actual` (or a temporary file).
//! Comparison ignores `node_id` and, unless `--strict-log-index` is set,
//! `event_id.log_index`; all other event fields and ordinals must match.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use clap::Parser;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::MAX_BATCH_REQUESTS;
use matchengine_raft::order_book::Order;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;

type C = MatchEngineTypeConfig;

#[derive(Parser, Debug)]
#[command(about = "Replay orders JSONL and compare match-result JSONL")]
struct Opt {
    /// Orders JSONL: one MatchEngineRequest or bare Order per line.
    #[arg(long)]
    orders: PathBuf,

    /// Expected match-result JSONL (usually from --match-event-log).
    #[arg(long)]
    expected: Option<PathBuf>,

    /// Write actual match results here; defaults to a temporary file.
    #[arg(long)]
    actual: Option<PathBuf>,

    /// HTTP API address; the gRPC port is this port + 1000.
    #[arg(long, default_value = "127.0.0.1:24201")]
    addr: String,

    /// Requests per /write_batch.
    #[arg(long, default_value_t = MAX_BATCH_REQUESTS)]
    batch_size: usize,

    /// Keep the node data directory and temporary actual file.
    #[arg(long)]
    keep: bool,

    /// Compare event_id.log_index exactly (default: ignore it).
    #[arg(long)]
    strict_log_index: bool,
}

#[derive(Default)]
struct ReplayStats {
    orders: usize,
    batches: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();
    if opt.batch_size == 0 || opt.batch_size > MAX_BATCH_REQUESTS {
        anyhow::bail!("--batch-size must be 1..={MAX_BATCH_REQUESTS}");
    }

    let temp = tempdir("replay-orders");
    let actual = opt
        .actual
        .clone()
        .unwrap_or_else(|| temp.join("actual-events.jsonl"));
    let storage = StorageConfig::new(temp.join("data"))
        .with_instance_prefix("replay-orders")
        .with_snapshot_interval(Duration::ZERO)
        .with_match_event_log(actual.clone());
    let node = build_match_engine_node(1, opt.addr.clone(), default_raft_config(), storage).await?;
    let client = MatchEngineClient::<C>::new(1, opt.addr.clone());
    client.init().await??;
    wait_for_leader(&client).await?;

    let stats = replay_orders(&client, &opt.orders, opt.batch_size).await?;
    C::sleep(Duration::from_millis(50)).await;
    node.shutdown().await?;

    let actual_events = read_events(&actual)?;
    println!(
        "orders: {}, batches: {}, events: {}",
        stats.orders,
        stats.batches,
        actual_events.len()
    );
    println!("actual: {}", actual.display());

    if let Some(expected) = &opt.expected {
        let expected_events = read_events(expected)?;
        compare_events(&expected_events, &actual_events, opt.strict_log_index)?;
        println!("match: OK ({} events)", actual_events.len());
    }

    if !opt.keep && opt.actual.is_none() {
        std::fs::remove_dir_all(&temp).ok();
    }
    Ok(())
}

async fn replay_orders(
    client: &MatchEngineClient<C>,
    path: &Path,
    batch_size: usize,
) -> anyhow::Result<ReplayStats> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut pending = Vec::with_capacity(batch_size);
    let mut stats = ReplayStats::default();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let request = parse_request(trimmed)
            .map_err(|error| anyhow::anyhow!("{}:{}: {error}", path.display(), line_no + 1))?;
        match request {
            MatchEngineRequest::Batch { requests } => {
                if requests
                    .iter()
                    .any(|request| matches!(request, MatchEngineRequest::Batch { .. }))
                {
                    anyhow::bail!(
                        "{}:{}: nested batches are not supported",
                        path.display(),
                        line_no + 1
                    );
                }
                flush_batch(client, &mut pending, &mut stats).await?;
                let count = requests.len();
                let batch = MatchEngineRequest::Batch { requests };
                client.write(&batch).await??;
                stats.orders += count;
                stats.batches += 1;
            }
            request => {
                pending.push(request);
                stats.orders += 1;
                if pending.len() == batch_size {
                    flush_batch(client, &mut pending, &mut stats).await?;
                }
            }
        }
        if stats.orders % 10_000 == 0 && stats.orders > 0 {
            eprintln!("replayed {} orders", stats.orders);
        }
    }
    flush_batch(client, &mut pending, &mut stats).await?;
    Ok(stats)
}

async fn flush_batch(
    client: &MatchEngineClient<C>,
    pending: &mut Vec<MatchEngineRequest>,
    stats: &mut ReplayStats,
) -> anyhow::Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    client.write_batch(pending).await??;
    stats.batches += 1;
    pending.clear();
    Ok(())
}

fn parse_request(line: &str) -> anyhow::Result<MatchEngineRequest> {
    match serde_json::from_str::<MatchEngineRequest>(line) {
        Ok(request) => Ok(request),
        Err(request_error) => match serde_json::from_str::<Order>(line) {
            Ok(order) => Ok(MatchEngineRequest::Place {
                order,
                client_order_id: None,
            }),
            Err(_) => Err(anyhow::anyhow!(
                "not a MatchEngineRequest or Order: {request_error}"
            )),
        },
    }
}

fn read_events(path: &Path) -> anyhow::Result<Vec<(usize, Value)>> {
    let file = File::open(path)?;
    let mut events = Vec::new();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|error| anyhow::anyhow!("{}:{}: {error}", path.display(), line_no + 1))?;
        events.push((line_no + 1, value));
    }
    Ok(events)
}

fn compare_events(
    expected: &[(usize, Value)],
    actual: &[(usize, Value)],
    strict_log_index: bool,
) -> anyhow::Result<()> {
    let expected: Vec<Value> = expected
        .iter()
        .map(|(_, value)| normalize(value.clone(), strict_log_index))
        .collect();
    let actual: Vec<Value> = actual
        .iter()
        .map(|(_, value)| normalize(value.clone(), strict_log_index))
        .collect();

    for index in 0..expected.len().max(actual.len()) {
        match (expected.get(index), actual.get(index)) {
            (Some(expected), Some(actual)) if expected == actual => {}
            (Some(expected), Some(actual)) => anyhow::bail!(
                "event {index} mismatch\n  expected: {expected}\n  actual:   {actual}"
            ),
            (Some(expected), None) => {
                anyhow::bail!("actual log ended at event {index}; expected: {expected}")
            }
            (None, Some(actual)) => {
                anyhow::bail!("actual log has extra event {index}: {actual}")
            }
            (None, None) => break,
        }
    }
    Ok(())
}

fn normalize(mut value: Value, strict_log_index: bool) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("node_id");
        if !strict_log_index
            && let Some(event_id) = object.get_mut("event_id").and_then(Value::as_object_mut)
        {
            event_id.remove("log_index");
        }
    }
    value
}

async fn wait_for_leader(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("node 1 never became leader");
        }
        C::sleep(Duration::from_millis(50)).await;
    }
}

fn tempdir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
