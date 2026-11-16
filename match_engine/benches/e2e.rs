//! End-to-end write latency: HTTP submit -> Raft commit -> apply -> response.
//!
//! Run with `cargo bench --bench e2e`. This is a single-node benchmark, so it
//! includes HTTP, WAL append/fsync, Raft apply and response serialization but
//! excludes cross-node replication. It also sweeps `/write_batch` sizes to show
//! how one WAL fsync per entry amortizes across many orders. By default the
//! suite runs on the production multi-thread Tokio runtime; `-- current` and
//! `-- both` compare the old current-thread flavor, and `--pin <cpu>` pins the
//! process on Linux (`sched_setaffinity`) to remove scheduler noise. The
//! order-book-only costs are measured by `cargo bench --bench matchengine`.
//!
//! For sustained CPU profiling, `--duration <seconds>` keeps `--clients <n>`
//! closed-loop `/write` clients running and reports process CPU alongside
//! throughput and latency:
//!
//! ```text
//! cargo bench --bench e2e -- --duration 60 --clients 500
//! cargo bench --bench e2e -- --duration 60 --clients 500 --rate 10000
//! ```
//!
//! `--rate <requests/s>` switches to a fixed-rate open-loop schedule: each
//! client keeps an absolute send schedule and latency is measured from the
//! scheduled send time, so queueing/overload delay is included instead of being
//! hidden by a closed loop. Without `--rate` the clients remain closed-loop.
//!
//! The sustained mode uses IOC orders so the book stays bounded; it profiles
//! the HTTP/Raft/WAL/apply/event path rather than deep matching sweeps. CPU is
//! the whole process (in-process node plus load generator).
//!
//! `MATCHENGINE_ASYNC_WAL=1` with `--features async-wal` and `RAFT_WAL_FSYNC_MS=0`
//! selects the prototype async WAL for the same suite.

use std::path::PathBuf;
use std::sync::Arc;
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
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use tokio::sync::Barrier;

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

/// Process CPU time in seconds, including all threads. `None` on platforms
/// without `getrusage`.
#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    Some(
        usage.ru_utime.tv_sec as f64
            + usage.ru_utime.tv_usec as f64 / 1_000_000.0
            + usage.ru_stime.tv_sec as f64
            + usage.ru_stime.tv_usec as f64 / 1_000_000.0,
    )
}

#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
}

/// Pins the process to one CPU before the runtimes start. Threads created
/// afterwards inherit the affinity mask, so this reproduces
/// `taskset -c <cpu> cargo bench --bench e2e` on Linux.
#[cfg(target_os = "linux")]
fn pin_to_cpu(cpu: usize) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if cpu >= 8 * std::mem::size_of::<libc::cpu_set_t>() {
        anyhow::bail!("CPU {cpu} does not fit the affinity mask");
    }
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context(format!("pin process to CPU {cpu}"));
    }
    Ok(())
}

/// macOS and the BSDs expose no process core-pinning API; fail loudly instead
/// of silently running unpinned.
#[cfg(not(target_os = "linux"))]
fn pin_to_cpu(cpu: usize) -> anyhow::Result<()> {
    anyhow::bail!(
        "--pin {cpu} needs Linux sched_setaffinity; this host has no process core-pinning API. \
         Run `cargo bench --bench e2e -- current` for a single-threaded runtime approximation, \
         or run the pinned pass on Linux."
    )
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

fn sustained_write_request() -> MatchEngineRequest {
    // IOC against an empty book cancels immediately, so a multi-minute run
    // does not grow the resting book or turn into a cancel benchmark.
    let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    order.tif = TimeInForce::Ioc;
    MatchEngineRequest::place(order)
}

fn async_wal_from_env() -> bool {
    std::env::var("MATCHENGINE_ASYNC_WAL")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn read_policy_from_env() -> matchengine_raft::network::read_policy::ReadPolicyMode {
    std::env::var(matchengine_raft::network::read_policy::READ_POLICY_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

async fn start_node(
    addr: &str,
    runtime: &str,
) -> anyhow::Result<(MatchEngineNode, MatchEngineClient<C>, PathBuf)> {
    let dir = temp_dir(runtime);
    let fsync = matchengine_raft::store::config::wal_fsync_from_env();
    let async_wal = async_wal_from_env();
    let read_policy = read_policy_from_env();
    if async_wal {
        #[cfg(not(feature = "async-wal"))]
        anyhow::bail!("MATCHENGINE_ASYNC_WAL requires --features async-wal");
        #[cfg(feature = "async-wal")]
        if fsync != Some(Duration::ZERO) {
            anyhow::bail!("async WAL requires RAFT_WAL_FSYNC_MS=0");
        }
    }
    let storage = StorageConfig::new(dir.clone())
        .with_instance_prefix(format!("e2e-{runtime}"))
        .with_snapshot_interval(Duration::ZERO)
        .with_wal_fsync(fsync)
        .with_read_policy(read_policy)
        // This bench measures the unbounded write path; admission control is
        // measured by `cluster_e2e --max-inflight-writes`.
        .with_max_inflight_writes(0)
        .with_async_wal(async_wal);
    let node = build_match_engine_node(1, addr.to_string(), default_raft_config(), storage).await?;
    println!("read policy: {read_policy}");
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

/// Sends one sustained `/write` and drains the body, returning the status.
///
/// The caller times the call itself: closed-loop latency starts at send time,
/// while open-loop latency starts at the scheduled send time.
async fn sustained_send(
    http: &reqwest::Client,
    url: &str,
    body: &[u8],
) -> Result<reqwest::StatusCode, ()> {
    let response = http
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|_| ())?;
    let status = response.status();
    response.bytes().await.map_err(|_| ())?;
    Ok(status)
}

async fn sustained_load(
    addr: &str,
    runtime: &str,
    clients: usize,
    duration: Duration,
    rate: Option<u64>,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&sustained_write_request())?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(clients)
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!("http://{addr}/write");
    match rate {
        Some(rate) => println!(
            "sustained writes ({runtime}): {clients} clients for {duration:?}, open-loop {rate} writes/s"
        ),
        None => println!("sustained writes ({runtime}): {clients} clients for {duration:?}"),
    }

    // Establish one keep-alive connection per client before the timed phase,
    // so connection setup does not hide the real in-flight concurrency.
    let preconnect_started = Instant::now();
    let health_url = format!("http://{addr}/health");
    let preconnect = (0..clients).map(|_| {
        let http = http.clone();
        let health_url = health_url.clone();
        async move {
            let response = http.get(&health_url).send().await?;
            let _ = response.bytes().await?;
            Ok::<_, anyhow::Error>(())
        }
    });
    for result in join_all(preconnect).await {
        result?;
    }
    println!(
        "  pre-connected {clients} keep-alive connections in {:?}",
        preconnect_started.elapsed()
    );

    let deadline = Instant::now() + duration;
    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut handles = Vec::with_capacity(clients);
    for _ in 0..clients {
        let http = http.clone();
        let url = url.clone();
        let body = body.clone();
        let barrier = barrier.clone();
        // Open-loop clients keep an absolute schedule; the interval is the
        // share of the total offered rate assigned to this client.
        let interval = rate.map(|rate| {
            Duration::from_secs_f64(clients as f64 / rate as f64).max(Duration::from_nanos(1))
        });
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut latencies = Vec::new();
            let mut service_latencies = Vec::new();
            let mut dispatch_lags = Vec::new();
            let mut rejected = 0usize;
            let mut failed = 0usize;
            let mut sent = 0usize;
            match interval {
                Some(interval) => {
                    let mut scheduled = Instant::now() + interval;
                    while scheduled < deadline {
                        let now = Instant::now();
                        if scheduled > now {
                            tokio::time::sleep(scheduled - now).await;
                        }
                        let started = Instant::now();
                        let dispatch_lag = started.saturating_duration_since(scheduled);
                        let result = sustained_send(&http, &url, &body).await;
                        let completed = Instant::now();
                        sent += 1;
                        match result {
                            Ok(status) if status.is_success() => {
                                latencies.push(completed.saturating_duration_since(scheduled));
                                service_latencies
                                    .push(completed.saturating_duration_since(started));
                                dispatch_lags.push(dispatch_lag);
                            }
                            Ok(status) if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                rejected += 1;
                            }
                            Ok(_) | Err(()) => failed += 1,
                        }
                        scheduled += interval;
                    }
                }
                None => {
                    while Instant::now() < deadline {
                        let started = Instant::now();
                        sent += 1;
                        match sustained_send(&http, &url, &body).await {
                            Ok(status) if status.is_success() => {
                                latencies.push(started.elapsed());
                            }
                            Ok(status) if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                rejected += 1;
                            }
                            Ok(_) | Err(()) => failed += 1,
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>((
                latencies,
                service_latencies,
                dispatch_lags,
                rejected,
                failed,
                sent,
            ))
        }));
    }

    let cpu_start = process_cpu_seconds();
    let wall_start = Instant::now();
    barrier.wait().await;

    let mut last_cpu = cpu_start;
    let mut last_wall = wall_start;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
        let now = Instant::now();
        if let (Some(start), Some(cpu), Some(previous_cpu)) =
            (cpu_start, process_cpu_seconds(), last_cpu)
        {
            let interval = now.duration_since(last_wall).as_secs_f64();
            if interval > 0.0 {
                let elapsed = wall_start.elapsed().as_secs_f64();
                println!(
                    "  [cpu] t={:>5.1}s interval={:>5.2} cores cumulative={:>5.2} cores",
                    elapsed,
                    (cpu - previous_cpu) / interval,
                    (cpu - start) / elapsed.max(f64::EPSILON)
                );
                last_cpu = Some(cpu);
                last_wall = now;
            }
        }
    }

    let mut latencies = Vec::new();
    let mut service_latencies = Vec::new();
    let mut dispatch_lags = Vec::new();
    let mut rejected = 0usize;
    let mut failed = 0usize;
    let mut sent = 0usize;
    for result in join_all(handles).await {
        let (
            mut client_latencies,
            mut client_service_latencies,
            mut client_dispatch_lags,
            client_rejected,
            client_failed,
            client_sent,
        ) = result??;
        latencies.append(&mut client_latencies);
        service_latencies.append(&mut client_service_latencies);
        dispatch_lags.append(&mut client_dispatch_lags);
        rejected += client_rejected;
        failed += client_failed;
        sent += client_sent;
    }
    let elapsed = wall_start.elapsed();
    let cpu_used = match (cpu_start, process_cpu_seconds()) {
        (Some(start), Some(end)) => Some(end - start),
        _ => None,
    };

    let accepted = latencies.len();
    println!("  wall:       {:>10.2}s", elapsed.as_secs_f64());
    if let Some(rate) = rate {
        let offered = rate as f64 * elapsed.as_secs_f64();
        println!(
            "  offered:    {rate} writes/s (~{offered:.0} requests); sent {sent} ({:.1}%)",
            if offered > 0.0 {
                sent as f64 / offered * 100.0
            } else {
                0.0
            }
        );
    }
    println!("  accepted:   {accepted}");
    println!("  rejected:   {rejected}");
    println!("  failed:     {failed}");
    println!(
        "  throughput: {:>10.0} writes/s",
        accepted as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
    );
    if accepted > 0 {
        latencies.sort_unstable();
        if rate.is_some() {
            println!(
                "  open-loop p50/p95/p99/max: {:?} / {:?} / {:?} / {:?} (completion - scheduled send)",
                percentile(&latencies, 0.50),
                percentile(&latencies, 0.95),
                percentile(&latencies, 0.99),
                latencies.last().copied().unwrap_or_default()
            );
        } else {
            println!(
                "  p50/p95/p99: {:?} / {:?} / {:?}",
                percentile(&latencies, 0.50),
                percentile(&latencies, 0.95),
                percentile(&latencies, 0.99)
            );
        }
    }
    if rate.is_some() && !dispatch_lags.is_empty() {
        dispatch_lags.sort_unstable();
        println!(
            "  dispatch lag p50/p95/p99/max: {:?} / {:?} / {:?} / {:?}",
            percentile(&dispatch_lags, 0.50),
            percentile(&dispatch_lags, 0.95),
            percentile(&dispatch_lags, 0.99),
            dispatch_lags.last().copied().unwrap_or_default()
        );
        service_latencies.sort_unstable();
        println!(
            "  service p50/p95/p99/max: {:?} / {:?} / {:?} / {:?} (completion - actual send)",
            percentile(&service_latencies, 0.50),
            percentile(&service_latencies, 0.95),
            percentile(&service_latencies, 0.99),
            service_latencies.last().copied().unwrap_or_default()
        );
    }
    if let Some(cpu) = cpu_used {
        let cores = cpu / elapsed.as_secs_f64().max(f64::EPSILON);
        let logical = logical_cores();
        println!(
            "  CPU:        {cpu:>10.2}s = {cores:.2} cores ({:.1}% of {logical} logical cores; node + load generator)",
            cores / logical as f64 * 100.0
        );
    } else {
        println!("  CPU:        process CPU not available on this platform");
    }
    Ok(())
}

async fn run_sustained_suite(
    runtime: &str,
    addr: &str,
    clients: usize,
    duration: Duration,
    rate: Option<u64>,
) -> anyhow::Result<()> {
    println!("\n=== {runtime} runtime, sustained ===");
    let (node, _client, dir) = start_node(addr, runtime).await?;
    sustained_load(addr, runtime, clients, duration, rate).await?;
    node.shutdown().await?;
    let _ = std::fs::remove_dir_all(dir);
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
    use anyhow::Context as _;

    println!("end-to-end write benchmark (single node, HTTP + WAL + apply)");
    let mut selection = String::new();
    let mut pin = None;
    let mut duration = None;
    let mut clients = 64usize;
    let mut clients_set = false;
    let mut rate = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Cargo passes `--bench` to harness-less bench binaries.
            "--bench" => {}
            "current" | "both" => selection = arg,
            "--pin" => {
                let cpu = args.next().context("--pin needs a CPU number")?;
                pin = Some(cpu.parse::<usize>()?);
            }
            other if other.starts_with("--pin=") => {
                pin = Some(other["--pin=".len()..].parse::<usize>()?);
            }
            "--duration" => {
                let seconds = args.next().context("--duration needs seconds")?;
                duration =
                    Some(Duration::from_secs(seconds.parse().context(
                        "--duration must be a positive number of seconds",
                    )?));
            }
            other if other.starts_with("--duration=") => {
                duration = Some(Duration::from_secs(
                    other["--duration=".len()..]
                        .parse()
                        .context("--duration must be a positive number of seconds")?,
                ));
            }
            "--clients" => {
                clients = args
                    .next()
                    .context("--clients needs a number")?
                    .parse()
                    .context("--clients must be a positive number")?;
                clients_set = true;
            }
            other if other.starts_with("--clients=") => {
                clients = other["--clients=".len()..]
                    .parse()
                    .context("--clients must be a positive number")?;
                clients_set = true;
            }
            "--rate" => {
                rate = Some(
                    args.next()
                        .context("--rate needs requests per second")?
                        .parse()
                        .context("--rate must be a positive integer")?,
                );
            }
            other if other.starts_with("--rate=") => {
                rate = Some(
                    other["--rate=".len()..]
                        .parse()
                        .context("--rate must be a positive integer")?,
                );
            }
            other => {
                anyhow::bail!(
                    "unknown argument {other:?}; expected current|both|--pin <cpu>|--duration <seconds>|--clients <n>|--rate <requests/s>"
                )
            }
        }
    }
    if duration == Some(Duration::ZERO) {
        anyhow::bail!("--duration must be at least one second");
    }
    if clients == 0 {
        anyhow::bail!("--clients must be at least one");
    }
    if clients_set && duration.is_none() {
        anyhow::bail!("--clients needs --duration <seconds>");
    }
    if rate == Some(0) {
        anyhow::bail!("--rate must be at least one request per second");
    }
    if rate.is_some() && duration.is_none() {
        anyhow::bail!("--rate needs --duration <seconds>");
    }
    if let Some(cpu) = pin {
        pin_to_cpu(cpu)?;
        println!("process pinned to CPU {cpu}");
    }
    match matchengine_raft::store::config::wal_fsync_from_env() {
        None => println!("WAL fsync disabled (RAFT_WAL_FSYNC_MS unset)"),
        Some(interval) if interval.is_zero() => println!("WAL fsync: every append"),
        Some(interval) => println!("WAL fsync: every {}ms", interval.as_millis()),
    }
    println!(
        "async WAL: {}",
        if async_wal_from_env() { "on" } else { "off" }
    );
    let run_current = matches!(selection.as_str(), "current" | "both");
    let run_multi = selection != "current";

    if run_current {
        let current = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        match duration {
            Some(duration) => current.block_on(run_sustained_suite(
                "current_thread",
                CURRENT_THREAD_ADDR,
                clients,
                duration,
                rate,
            ))?,
            None => current.block_on(run_suite("current_thread", CURRENT_THREAD_ADDR))?,
        }
    }
    if run_multi {
        let multi = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        match duration {
            Some(duration) => multi.block_on(run_sustained_suite(
                "multi_thread",
                MULTI_THREAD_ADDR,
                clients,
                duration,
                rate,
            ))?,
            None => multi.block_on(run_suite("multi_thread", MULTI_THREAD_ADDR))?,
        }
    }
    Ok(())
}
