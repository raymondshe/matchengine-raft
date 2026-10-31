//! A/B benchmark for the P0 state-machine optimizations.
//!
//! Run with `cargo bench --bench p0_bench -- duplicate|read|snapshot`.
//! Copy this file into a baseline checkout to compare the same workload without
//! the P0 changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::stream;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::NoopSink;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::state_machine::StateMachineStore;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::LogId;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryOf;
use openraft::alias::EntryPayloadOf;
use openraft::alias::LogIdOf;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;
use openraft::type_config::TypeConfigExt;
use openraft::vote::RaftLeaderId;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:24101";
const PRELOAD_ORDERS: usize = 100_000;
const BATCH_SIZE: usize = 128;
const EVENT_WINDOW: usize = 262_144;

fn tempdir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-p0-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn log_id(term: u64, index: u64) -> LogIdOf<C> {
    LogId::new(CommittedLeaderIdOf::<C>::new(term, 1), index)
}

fn entry(index: u64, payload: EntryPayloadOf<C>) -> EntryOf<C> {
    Entry {
        log_id: log_id(1, index),
        payload,
    }
}

fn write_request(index: usize) -> MatchEngineRequest {
    let (side, price) = if index.is_multiple_of(2) {
        (OrderSide::Buy, 100.0)
    } else {
        (OrderSide::Sell, 101.0)
    };
    MatchEngineRequest::place(Order::new(side, price, 1.0, 0))
}

fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
}

fn summarize(label: &str, mut latencies: Vec<Duration>) {
    latencies.sort_unstable();
    let total: Duration = latencies.iter().sum();
    let mean = total / latencies.len() as u32;
    println!(
        "{label}: n={} mean={mean:?} p50={:?} p99={:?} max={:?}",
        latencies.len(),
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.99),
        latencies.last().unwrap(),
    );
}

async fn apply_entry(store: &mut StateMachineStore<C>, index: u64, payload: EntryPayloadOf<C>) {
    let item: Result<EntryResponder<C>, std::io::Error> = Ok((entry(index, payload), None));
    let entries = stream::iter(vec![item]);
    store.apply(entries).await.unwrap();
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

async fn start_node(
    label: &str,
) -> anyhow::Result<(MatchEngineNode, MatchEngineClient<C>, PathBuf)> {
    let dir = tempdir(label);
    let storage = StorageConfig::new(dir.clone())
        .with_instance_prefix(format!("p0-{label}"))
        .with_snapshot_interval(Duration::ZERO);
    let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage).await?;
    let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
    client.init().await??;
    wait_for_leader(&client).await?;
    Ok((node, client, dir))
}

async fn preload(client: &MatchEngineClient<C>, orders: usize) -> anyhow::Result<()> {
    let mut next = 0usize;
    while next < orders {
        let size = (orders - next).min(BATCH_SIZE);
        let requests: Vec<MatchEngineRequest> = (0..size)
            .map(|offset| write_request(next + offset))
            .collect();
        client.write_batch(&requests).await??;
        next += size;
    }
    Ok(())
}

async fn duplicate() -> anyhow::Result<()> {
    let dir = tempdir("duplicate");
    let mut store = StateMachineStore::<C>::open_with_sinks_and_retention(
        &dir,
        Arc::new(NoopSink),
        None,
        EVENT_WINDOW,
    )?;

    let started = Instant::now();
    let batches = EVENT_WINDOW / BATCH_SIZE;
    for batch in 0..batches {
        let requests: Vec<MatchEngineRequest> = (0..BATCH_SIZE)
            .map(|offset| write_request(batch * BATCH_SIZE + offset))
            .collect();
        apply_entry(
            &mut store,
            batch as u64 + 1,
            EntryPayload::Normal(MatchEngineRequest::Batch { requests }),
        )
        .await;
    }

    let target_index = batches as u64 + 1;
    apply_entry(
        &mut store,
        target_index,
        EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
            Order::new(OrderSide::Buy, 99.0, 1.0, 0),
            "target",
        )),
    )
    .await;
    println!(
        "duplicate setup: {} events in {:?}",
        batches * BATCH_SIZE + 1,
        started.elapsed()
    );

    let mut latencies = Vec::with_capacity(200);
    for retry in 0..200u64 {
        let started = Instant::now();
        apply_entry(
            &mut store,
            target_index + retry + 1,
            EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Sell, 50.0, 1.0, 0),
                "target",
            )),
        )
        .await;
        latencies.push(started.elapsed());
    }
    summarize("duplicate retry", latencies);
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn read() -> anyhow::Result<()> {
    let (node, client, dir) = start_node("read").await?;
    let started = Instant::now();
    preload(&client, PRELOAD_ORDERS).await?;
    println!(
        "read setup: {PRELOAD_ORDERS} orders in {:?}",
        started.elapsed()
    );

    let writer_count = 8usize;
    let writes_per_writer = 2_000usize;
    let reader_count = 2usize;
    let reads_per_reader = 10usize;

    let mut writers = Vec::with_capacity(writer_count);
    for writer in 0..writer_count {
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        writers.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(writes_per_writer);
            for offset in 0..writes_per_writer {
                let started = Instant::now();
                client
                    .write(&write_request(writer * writes_per_writer + offset))
                    .await??;
                latencies.push(started.elapsed());
            }
            Ok::<_, anyhow::Error>(latencies)
        }));
    }

    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{ADDR}/read");
    let mut readers = Vec::with_capacity(reader_count);
    for _ in 0..reader_count {
        let http = http.clone();
        let url = url.clone();
        readers.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(reads_per_reader);
            let mut bytes = 0usize;
            for _ in 0..reads_per_reader {
                let started = Instant::now();
                let response = http.post(&url).json(&"orderbook_orders").send().await?;
                let body = response.bytes().await?;
                bytes += body.len();
                latencies.push(started.elapsed());
            }
            Ok::<_, anyhow::Error>((latencies, bytes))
        }));
    }

    let mut write_latencies = Vec::with_capacity(writer_count * writes_per_writer);
    for writer in writers {
        write_latencies.extend(writer.await??);
    }
    let mut read_latencies = Vec::with_capacity(reader_count * reads_per_reader);
    let mut read_bytes = 0usize;
    for reader in readers {
        let (latencies, bytes) = reader.await??;
        read_latencies.extend(latencies);
        read_bytes += bytes;
    }
    summarize("writes during large reads", write_latencies);
    summarize("large orderbook reads", read_latencies);
    println!("read bytes: {read_bytes}");

    node.shutdown().await?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn snapshot() -> anyhow::Result<()> {
    let (node, client, dir) = start_node("snapshot").await?;
    let started = Instant::now();
    preload(&client, PRELOAD_ORDERS).await?;
    println!(
        "snapshot setup: {PRELOAD_ORDERS} orders in {:?}",
        started.elapsed()
    );

    let writer_count = 8usize;
    let writes_per_writer = 1_000usize;
    let mut writers = Vec::with_capacity(writer_count);
    for writer in 0..writer_count {
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        writers.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(writes_per_writer);
            for offset in 0..writes_per_writer {
                let started = Instant::now();
                client
                    .write(&write_request(writer * writes_per_writer + offset))
                    .await??;
                latencies.push(started.elapsed());
            }
            Ok::<_, anyhow::Error>(latencies)
        }));
    }

    C::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    node.state_machine.persist_snapshot().await?;
    let snapshot_elapsed = started.elapsed();

    let mut write_latencies = Vec::with_capacity(writer_count * writes_per_writer);
    for writer in writers {
        write_latencies.extend(writer.await??);
    }
    summarize("writes during snapshot", write_latencies);
    println!("snapshot build+write: {snapshot_elapsed:?}");

    node.shutdown().await?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("duplicate") => duplicate().await,
        Some("read") => read().await,
        Some("snapshot") => snapshot().await,
        _ => {
            eprintln!("usage: p0_bench duplicate|read|snapshot");
            Ok(())
        }
    }
}
