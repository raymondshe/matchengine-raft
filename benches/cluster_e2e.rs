//! Four-node no-fsync cluster write and concurrency benchmark.
//!
//! Starts four in-process voters on loopback with WAL fsync disabled, initializes
//! all four in one `/init`, and measures leader-direct `/write` and
//! `/write_batch`. Each round starts a fresh cluster so the book and WAL do not
//! grow across rounds. The cluster runs on a server runtime and the clients run
//! on a separate runtime.
//!
//! The concurrency sweep covers 1/8/64/256/1024/4096 clients. The one-factor
//! OpenRaft sweeps are exposed as flags so each run stays reproducible:
//!
//! ```text
//! cargo bench --bench cluster_e2e -- --rounds 3 --concurrency-only
//! cargo bench --bench cluster_e2e -- --concurrency-only --max-payload-entries 1024
//! cargo bench --bench cluster_e2e -- --concurrency-only --purge-batch-size 1024 --snapshot-logs 10000
//! cargo bench --bench cluster_e2e -- --concurrency-only --api-batch-linger-ms 100
//! RAFT_CORE_WORKERS=2 cargo bench --bench cluster_e2e -- --concurrency-only
//! ```
//!
//! The thin-LTO A/B is a build-profile run:
//!
//! ```text
//! CARGO_PROFILE_BENCH_LTO=thin CARGO_PROFILE_BENCH_CODEGEN_UNITS=1 \
//!   cargo bench --bench cluster_e2e -- --concurrency-only
//! ```

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
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use tokio::sync::Barrier;

type C = MatchEngineTypeConfig;

const NODE_COUNT: u64 = 4;
const FIRST_HTTP_PORT: u16 = 25101;
const SEQUENTIAL_WRITES: usize = 500;
const WARMUP_WRITES: usize = 50;
const BATCH_SIZES: [usize; 5] = [1, 8, 32, 64, 128];
const BATCHES_PER_SIZE: usize = 200;
const CONCURRENT_BATCHES: usize = 100;
const CONCURRENT_BATCH_SIZE: usize = 32;
const TOTAL_CONCURRENCY_REQUESTS: usize = 2_000;
const DEFAULT_CLIENTS: [usize; 6] = [1, 8, 64, 256, 1_024, 4_096];

#[derive(Clone, Debug)]
struct Args {
    rounds: usize,
    clients: Vec<usize>,
    concurrency_only: bool,
    max_payload_entries: u64,
    purge_batch_size: u64,
    api_batch_linger_ms: u64,
    snapshot_logs: Option<u64>,
    max_inflight_writes: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            rounds: 3,
            clients: DEFAULT_CLIENTS.to_vec(),
            concurrency_only: false,
            max_payload_entries: 300,
            purge_batch_size: 1,
            api_batch_linger_ms: 0,
            snapshot_logs: None,
            max_inflight_writes: 0,
        }
    }
}

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
struct ConcurrencyRow {
    clients: usize,
    total: usize,
    accepted: usize,
    rejected: usize,
    throughput: f64,
    p50: Duration,
    p95: Duration,
    p99: Duration,
}

struct RoundSummary {
    sequential: Option<Stats>,
    batch128: Option<BatchRow>,
    concurrent_batches: Option<f64>,
    concurrent_batches_p50: Option<Duration>,
    concurrent_batches_p99: Option<Duration>,
    concurrency: Vec<ConcurrencyRow>,
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

fn config_from_args(args: &Args) -> Config {
    let mut config = default_raft_config();
    config.max_payload_entries = args.max_payload_entries;
    config.purge_batch_size = args.purge_batch_size;
    config.api_batch_linger_ms = args.api_batch_linger_ms;
    if let Some(logs) = args.snapshot_logs {
        config.snapshot_policy = SnapshotPolicy::LogsSinceLast(logs);
    }
    config
}

impl Cluster {
    async fn start(
        round: usize,
        config: Config,
        max_inflight_writes: usize,
    ) -> anyhow::Result<Self> {
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
                .with_min_voters(NODE_COUNT as usize)
                .with_max_inflight_writes(max_inflight_writes);
            let node = build_match_engine_node(id, addr.clone(), config.clone(), storage).await?;
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

async fn concurrent_batch_throughput(
    client: &MatchEngineClient<C>,
) -> anyhow::Result<(f64, Duration, Duration)> {
    let batches: Vec<Vec<MatchEngineRequest>> = (0..CONCURRENT_BATCHES)
        .map(|batch| {
            (0..CONCURRENT_BATCH_SIZE)
                .map(|offset| write_request(batch * CONCURRENT_BATCH_SIZE + offset))
                .collect()
        })
        .collect();
    let orders = CONCURRENT_BATCHES * CONCURRENT_BATCH_SIZE;

    let started = Instant::now();
    let responses = join_all(batches.iter().map(|requests| {
        let start = Instant::now();
        async move { (client.write_batch(requests).await, start.elapsed()) }
    }))
    .await;
    let elapsed = started.elapsed();
    let mut latencies = Vec::with_capacity(CONCURRENT_BATCHES);
    for (response, latency) in responses {
        if response.as_ref().is_ok_and(|inner| inner.is_ok()) {
            latencies.push(latency);
        }
    }
    let ok = latencies.len();
    if ok != CONCURRENT_BATCHES {
        bail!("only {ok}/{CONCURRENT_BATCHES} concurrent batches succeeded");
    }
    latencies.sort_unstable();
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
    println!(
        "  p50 / p99:  {:>10?} / {:?}",
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.99)
    );
    Ok((
        throughput,
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.99),
    ))
}

async fn write_concurrency(addr: &str, clients: usize) -> anyhow::Result<ConcurrencyRow> {
    let per_client = (TOTAL_CONCURRENCY_REQUESTS / clients).max(1);
    let total = clients * per_client;
    let body = serde_json::to_vec(&write_request(0))?;
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{addr}/write");
    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut handles = Vec::with_capacity(clients);

    for _ in 0..clients {
        let http = http.clone();
        let url = url.clone();
        let body = body.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut latencies = Vec::with_capacity(per_client);
            let mut rejected = 0usize;
            for _ in 0..per_client {
                let started = Instant::now();
                let status = http
                    .post(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
                    .send()
                    .await?
                    .status();
                if status.is_success() {
                    latencies.push(started.elapsed());
                } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    rejected += 1;
                } else {
                    bail!("write failed with HTTP {status}");
                }
            }
            Ok::<_, anyhow::Error>((latencies, rejected))
        }));
    }

    let started = Instant::now();
    barrier.wait().await;
    let mut latencies = Vec::with_capacity(total);
    let mut rejected = 0usize;
    for handle in handles {
        let (accepted, refused) = handle.await??;
        latencies.extend(accepted);
        rejected += refused;
    }
    let elapsed = started.elapsed();
    let accepted = latencies.len();
    if accepted == 0 {
        bail!("no writes were admitted");
    }
    latencies.sort_unstable();
    Ok(ConcurrencyRow {
        clients,
        total,
        accepted,
        rejected,
        throughput: accepted as f64 / elapsed.as_secs_f64(),
        p50: percentile(&latencies, 0.50),
        p95: percentile(&latencies, 0.95),
        p99: percentile(&latencies, 0.99),
    })
}

async fn run_workloads(
    cluster: &Cluster,
    leader_index: usize,
    args: &Args,
) -> anyhow::Result<RoundSummary> {
    let leader = cluster.clients[leader_index].clone();
    let leader_addr = cluster.addrs[leader_index].1.clone();

    println!("warm-up: {WARMUP_WRITES} writes (excluded)");
    for index in 0..WARMUP_WRITES {
        leader.write(&write_request(index)).await??;
    }

    let sequential = if args.concurrency_only {
        None
    } else {
        Some(sequential_latency(&leader).await?)
    };
    let batch_rows = if args.concurrency_only {
        None
    } else {
        Some(batch_throughput(&leader).await?)
    };
    let concurrent_batches = if args.concurrency_only {
        None
    } else {
        Some(concurrent_batch_throughput(&leader).await?)
    };

    println!("concurrency sweep (one client task per in-flight request):");
    println!(
        "  {:>7}  {:>8}  {:>8}  {:>8}  {:>10}  {:>10}  {:>10}  {:>10}",
        "clients", "attempts", "accepted", "rejected", "writes/s", "p50", "p95", "p99"
    );
    let mut concurrency = Vec::with_capacity(args.clients.len());
    for &clients in &args.clients {
        let row = write_concurrency(&leader_addr, clients).await?;
        println!(
            "  {:>7}  {:>8}  {:>8}  {:>8}  {:>10.0}  {:>10?}  {:>10?}  {:>10?}",
            row.clients,
            row.total,
            row.accepted,
            row.rejected,
            row.throughput,
            row.p50,
            row.p95,
            row.p99
        );
        concurrency.push(row);
    }

    Ok(RoundSummary {
        sequential,
        batch128: batch_rows.and_then(|rows| rows.into_iter().find(|row| row.size == 128)),
        concurrent_batches: concurrent_batches.map(|(throughput, _, _)| throughput),
        concurrent_batches_p50: concurrent_batches.map(|(_, p50, _)| p50),
        concurrent_batches_p99: concurrent_batches.map(|(_, _, p99)| p99),
        concurrency,
    })
}

fn run_round(round: usize, rounds: usize, args: &Args) -> anyhow::Result<RoundSummary> {
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("cluster-bench-server")
        .build()?;
    let cluster = server_rt.block_on(Cluster::start(
        round,
        config_from_args(args),
        args.max_inflight_writes,
    ))?;
    let leader_index = server_rt.block_on(cluster.initialize())?;
    let leader_id = cluster.addrs[leader_index].0;
    let leader_addr = cluster.addrs[leader_index].1.clone();

    println!("\n=== round {round}/{rounds}: leader node {leader_id} ({leader_addr}) ===");
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("cluster-bench-client")
        .build()?;
    let result = client_rt.block_on(run_workloads(&cluster, leader_index, args));
    let shutdown = server_rt.block_on(cluster.shutdown());
    let summary = result?;
    shutdown?;
    // Give the fixed loopback ports a moment to be released.
    std::thread::sleep(Duration::from_millis(100));
    Ok(summary)
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn median_duration(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn print_summary(rounds: &[RoundSummary], args: &Args) {
    println!(
        "\n=== median of {} fresh four-node rounds ===",
        rounds.len()
    );
    if rounds.iter().any(|round| round.sequential.is_some()) {
        let throughput = median_f64(
            rounds
                .iter()
                .filter_map(|r| r.sequential)
                .map(|s| s.throughput)
                .collect(),
        );
        let p50 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.sequential)
                .map(|s| s.p50)
                .collect(),
        );
        let p95 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.sequential)
                .map(|s| s.p95)
                .collect(),
        );
        let p99 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.sequential)
                .map(|s| s.p99)
                .collect(),
        );
        println!(
            "sequential /write: {throughput:>8.0} writes/s; p50 {p50:?}, p95 {p95:?}, p99 {p99:?}"
        );
    }
    if rounds.iter().any(|round| round.batch128.is_some()) {
        let through = median_f64(
            rounds
                .iter()
                .filter_map(|r| r.batch128)
                .map(|b| b.orders_per_sec)
                .collect(),
        );
        let p50 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.batch128)
                .map(|b| b.p50)
                .collect(),
        );
        let p99 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.batch128)
                .map(|b| b.p99)
                .collect(),
        );
        println!("batch 128: {through:>8.0} orders/s; p50 {p50:?}, p99 {p99:?}");
    }
    if rounds
        .iter()
        .any(|round| round.concurrent_batches.is_some())
    {
        let median = median_f64(rounds.iter().filter_map(|r| r.concurrent_batches).collect());
        let p50 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.concurrent_batches_p50)
                .collect(),
        );
        let p99 = median_duration(
            rounds
                .iter()
                .filter_map(|r| r.concurrent_batches_p99)
                .collect(),
        );
        println!("concurrent batches 100x32: {median:>8.0} orders/s; p50 {p50:?}, p99 {p99:?}");
    }

    println!(
        "concurrency sweep: clients={:?}, max_inflight_writes={}, max_payload_entries={}, purge_batch_size={}, api_batch_linger_ms={}, snapshot_logs={:?}, RAFT_CORE_WORKERS={}",
        args.clients,
        args.max_inflight_writes,
        args.max_payload_entries,
        args.purge_batch_size,
        args.api_batch_linger_ms,
        args.snapshot_logs,
        std::env::var("RAFT_CORE_WORKERS").unwrap_or_else(|_| "unset/shared".to_string())
    );
    println!(
        "  {:>7}  {:>8}  {:>8}  {:>10}  {:>10}  {:>10}",
        "clients", "accepted", "rejected", "writes/s", "p50", "p99"
    );
    for &clients in &args.clients {
        let rows: Vec<ConcurrencyRow> = rounds
            .iter()
            .flat_map(|round| round.concurrency.iter().copied())
            .filter(|row| row.clients == clients)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let throughput = median_f64(rows.iter().map(|row| row.throughput).collect());
        let accepted = median_f64(rows.iter().map(|row| row.accepted as f64).collect());
        let rejected = median_f64(rows.iter().map(|row| row.rejected as f64).collect());
        let p50 = median_duration(rows.iter().map(|row| row.p50).collect());
        let p99 = median_duration(rows.iter().map(|row| row.p99).collect());
        println!(
            "  {:>7}  {:>8.0}  {:>8.0}  {:>10.0}  {:>10?}  {:>10?}",
            clients, accepted, rejected, throughput, p50, p99
        );
    }
}

fn parse_clients(value: &str) -> anyhow::Result<Vec<usize>> {
    let clients: Vec<usize> = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<usize>())
        .collect::<Result<_, _>>()
        .context("--clients must be a comma-separated list of positive integers")?;
    if clients.is_empty() || clients.contains(&0) {
        bail!("--clients must contain at least one positive integer");
    }
    Ok(clients)
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bench" => {}
            "--rounds" => {
                args.rounds = iter
                    .next()
                    .context("--rounds needs a count")?
                    .parse()
                    .context("--rounds must be a number")?;
            }
            "--clients" => {
                args.clients = parse_clients(&iter.next().context("--clients needs a list")?)?;
            }
            "--concurrency-only" => args.concurrency_only = true,
            "--max-payload-entries" => {
                args.max_payload_entries = iter
                    .next()
                    .context("--max-payload-entries needs a value")?
                    .parse()
                    .context("--max-payload-entries must be a number")?;
            }
            "--purge-batch-size" => {
                args.purge_batch_size = iter
                    .next()
                    .context("--purge-batch-size needs a value")?
                    .parse()
                    .context("--purge-batch-size must be a number")?;
            }
            "--api-batch-linger-ms" => {
                args.api_batch_linger_ms = iter
                    .next()
                    .context("--api-batch-linger-ms needs a value")?
                    .parse()
                    .context("--api-batch-linger-ms must be a number")?;
            }
            "--snapshot-logs" => {
                args.snapshot_logs = Some(
                    iter.next()
                        .context("--snapshot-logs needs a value")?
                        .parse()
                        .context("--snapshot-logs must be a number")?,
                );
            }
            "--max-inflight-writes" => {
                args.max_inflight_writes = iter
                    .next()
                    .context("--max-inflight-writes needs a value")?
                    .parse()
                    .context("--max-inflight-writes must be a number")?;
            }
            other => bail!("unknown argument {other:?}"),
        }
    }
    if args.rounds == 0 {
        bail!("--rounds must be at least 1");
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    println!("four-node no-fsync cluster benchmark ({NODE_COUNT} voters, quorum 3/4, loopback)");
    println!("all nodes run in one process; each has its own storage, HTTP and gRPC listener");
    println!("WAL fsync disabled; {WARMUP_WRITES} warm-up writes per round are excluded");
    println!(
        "config: clients={:?}, max_inflight_writes={}, max_payload_entries={}, purge_batch_size={}, api_batch_linger_ms={}, snapshot_logs={:?}, RAFT_CORE_WORKERS={}",
        args.clients,
        args.max_inflight_writes,
        args.max_payload_entries,
        args.purge_batch_size,
        args.api_batch_linger_ms,
        args.snapshot_logs,
        std::env::var("RAFT_CORE_WORKERS").unwrap_or_else(|_| "unset/shared".to_string())
    );

    let mut results = Vec::with_capacity(args.rounds);
    for round in 1..=args.rounds {
        results.push(run_round(round, args.rounds, &args)?);
    }
    print_summary(&results, &args);
    Ok(())
}
