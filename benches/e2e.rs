//! End-to-end write latency: HTTP submit -> Raft commit -> apply -> response.
//!
//! Run with `cargo bench --bench e2e`. This is a single-node benchmark, so it
//! includes HTTP, WAL append/fsync, Raft apply and response serialization but
//! excludes cross-node replication. It also sweeps `/write_batch` sizes to show
//! how one WAL fsync per entry amortizes across many orders. By default the
//! suite runs on the production multi-thread Tokio runtime; `-- current` and
//! `-- both` compare the old current-thread flavor. The order-book-only costs
//! are measured by `cargo bench --bench matchengine`.

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::future::join_all;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

type C = MatchEngineTypeConfig;

const CURRENT_THREAD_ADDR: &str = "127.0.0.1:24001";
const MULTI_THREAD_ADDR: &str = "127.0.0.1:24002";
const SEQUENTIAL_WRITES: usize = 500;
const CONCURRENT_WRITES: usize = 1_000;
/// Batch sizes swept by the sequential batch benchmark.
const BATCH_SIZES: [usize; 5] = [1, 8, 32, 64, 128];
/// Batches submitted per batch size.
const BATCHES_PER_SIZE: usize = 200;
/// In-flight batches in the concurrent batch benchmark.
const CONCURRENT_BATCHES: usize = 100;
/// Orders per concurrent batch.
const CONCURRENT_BATCH_SIZE: usize = 32;

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-bench-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
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

fn write_request(index: usize) -> MatchEngineRequest {
    // Alternating prices never cross, so the benchmark measures commit/apply
    // without sweeping the whole book.
    let (side, price) = if index.is_multiple_of(2) {
        (OrderSide::Buy, 100.0)
    } else {
        (OrderSide::Sell, 101.0)
    };
    MatchEngineRequest::place(Order::new(side, price, 1.0, 0))
}

async fn start_node(
    addr: &str,
    runtime: &str,
) -> anyhow::Result<(MatchEngineNode, MatchEngineClient<C>, PathBuf)> {
    let dir = temp_dir(runtime);
    let storage = StorageConfig::new(dir.clone())
        .with_instance_prefix(format!("e2e-{runtime}"))
        .with_snapshot_interval(Duration::ZERO);
    let node = build_match_engine_node(1, addr.to_string(), default_raft_config(), storage).await?;
    let client = MatchEngineClient::<C>::new(1, addr.to_string());
    client.init().await??;
    wait_for_leader(&client).await?;
    Ok((node, client, dir))
}

async fn sequential_latency(client: &MatchEngineClient<C>, runtime: &str) -> anyhow::Result<()> {
    let mut latencies = Vec::with_capacity(SEQUENTIAL_WRITES);
    let started = Instant::now();
    for index in 0..SEQUENTIAL_WRITES {
        let request = write_request(index);
        let start = Instant::now();
        client.write(&request).await??;
        latencies.push(start.elapsed());
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();

    let mean = elapsed / SEQUENTIAL_WRITES as u32;
    println!("sequential writes ({runtime}): {SEQUENTIAL_WRITES}");
    println!(
        "  throughput: {:>10.0} writes/s",
        SEQUENTIAL_WRITES as f64 / elapsed.as_secs_f64()
    );
    println!("  mean:       {mean:>10?}");
    println!("  p50:        {:>10?}", percentile(&latencies, 0.50));
    println!("  p95:        {:>10?}", percentile(&latencies, 0.95));
    println!("  p99:        {:>10?}", percentile(&latencies, 0.99));
    println!("  max:        {:>10?}", latencies.last().unwrap());
    Ok(())
}

async fn batch_throughput(client: &MatchEngineClient<C>, runtime: &str) -> anyhow::Result<()> {
    println!("sequential batches ({runtime}, HTTP /write_batch, one Raft entry each):");
    println!(
        "  {:>5}  {:>8}  {:>10}  {:>10}  {:>7}  {:>11}  {:>9}  {:>9}",
        "size", "orders", "batches/s", "orders/s", "speedup", "batch mean", "p50", "p99"
    );

    let mut next_order = 0usize;
    let mut baseline_orders_per_sec = None;
    for size in BATCH_SIZES {
        let mut latencies = Vec::with_capacity(BATCHES_PER_SIZE);
        let started = Instant::now();
        for _ in 0..BATCHES_PER_SIZE {
            let requests: Vec<MatchEngineRequest> = (0..size)
                .map(|offset| write_request(next_order + offset))
                .collect();
            next_order += size;
            let start = Instant::now();
            client.write_batch(&requests).await??;
            latencies.push(start.elapsed());
        }
        let elapsed = started.elapsed();
        latencies.sort_unstable();

        let orders = BATCHES_PER_SIZE * size;
        let batches_per_sec = BATCHES_PER_SIZE as f64 / elapsed.as_secs_f64();
        let orders_per_sec = orders as f64 / elapsed.as_secs_f64();
        let baseline = *baseline_orders_per_sec.get_or_insert(orders_per_sec);
        let mean = latencies.iter().sum::<Duration>() / BATCHES_PER_SIZE as u32;

        println!(
            "  {:>5}  {:>8}  {:>10.0}  {:>10.0}  {:>6.2}x  {:>11?}  {:>9?}  {:>9?}",
            size,
            orders,
            batches_per_sec,
            orders_per_sec,
            orders_per_sec / baseline,
            mean,
            percentile(&latencies, 0.50),
            percentile(&latencies, 0.99),
        );
    }
    Ok(())
}

async fn concurrent_batch_throughput(
    client: &MatchEngineClient<C>,
    runtime: &str,
) -> anyhow::Result<()> {
    let batches: Vec<Vec<MatchEngineRequest>> = (0..CONCURRENT_BATCHES)
        .map(|batch| {
            (0..CONCURRENT_BATCH_SIZE)
                .map(|offset| write_request(batch * CONCURRENT_BATCH_SIZE + offset))
                .collect()
        })
        .collect();
    let orders = CONCURRENT_BATCHES * CONCURRENT_BATCH_SIZE;

    let started = Instant::now();
    let responses = join_all(batches.iter().map(|requests| client.write_batch(requests))).await;
    let elapsed = started.elapsed();
    let ok = responses
        .into_iter()
        .filter(|response| matches!(response, Ok(Ok(_))))
        .count();

    println!(
        "concurrent batches ({runtime}): {CONCURRENT_BATCHES} x {CONCURRENT_BATCH_SIZE} orders ({orders} total)"
    );
    println!(
        "  throughput: {:>10.0} orders/s",
        orders as f64 / elapsed.as_secs_f64()
    );
    println!(
        "  batches:    {:>10.0} batches/s",
        CONCURRENT_BATCHES as f64 / elapsed.as_secs_f64()
    );
    println!("  completed:  {ok}/{CONCURRENT_BATCHES} batches");
    Ok(())
}

async fn concurrent_throughput(addr: &str, runtime: &str) -> anyhow::Result<()> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{addr}/write");
    let body = serde_json::to_vec(&write_request(0))?;
    let started = Instant::now();
    let responses = join_all((0..CONCURRENT_WRITES).map(|_| {
        let http = http.clone();
        let url = url.clone();
        let body = body.clone();
        async move {
            http.post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
        }
    }))
    .await;
    let elapsed = started.elapsed();
    let ok = responses
        .into_iter()
        .filter(|response| response.as_ref().is_ok_and(|r| r.status().is_success()))
        .count();
    println!("concurrent writes ({runtime}): {CONCURRENT_WRITES}");
    println!(
        "  throughput: {:>10.0} writes/s",
        CONCURRENT_WRITES as f64 / elapsed.as_secs_f64()
    );
    println!("  completed:  {ok}/{CONCURRENT_WRITES}");
    Ok(())
}

async fn run_suite(runtime: &str, addr: &str) -> anyhow::Result<()> {
    println!("\n=== {runtime} runtime ===");
    let (node, client, dir) = start_node(addr, runtime).await?;
    sequential_latency(&client, runtime).await?;
    batch_throughput(&client, runtime).await?;
    concurrent_throughput(addr, runtime).await?;
    concurrent_batch_throughput(&client, runtime).await?;
    node.shutdown().await?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    println!("end-to-end write benchmark (single node, HTTP + WAL + apply)");
    let selection = std::env::args().nth(1).unwrap_or_default();
    let run_current = matches!(selection.as_str(), "current" | "both");
    let run_multi = selection != "current";

    if run_current {
        let current = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        current.block_on(run_suite("current_thread", CURRENT_THREAD_ADDR))?;
    }
    if run_multi {
        let multi = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        multi.block_on(run_suite("multi_thread", MULTI_THREAD_ADDR))?;
    }
    Ok(())
}
