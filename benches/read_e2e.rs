//! Read-path benchmark: local `/read`, `/linearizable_read` and `/events`.
//!
//! Starts a four-node no-fsync cluster, seeds a small book and event window,
//! then measures each endpoint at one and 64 concurrent readers. The mixed
//! cases run a paced writer in parallel. The cluster runs on one runtime and
//! the read/write clients run on a separate runtime, so the client tasks do not
//! steal workers from the server.
//!
//! Run with:
//!
//! ```text
//! cargo bench --bench read_e2e -- [--policy read_index|lease|both]
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use anyhow::bail;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::network::read_policy::ReadPath;
use matchengine_raft::network::read_policy::ReadPathMetrics;
use matchengine_raft::network::read_policy::ReadPolicyMode;
use matchengine_raft::network::read_policy::ReadPolicyStatus;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use tokio::sync::Barrier;
use tokio::sync::watch;

type C = MatchEngineTypeConfig;

const NODE_COUNT: u64 = 4;
const FIRST_HTTP_PORT: u16 = 25101;
const READERS: [usize; 2] = [1, 64];
const READS_PER_READER: usize = 2_000;
const WARMUP_READS: usize = 100;
const SEED_ORDERS: usize = 2_000;
const SEED_BATCH: usize = 128;
const EVENTS_LIMIT: usize = 100;
const MIXED_WRITE_RATE: u64 = 2_000;
const READ_KEY: &str = "orderbook_depth:20";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endpoint {
    LocalRead,
    LinearizableRead,
    Events,
}

impl Endpoint {
    const ALL: [Self; 3] = [Self::LocalRead, Self::LinearizableRead, Self::Events];

    fn as_str(self) -> &'static str {
        match self {
            Self::LocalRead => "local_read",
            Self::LinearizableRead => "linearizable_read",
            Self::Events => "events",
        }
    }

    fn policy_path(self) -> Option<ReadPath> {
        match self {
            Self::LocalRead => None,
            Self::LinearizableRead => Some(ReadPath::LinearizableRead),
            Self::Events => Some(ReadPath::Events),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaseKey {
    endpoint: Endpoint,
    readers: usize,
    mixed: bool,
}

impl CaseKey {
    fn load(self) -> &'static str {
        if self.mixed { "mixed" } else { "read-only" }
    }
}

#[derive(Clone, Copy)]
struct Stats {
    throughput: f64,
    p50: Duration,
    p95: Duration,
    p99: Duration,
}

struct CaseResult {
    key: CaseKey,
    stats: Stats,
    lease_attempts: u64,
    lease_hits: u64,
    lease_fallbacks: u64,
    read_index_attempts: u64,
    writes: u64,
}

struct PolicyResult {
    mode: ReadPolicyMode,
    cases: Vec<CaseResult>,
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

fn summarize(mut latencies: Vec<Duration>, elapsed: Duration) -> Stats {
    latencies.sort_unstable();
    let total = latencies.len();
    Stats {
        throughput: total as f64 / elapsed.as_secs_f64(),
        p50: percentile(&latencies, 0.50),
        p95: percentile(&latencies, 0.95),
        p99: percentile(&latencies, 0.99),
    }
}

impl Cluster {
    async fn start(policy: ReadPolicyMode) -> anyhow::Result<Self> {
        let root = temp_dir("read-e2e");
        let mut nodes = Vec::with_capacity(NODE_COUNT as usize);
        let mut clients = Vec::with_capacity(NODE_COUNT as usize);
        let mut addrs = Vec::with_capacity(NODE_COUNT as usize);

        for id in 1..=NODE_COUNT {
            let addr = http_addr(id);
            let storage = StorageConfig::new(root.clone())
                .with_instance_prefix("read-e2e")
                .with_snapshot_interval(Duration::ZERO)
                .with_wal_fsync(None)
                .with_min_voters(NODE_COUNT as usize)
                .with_read_policy(policy);
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

async fn fetch_status(addr: &str) -> anyhow::Result<ReadPolicyStatus> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let status = http
        .get(format!("http://{addr}/read-policy/status"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(status)
}

fn status_metrics(status: &ReadPolicyStatus, endpoint: Endpoint) -> Option<ReadPathMetrics> {
    match endpoint.policy_path()? {
        ReadPath::LinearizableRead => Some(status.linearizable_read),
        ReadPath::Events => Some(status.events),
        ReadPath::StreamEvents => Some(status.stream_events),
        ReadPath::GetLinearizer => Some(status.get_linearizer),
    }
}

fn metric_delta(after: ReadPathMetrics, before: ReadPathMetrics) -> ReadPathMetrics {
    ReadPathMetrics {
        lease_attempts: after.lease_attempts.saturating_sub(before.lease_attempts),
        lease_hits: after.lease_hits.saturating_sub(before.lease_hits),
        lease_fallbacks: after.lease_fallbacks.saturating_sub(before.lease_fallbacks),
        lease_failures: after.lease_failures.saturating_sub(before.lease_failures),
        read_index_attempts: after
            .read_index_attempts
            .saturating_sub(before.read_index_attempts),
        read_index_failures: after
            .read_index_failures
            .saturating_sub(before.read_index_failures),
    }
}

async fn read_once(
    client: &MatchEngineClient<C>,
    endpoint: Endpoint,
    key: &String,
) -> anyhow::Result<()> {
    match endpoint {
        Endpoint::LocalRead => {
            client.read(key).await?;
        }
        Endpoint::LinearizableRead => {
            client.linearizable_read(key).await??;
        }
        Endpoint::Events => {
            let page = client
                .events(&EventsRequest {
                    after: None,
                    limit: Some(EVENTS_LIMIT),
                    wait_ms: None,
                })
                .await??;
            if page.events.is_empty() {
                bail!("seeded event page is empty");
            }
        }
    }
    Ok(())
}

async fn paced_writer(
    client: Arc<MatchEngineClient<C>>,
    rate: u64,
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let start = Instant::now();
    let mut index = 0u64;
    loop {
        if *stop.borrow() {
            break;
        }
        let due = start + Duration::from_secs_f64(index as f64 / rate as f64);
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(due)) => {}
            _ = stop.changed() => break,
        }
        client.write(&write_request(index as usize)).await??;
        index += 1;
    }
    Ok(index)
}

async fn measure_case(
    client: Arc<MatchEngineClient<C>>,
    leader_addr: &str,
    key: String,
    endpoint: Endpoint,
    readers: usize,
    mixed: bool,
) -> anyhow::Result<CaseResult> {
    let before_status = fetch_status(leader_addr).await?;
    let (stop_tx, writer) = if mixed {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(paced_writer(client.clone(), MIXED_WRITE_RATE, rx));
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

    let barrier = Arc::new(Barrier::new(readers + 1));
    let mut handles = Vec::with_capacity(readers);
    for _ in 0..readers {
        let client = client.clone();
        let barrier = barrier.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut latencies = Vec::with_capacity(READS_PER_READER);
            for _ in 0..READS_PER_READER {
                let started = Instant::now();
                read_once(&client, endpoint, &key).await?;
                latencies.push(started.elapsed());
            }
            Ok::<_, anyhow::Error>(latencies)
        }));
    }

    let started = Instant::now();
    barrier.wait().await;
    let mut latencies = Vec::with_capacity(readers * READS_PER_READER);
    for handle in handles {
        latencies.extend(handle.await??);
    }
    let elapsed = started.elapsed();

    let writes = if let Some((tx, handle)) = stop_tx.zip(writer) {
        let _ = tx.send(true);
        handle.await??
    } else {
        0
    };

    let after_status = fetch_status(leader_addr).await?;
    let (lease_attempts, lease_hits, lease_fallbacks, read_index_attempts) = match (
        status_metrics(&before_status, endpoint),
        status_metrics(&after_status, endpoint),
    ) {
        (Some(before), Some(after)) => {
            let delta = metric_delta(after, before);
            (
                delta.lease_attempts,
                delta.lease_hits,
                delta.lease_fallbacks,
                delta.read_index_attempts,
            )
        }
        _ => (0, 0, 0, 0),
    };

    Ok(CaseResult {
        key: CaseKey {
            endpoint,
            readers,
            mixed,
        },
        stats: summarize(latencies, elapsed),
        lease_attempts,
        lease_hits,
        lease_fallbacks,
        read_index_attempts,
        writes,
    })
}

async fn seed_book(client: &MatchEngineClient<C>) -> anyhow::Result<()> {
    let mut next = 0usize;
    while next < SEED_ORDERS {
        let requests: Vec<MatchEngineRequest> = (next..(next + SEED_BATCH).min(SEED_ORDERS))
            .map(write_request)
            .collect();
        next += requests.len();
        client.write_batch(&requests).await??;
    }
    Ok(())
}

async fn run_cases(cluster: &Cluster, leader_index: usize) -> anyhow::Result<Vec<CaseResult>> {
    let client = cluster.clients[leader_index].clone();
    let leader_addr = cluster.addrs[leader_index].1.clone();
    let key = READ_KEY.to_string();

    seed_book(&client).await?;
    for _ in 0..WARMUP_READS {
        for endpoint in Endpoint::ALL {
            read_once(&client, endpoint, &key).await?;
        }
    }

    let mut results = Vec::with_capacity(Endpoint::ALL.len() * READERS.len() * 2);
    for readers in READERS {
        for mixed in [false, true] {
            for endpoint in Endpoint::ALL {
                let case = measure_case(
                    client.clone(),
                    &leader_addr,
                    key.clone(),
                    endpoint,
                    readers,
                    mixed,
                )
                .await?;
                results.push(case);
            }
        }
    }
    Ok(results)
}

fn run_policy(policy: ReadPolicyMode) -> anyhow::Result<PolicyResult> {
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("read-bench-server")
        .build()?;
    let cluster = server_rt.block_on(Cluster::start(policy))?;
    let leader_index = server_rt.block_on(cluster.initialize())?;

    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("read-bench-client")
        .build()?;
    let result = client_rt.block_on(run_cases(&cluster, leader_index));
    let shutdown = server_rt.block_on(cluster.shutdown());
    let cases = result?;
    shutdown?;
    std::thread::sleep(Duration::from_millis(100));
    Ok(PolicyResult {
        mode: policy,
        cases,
    })
}

fn print_policy(result: &PolicyResult) {
    println!("\n=== read policy: {} ===", result.mode);
    println!(
        "{:<18} {:>7} {:>9} {:>11} {:>10} {:>10} {:>10} {:>9} {:>8} {:>8}",
        "endpoint",
        "readers",
        "load",
        "reads/s",
        "p50",
        "p95",
        "p99",
        "lease hit",
        "fallback",
        "read idx"
    );
    for case in &result.cases {
        let (hit, fallback) = if case.key.endpoint == Endpoint::LocalRead {
            ("-".to_string(), "-".to_string())
        } else {
            (
                format!("{}/{}", case.lease_hits, case.lease_attempts),
                case.lease_fallbacks.to_string(),
            )
        };
        println!(
            "{:<18} {:>7} {:>9} {:>11.0} {:>10?} {:>10?} {:>10?} {:>10} {:>8} {:>8}",
            case.key.endpoint.as_str(),
            case.key.readers,
            case.key.load(),
            case.stats.throughput,
            case.stats.p50,
            case.stats.p95,
            case.stats.p99,
            hit,
            fallback,
            case.read_index_attempts,
        );
        if case.key.mixed {
            println!("  paced writes: {}", case.writes);
        }
    }
}

fn find_case(result: &PolicyResult, key: CaseKey) -> Option<&CaseResult> {
    result.cases.iter().find(|case| case.key == key)
}

fn print_comparison(read_index: &PolicyResult, lease: &PolicyResult) {
    println!("\n=== lease vs read_index (same 4-node no-fsync cluster shape) ===");
    println!(
        "{:<18} {:>7} {:>9} {:>12} {:>12} {:>8} {:>12} {:>12} {:>8}",
        "endpoint",
        "readers",
        "load",
        "index p50",
        "lease p50",
        "p50 gain",
        "index reads/s",
        "lease reads/s",
        "speedup"
    );
    for readers in READERS {
        for mixed in [false, true] {
            for endpoint in Endpoint::ALL {
                let key = CaseKey {
                    endpoint,
                    readers,
                    mixed,
                };
                let Some(index) = find_case(read_index, key) else {
                    continue;
                };
                let Some(lease) = find_case(lease, key) else {
                    continue;
                };
                let p50_gain = index.stats.p50.as_secs_f64() / lease.stats.p50.as_secs_f64();
                let speedup = lease.stats.throughput / index.stats.throughput;
                println!(
                    "{:<18} {:>7} {:>9} {:>12?} {:>12?} {:>7.2}x {:>12.0} {:>12.0} {:>7.2}x",
                    endpoint.as_str(),
                    readers,
                    if mixed { "mixed" } else { "read-only" },
                    index.stats.p50,
                    lease.stats.p50,
                    p50_gain,
                    index.stats.throughput,
                    lease.stats.throughput,
                    speedup,
                );
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut policies = vec![ReadPolicyMode::ReadIndex, ReadPolicyMode::Lease];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => {}
            "--policy" => {
                let value = args.next().context("--policy needs a value")?;
                policies = if value.eq_ignore_ascii_case("both") {
                    vec![ReadPolicyMode::ReadIndex, ReadPolicyMode::Lease]
                } else {
                    let mode =
                        ReadPolicyMode::from_str(&value).map_err(|error| anyhow::anyhow!(error))?;
                    vec![mode]
                };
            }
            other => bail!("unknown argument {other:?}; expected --policy <read_index|lease|both>"),
        }
    }

    println!(
        "read-path benchmark: {NODE_COUNT}-node no-fsync cluster, {READS_PER_READER} reads/reader, key {READ_KEY}, events page {EVENTS_LIMIT}, mixed writes {MIXED_WRITE_RATE}/s"
    );
    println!("cluster runtime and client runtime are separate");

    let mut results = Vec::with_capacity(policies.len());
    for policy in policies {
        let result = run_policy(policy)?;
        print_policy(&result);
        results.push(result);
    }
    if results.len() == 2 {
        print_comparison(&results[0], &results[1]);
    }
    Ok(())
}
