//! Four-node no-fsync cluster end-to-end write benchmark.
//!
//! Starts four in-process voters on loopback with WAL fsync disabled, initializes
//! all four in one `/init`, and measures leader-direct `/write` and
//! `/write_batch` throughput/latency. Every round starts a fresh cluster so the
//! book and WAL do not grow across rounds.
//!
//! Run with:
//!
//! ```text
//! cargo bench --bench cluster_e2e -- [--rounds 3]
//! ```
//!
//! Scope: all four nodes run in one process and share the host CPU, but each
//! node has its own storage, HTTP listener and gRPC listener. The measured path
//! includes HTTP, Raft quorum replication over gRPC, WAL append (no fsync),
//! state-machine apply and response serialization.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use anyhow::bail;
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

const NODE_COUNT: u64 = 4;
const FIRST_HTTP_PORT: u16 = 25101;
const SEQUENTIAL_WRITES: usize = 500;
const CONCURRENT_WRITES: usize = 1_000;
const WARMUP_WRITES: usize = 50;
const BATCH_SIZES: [usize; 5] = [1, 8, 32, 64, 128];
const BATCHES_PER_SIZE: usize = 200;
const CONCURRENT_BATCHES: usize = 100;
const CONCURRENT_BATCH_SIZE: usize = 32;

#[derive(Clone, Copy)]
struct Stats {
    throughput: f64,
    p50: Duration,
    p95: Duration,
    p99: Duration,
}

#[derive(Clone, Copy)]
struct BatchRow {
    size: usize,
    orders_per_sec: f64,
    p50: Duration,
    p99: Duration,
}

#[derive(Clone, Copy)]
struct RoundSummary {
    sequential: Stats,
    concurrent_writes: f64,
    batch128: BatchRow,
    concurrent_batches: f64,
}

struct Cluster {
    nodes: Vec<MatchEngineNode>,
    clients: Vec<Arc<MatchEngineClient<C>>>,
    addrs: Vec<(u64, String)>,
    root: PathBuf,
}

fn temp_dir(label: &str) -> PathBuf {
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

fn http_addr(id: u64) -> String {
    format!("127.0.0.1:{}", FIRST_HTTP_PORT + (id - 1) as u16)
}

fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
}

fn write_request(index: usize) -> MatchEngineRequest {
    // Alternating prices never cross, so the benchmark measures commit/apply
    // without sweeping the book.
    let (side, price) = if index.is_multiple_of(2) {
        (OrderSide::Buy, 100.0)
    } else {
        (OrderSide::Sell, 101.0)
    };
    MatchEngineRequest::place(Order::new(side, price, 1.0, 0))
}

impl Cluster {
    async fn start(round: usize) -> anyhow::Result<Self> {
        let root = temp_dir(&format!("cluster-e2e-r{round}"));
        let mut nodes = Vec::with_capacity(NODE_COUNT as usize);
        let mut clients = Vec::with_capacity(NODE_COUNT as usize);
        let mut addrs = Vec::with_capacity(NODE_COUNT as usize);

        for id in 1..=NODE_COUNT {
            let addr = http_addr(id);
            let storage = StorageConfig::new(root.clone())
                .with_instance_prefix("cluster-e2e")
                .with_snapshot_interval(Duration::ZERO)
                .with_wal_fsync(None)
                .with_min_voters(NODE_COUNT as usize);
            let node =
                build_match_engine_node(id, addr.clone(), default_raft_config(), storage).await?;
            nodes.push(node);
            clients.push(Arc::new(MatchEngineClient::new(id, addr.clone())));
            addrs.push((id, addr));
        }

        Ok(Self {
            nodes,
            clients,
            addrs,
            root,
        })
    }

    /// Initializes all four voters in one request and waits until every node
    /// reports the same leader and applied index. Returns the leader index.
    async fn initialize(&self) -> anyhow::Result<usize> {
        let http = reqwest::Client::builder().no_proxy().build()?;
        let url = format!("http://{}/init", self.addrs[0].1);
        let body = serde_json::to_vec(&self.addrs)?;
        let response = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if !status.is_success() || value.get("Ok").is_none() {
            bail!("cluster init failed ({status}): {text}");
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = String::new();
        loop {
            let mut leaders = Vec::with_capacity(NODE_COUNT as usize);
            let mut applied = Vec::with_capacity(NODE_COUNT as usize);
            for client in &self.clients {
                match client.metrics().await {
                    Ok(metrics) => {
                        leaders.push(metrics.current_leader);
                        applied.push(metrics.last_applied.map(|log_id| log_id.index));
                    }
                    Err(error) => {
                        leaders.push(None);
                        applied.push(None);
                        last = format!("metrics error: {error}");
                    }
                }
            }

            let stable_leader =
                leaders[0].is_some() && leaders.iter().all(|leader| *leader == leaders[0]);
            let caught_up =
                applied[0].is_some() && applied.iter().all(|index| *index == applied[0]);
            if stable_leader && caught_up {
                let leader_id = leaders[0].expect("checked above");
                let index = (leader_id - 1) as usize;
                if index < self.clients.len() {
                    return Ok(index);
                }
            }

            if Instant::now() >= deadline {
                bail!("cluster did not converge: leaders={leaders:?}, applied={applied:?}, {last}");
            }
            C::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        let Self { nodes, root, .. } = self;
        for node in nodes {
            node.shutdown().await?;
        }
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}

async fn sequential_latency(client: &MatchEngineClient<C>) -> anyhow::Result<Stats> {
    let mut latencies = Vec::with_capacity(SEQUENTIAL_WRITES);
    let started = Instant::now();
    for index in 0..SEQUENTIAL_WRITES {
        let start = Instant::now();
        client.write(&write_request(index)).await??;
        latencies.push(start.elapsed());
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();

    let throughput = SEQUENTIAL_WRITES as f64 / elapsed.as_secs_f64();
    let mean = elapsed / SEQUENTIAL_WRITES as u32;
    println!("sequential /write: {SEQUENTIAL_WRITES}");
    println!("  throughput: {throughput:>10.0} writes/s");
    println!("  mean:       {mean:>10?}");
    println!("  p50:        {:>10?}", percentile(&latencies, 0.50));
    println!("  p95:        {:>10?}", percentile(&latencies, 0.95));
    println!("  p99:        {:>10?}", percentile(&latencies, 0.99));
    println!("  max:        {:>10?}", latencies.last().unwrap());

    Ok(Stats {
        throughput,
        p50: percentile(&latencies, 0.50),
        p95: percentile(&latencies, 0.95),
        p99: percentile(&latencies, 0.99),
    })
}

async fn batch_throughput(client: &MatchEngineClient<C>) -> anyhow::Result<Vec<BatchRow>> {
    println!("sequential /write_batch (one Raft entry per batch):");
    println!(
        "  {:>5}  {:>8}  {:>10}  {:>7}  {:>11}  {:>9}  {:>9}",
        "size", "orders", "orders/s", "speedup", "batch mean", "p50", "p99"
    );

    let mut rows = Vec::with_capacity(BATCH_SIZES.len());
    let mut next_order = 0usize;
    let mut baseline: Option<f64> = None;
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
        let orders_per_sec = orders as f64 / elapsed.as_secs_f64();
        let baseline_value = *baseline.get_or_insert(orders_per_sec);
        let mean = latencies.iter().sum::<Duration>() / BATCHES_PER_SIZE as u32;
        let row = BatchRow {
            size,
            orders_per_sec,
            p50: percentile(&latencies, 0.50),
            p99: percentile(&latencies, 0.99),
        };
        println!(
            "  {:>5}  {:>8}  {:>10.0}  {:>6.2}x  {:>11?}  {:>9?}  {:>9?}",
            size,
            orders,
            orders_per_sec,
            orders_per_sec / baseline_value,
            mean,
            row.p50,
            row.p99,
        );
        rows.push(row);
    }
    Ok(rows)
}

async fn concurrent_throughput(addr: &str) -> anyhow::Result<f64> {
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
        .iter()
        .filter(|response| {
            response
                .as_ref()
                .is_ok_and(|response| response.status().is_success())
        })
        .count();
    let throughput = CONCURRENT_WRITES as f64 / elapsed.as_secs_f64();
    println!("concurrent /write: {CONCURRENT_WRITES} in flight");
    println!("  throughput: {throughput:>10.0} writes/s");
    println!("  completed:  {ok}/{CONCURRENT_WRITES}");
    if ok != CONCURRENT_WRITES {
        bail!("only {ok}/{CONCURRENT_WRITES} concurrent writes succeeded");
    }
    Ok(throughput)
}

async fn concurrent_batch_throughput(client: &MatchEngineClient<C>) -> anyhow::Result<f64> {
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
        .iter()
        .filter(|response| response.as_ref().is_ok_and(|inner| inner.is_ok()))
        .count();
    let throughput = orders as f64 / elapsed.as_secs_f64();
    println!(
        "concurrent /write_batch: {CONCURRENT_BATCHES} x {CONCURRENT_BATCH_SIZE} orders ({orders} total)"
    );
    println!("  throughput: {throughput:>10.0} orders/s");
    println!(
        "  batches:    {:>10.0} batches/s",
        CONCURRENT_BATCHES as f64 / elapsed.as_secs_f64()
    );
    println!("  completed:  {ok}/{CONCURRENT_BATCHES} batches");
    if ok != CONCURRENT_BATCHES {
        bail!("only {ok}/{CONCURRENT_BATCHES} concurrent batches succeeded");
    }
    Ok(throughput)
}

async fn run_round(round: usize, rounds: usize) -> anyhow::Result<RoundSummary> {
    let cluster = Cluster::start(round).await?;
    let leader_index = cluster.initialize().await?;
    let leader_addr = cluster.addrs[leader_index].1.clone();
    let leader_id = cluster.addrs[leader_index].0;
    let leader = cluster.clients[leader_index].clone();

    println!("\n=== round {round}/{rounds}: leader node {leader_id} ({leader_addr}) ===");
    for index in 0..WARMUP_WRITES {
        leader.write(&write_request(index)).await??;
    }
    println!("warm-up: {WARMUP_WRITES} writes (excluded)");

    let sequential = sequential_latency(&leader).await?;
    let batches = batch_throughput(&leader).await?;
    let concurrent_writes = concurrent_throughput(&leader_addr).await?;
    let concurrent_batches = concurrent_batch_throughput(&leader).await?;

    cluster.shutdown().await?;
    // Give the listeners a moment to release the fixed loopback ports.
    C::sleep(Duration::from_millis(100)).await;

    let batch128 = batches
        .iter()
        .find(|row| row.size == 128)
        .copied()
        .context("missing 128-order batch row")?;
    Ok(RoundSummary {
        sequential,
        concurrent_writes,
        batch128,
        concurrent_batches,
    })
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn median_duration(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn range_f64(values: &[f64]) -> (f64, f64) {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

fn print_summary(rounds: &[RoundSummary]) {
    let seq_throughput = median_f64(rounds.iter().map(|r| r.sequential.throughput).collect());
    let seq_p50 = median_duration(rounds.iter().map(|r| r.sequential.p50).collect());
    let seq_p95 = median_duration(rounds.iter().map(|r| r.sequential.p95).collect());
    let seq_p99 = median_duration(rounds.iter().map(|r| r.sequential.p99).collect());
    let concurrent_writes = median_f64(rounds.iter().map(|r| r.concurrent_writes).collect());
    let batch128 = median_f64(rounds.iter().map(|r| r.batch128.orders_per_sec).collect());
    let batch128_p50 = median_duration(rounds.iter().map(|r| r.batch128.p50).collect());
    let batch128_p99 = median_duration(rounds.iter().map(|r| r.batch128.p99).collect());
    let concurrent_batches = median_f64(rounds.iter().map(|r| r.concurrent_batches).collect());

    let throughputs: Vec<f64> = rounds.iter().map(|r| r.sequential.throughput).collect();
    let concurrent: Vec<f64> = rounds.iter().map(|r| r.concurrent_writes).collect();
    let batches: Vec<f64> = rounds.iter().map(|r| r.batch128.orders_per_sec).collect();
    let concurrent_batch_values: Vec<f64> = rounds.iter().map(|r| r.concurrent_batches).collect();
    let (seq_min, seq_max) = range_f64(&throughputs);
    let (concurrent_min, concurrent_max) = range_f64(&concurrent);
    let (batch_min, batch_max) = range_f64(&batches);
    let (concurrent_batch_min, concurrent_batch_max) = range_f64(&concurrent_batch_values);

    println!(
        "\n=== median of {} fresh four-node rounds ===",
        rounds.len()
    );
    println!("sequential /write: {seq_throughput:>8.0} writes/s (range {seq_min:.0}-{seq_max:.0})");
    println!("  p50 / p95 / p99: {seq_p50:?} / {seq_p95:?} / {seq_p99:?}");
    println!(
        "concurrent /write: {concurrent_writes:>8.0} writes/s (range {concurrent_min:.0}-{concurrent_max:.0})"
    );
    println!(
        "batch 128:         {batch128:>8.0} orders/s (range {batch_min:.0}-{batch_max:.0}); p50 {batch128_p50:?}, p99 {batch128_p99:?}"
    );
    println!(
        "concurrent batches: {concurrent_batches:>7.0} orders/s (range {concurrent_batch_min:.0}-{concurrent_batch_max:.0})"
    );
}

fn main() -> anyhow::Result<()> {
    let mut rounds = 3usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Cargo passes `--bench` to harness-less bench binaries.
            "--bench" => {}
            "--rounds" => {
                rounds = args
                    .next()
                    .context("--rounds needs a count")?
                    .parse()
                    .context("--rounds must be a number")?;
            }
            other => bail!("unknown argument {other:?}; expected --rounds <n>"),
        }
    }
    if rounds == 0 {
        bail!("--rounds must be at least 1");
    }

    println!("four-node no-fsync cluster benchmark ({NODE_COUNT} voters, quorum 3/4, loopback)");
    println!("all nodes run in one process; each has its own storage, HTTP and gRPC listener");
    println!("WAL fsync disabled; {WARMUP_WRITES} warm-up writes per round are excluded");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let results = runtime.block_on(async {
        let mut results = Vec::with_capacity(rounds);
        for round in 1..=rounds {
            results.push(run_round(round, rounds).await?);
        }
        Ok::<_, anyhow::Error>(results)
    })?;
    print_summary(&results);
    Ok(())
}
