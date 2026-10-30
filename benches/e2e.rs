//! End-to-end write latency: HTTP submit -> Raft commit -> apply -> response.
//!
//! Run with `cargo bench --bench e2e`. This is a single-node benchmark, so it
//! includes HTTP, WAL append/fsync, Raft apply and response serialization but
//! excludes cross-node replication. The order-book-only costs are measured by
//! `cargo bench --bench matchengine`.

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

const ADDR: &str = "127.0.0.1:24001";
const SEQUENTIAL_WRITES: usize = 500;
const CONCURRENT_WRITES: usize = 1_000;

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

async fn start_node() -> anyhow::Result<(MatchEngineNode, MatchEngineClient<C>, PathBuf)> {
    let dir = temp_dir("e2e");
    let storage = StorageConfig::new(dir.clone())
        .with_instance_prefix("e2e-bench")
        .with_snapshot_interval(Duration::ZERO);
    let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage).await?;
    let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
    client.init().await??;
    wait_for_leader(&client).await?;
    Ok((node, client, dir))
}

async fn sequential_latency(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
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
    println!("sequential writes: {SEQUENTIAL_WRITES}");
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

async fn concurrent_throughput() -> anyhow::Result<()> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{ADDR}/write");
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
    println!("concurrent writes: {CONCURRENT_WRITES}");
    println!(
        "  throughput: {:>10.0} writes/s",
        CONCURRENT_WRITES as f64 / elapsed.as_secs_f64()
    );
    println!("  completed:  {ok}/{CONCURRENT_WRITES}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (node, client, dir) = start_node().await?;
    println!("end-to-end write benchmark (single node, HTTP + WAL + apply)");
    sequential_latency(&client).await?;
    concurrent_throughput().await?;
    node.shutdown().await?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
