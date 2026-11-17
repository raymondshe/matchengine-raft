//! Supervisor for a supervisor-mode `SymbolShard` pod.
//!
//! One supervisor runs as the pod's only container. It reads the rendered
//! `processes.json` (a ConfigMap cache of the shard's `SymbolDeployment`s),
//! spawns one existing single-symbol engine per process, restarts crashed
//! children with backoff, and aggregates `/health` and `/ready` for the pod.
//!
//! Adding a symbol changes only the ConfigMap. The supervisor starts one new
//! child on each pod; existing children keep running with the same PIDs and
//! data directories. Removing a symbol sends SIGTERM to that child and keeps
//! its data directory for the retention window.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "match_supervisor",
    version,
    about = "Supervisor-mode pod agent for matchengine-raft"
)]
struct Opt {
    /// Rendered process list (mounted from the shard ConfigMap).
    #[arg(long, default_value = "/etc/match/processes.json")]
    config: PathBuf,
    /// Pod ordinal, passed to every engine child as `--id`.
    #[arg(long)]
    id: u64,
    /// Stable pod FQDN; every child binds `<pod-fqdn>:<httpPort>`.
    #[arg(long)]
    pod_fqdn: String,
    /// Root data directory; children use their own instance-prefix subdirectory.
    #[arg(long, default_value = "/data")]
    data_dir: PathBuf,
    /// Engine binary path.
    #[arg(long, default_value = "/usr/local/bin/matchengine-raft")]
    engine_bin: PathBuf,
    /// Supervisor HTTP listen address for health/readiness/diagnostics.
    #[arg(long, default_value = "0.0.0.0:9090")]
    supervisor_addr: String,
    #[arg(long, default_value_t = 1_800_000)]
    snapshot_interval_ms: u64,
    #[arg(long, default_value = "local_lease")]
    read_policy: String,
    #[arg(long, default_value_t = 1024)]
    max_inflight_writes: u32,
    #[arg(long, default_value_t = 262_144)]
    max_events: usize,
    /// How often the rendered process list is re-read.
    #[arg(long, default_value_t = 1_000)]
    poll_interval_ms: u64,
    /// Maximum number of children started from the process list.
    ///
    /// The operator validates the shard cap, but the supervisor enforces it too
    /// so a bad ConfigMap cannot exhaust the pod. Excess processes are reported
    /// under `rejected` instead of being started.
    #[arg(long, default_value_t = 20)]
    max_processes: usize,
    /// Initial restart delay after a child exits.
    #[arg(long, default_value_t = 1_000)]
    restart_base_ms: u64,
    /// Maximum restart delay; the delay doubles per consecutive crash.
    #[arg(long, default_value_t = 30_000)]
    restart_max_ms: u64,
    /// How long a child must stay up before its backoff resets to the base delay.
    #[arg(long, default_value_t = 60_000)]
    restart_stable_ms: u64,
    /// PID/state directory. Defaults to `<data-dir>/.supervisor`.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Optional cgroup v2 root. When set, each child is placed in `<root>/<name>`.
    ///
    /// This requires cgroup delegation inside the container; failures are logged and
    /// do not stop the child, because the pod/container limit remains the outer bound.
    #[arg(long)]
    cgroup_root: Option<PathBuf>,
    /// Optional `memory.max` value for each child cgroup (bytes or `max`).
    #[arg(long)]
    child_memory_max: Option<String>,
    /// Optional `cpu.max` value for each child cgroup, for example `50000 100000`.
    #[arg(long)]
    child_cpu_max: Option<String>,
    /// Optional per-child log directory. When set, stdout/stderr are mirrored to the
    /// container log and written to `<log-dir>/<name>.log` with size-based rotation.
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// Rotate a child log after this many bytes.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    log_max_bytes: u64,
    /// Number of rotated files to keep per child, including the active `.log`.
    #[arg(long, default_value_t = 4)]
    log_max_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketDataSpec {
    /// Aeron channel, with an optional `{pod_ip}` placeholder the supervisor
    /// expands from the `POD_IP` environment variable.
    channel: String,
    stream_base: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(default)]
    embedded: bool,
    #[serde(default = "default_market_data_heartbeat_ms")]
    heartbeat_ms: u64,
    #[serde(default = "default_market_data_status_every")]
    status_every: u64,
}

fn default_market_data_heartbeat_ms() -> u64 {
    1_000
}

fn default_market_data_status_every() -> u64 {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessSpec {
    name: String,
    slot: u16,
    http_port: u16,
    raft_port: u16,
    #[serde(default)]
    raft_id_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    market_data: Option<MarketDataSpec>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProcessConfig {
    #[serde(default)]
    processes: Vec<ProcessSpec>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunningProcess {
    name: String,
    slot: u16,
    http_port: u16,
    raft_port: u16,
    pid: u32,
    uptime_secs: u64,
    restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    isolation_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusSnapshot {
    desired: Vec<ProcessSpec>,
    running: Vec<RunningProcess>,
    rejected: Vec<RejectedProcess>,
    config_error: Option<String>,
    supervisor_uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RejectedProcess {
    name: String,
    reason: String,
}

struct Managed {
    spec: ProcessSpec,
    child: Child,
    started: Instant,
    restarts: u32,
    logs: Vec<JoinHandle<()>>,
    isolation_error: Option<String>,
}

struct SpawnedChild {
    child: Child,
    logs: Vec<JoinHandle<()>>,
    isolation_error: Option<String>,
}

/// A size-rotated file shared by one child's stdout and stderr.
struct RotatingLog {
    base: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: tokio::fs::File,
    size: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "match_supervisor=info,matchengine_raft=info".into()),
        )
        .with_target(false)
        .init();

    let opt = Opt::parse();
    info!(
        config = %opt.config.display(),
        id = opt.id,
        pod_fqdn = %opt.pod_fqdn,
        supervisor_addr = %opt.supervisor_addr,
        "supervisor starting"
    );

    let started = Instant::now();
    let status = Arc::new(Mutex::new(StatusSnapshot::default()));
    let http = tokio::spawn(serve(
        opt.supervisor_addr.clone(),
        status.clone(),
        opt.pod_fqdn.clone(),
    ));

    let mut desired: Vec<ProcessSpec> = Vec::new();
    let mut children: BTreeMap<String, Managed> = BTreeMap::new();
    let mut backoff: BTreeMap<String, Instant> = BTreeMap::new();
    let mut restart_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut last_config_error: Option<String> = None;
    let state_dir = opt
        .state_dir
        .clone()
        .unwrap_or_else(|| opt.data_dir.join(".supervisor"));
    cleanup_orphans(&state_dir, &opt.engine_bin).await;
    let mut shutdown = Box::pin(shutdown_signal());
    let mut ticker = tokio::time::interval(Duration::from_millis(opt.poll_interval_ms.max(100)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received; stopping children");
                break;
            }
            _ = ticker.tick() => {}
        }

        // A failed or transiently missing ConfigMap must never stop running children.
        match read_config(&opt.config) {
            Ok(next) => {
                if next != desired {
                    if next.len() > opt.max_processes {
                        warn!(
                            desired = next.len(),
                            max_processes = opt.max_processes,
                            "process list exceeds the supervisor cap; excess processes stay stopped"
                        );
                    }
                    info!(
                        processes = next.len(),
                        names = ?next.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                        "process list updated"
                    );
                    desired = next;
                }
                if last_config_error.take().is_some() {
                    info!(config = %opt.config.display(), "process list is readable again");
                }
            }
            Err(error) => {
                let message = format!("{}: {error}", opt.config.display());
                if last_config_error.as_deref() != Some(message.as_str()) {
                    warn!(%message, "cannot read process list; keeping current children");
                    last_config_error = Some(message);
                }
            }
        }

        let allowed = allowed_names(&desired, opt.max_processes);
        let rejected = rejected_processes(&desired, opt.max_processes);
        let desired_names: BTreeSet<String> =
            desired.iter().map(|spec| spec.name.clone()).collect();

        // Stop children that are no longer desired or exceed the current cap.
        let removed: Vec<String> = children
            .keys()
            .filter(|name| !desired_names.contains(*name) || !allowed.contains(*name))
            .cloned()
            .collect();
        for name in removed {
            if let Some(mut managed) = children.remove(&name) {
                info!(process = %name, pid = managed.child.id(), "stopping removed process");
                terminate(&mut managed.child).await;
                for task in managed.logs.drain(..) {
                    task.abort();
                }
            }
            backoff.remove(&name);
            restart_counts.remove(&name);
            remove_pid_file(&state_dir, &name);
        }

        // Reap crashed children. They are restarted on a later tick with backoff.
        let exited: Vec<(String, Option<std::process::ExitStatus>)> = children
            .iter_mut()
            .filter_map(|(name, managed)| match managed.child.try_wait() {
                Ok(Some(status)) => Some((name.clone(), Some(status))),
                Ok(None) => None,
                Err(error) => {
                    warn!(process = %name, %error, "try_wait failed");
                    None
                }
            })
            .collect();
        for (name, status) in exited {
            if let Some(mut managed) = children.remove(&name) {
                for task in managed.logs.drain(..) {
                    task.abort();
                }
            }
            remove_pid_file(&state_dir, &name);
            let attempts = {
                let attempts = restart_counts.entry(name.clone()).or_insert(0);
                *attempts += 1;
                *attempts
            };
            let delay = restart_delay(opt.restart_base_ms, opt.restart_max_ms, attempts);
            warn!(
                process = %name,
                ?status,
                attempts,
                delay_ms = delay.as_millis(),
                "child exited; restarting with exponential backoff"
            );
            backoff.insert(name, Instant::now() + delay);
        }

        // Reset the backoff exponent after a child has stayed up long enough.
        for (name, managed) in &children {
            if managed.started.elapsed() >= Duration::from_millis(opt.restart_stable_ms)
                && restart_counts.get(name).is_some_and(|count| *count > 0)
            {
                restart_counts.insert(name.clone(), 0);
                info!(process = %name, "child stable; restart backoff reset");
            }
        }

        // Detect a changed slot/port for an existing name without touching it.
        for spec in &desired {
            if let Some(managed) = children.get(&spec.name)
                && (managed.spec.http_port != spec.http_port || managed.spec.slot != spec.slot)
            {
                warn!(
                    process = %spec.name,
                    running_http = managed.spec.http_port,
                    desired_http = spec.http_port,
                    "process placement changed; keeping the running child (operator must remove/re-add it)"
                );
            }
        }

        // A changed market-data configuration is part of the child command line.
        // Restart the affected child so it starts publishing without a manual
        // removal/re-add; leave children whose placement changed untouched.
        let mut market_data_restarts = Vec::new();
        for spec in &desired {
            if let Some(managed) = children.get(&spec.name)
                && managed.spec.market_data != spec.market_data
            {
                info!(
                    process = %spec.name,
                    "market-data configuration changed; restarting child"
                );
                market_data_restarts.push(spec.name.clone());
            }
        }
        for name in market_data_restarts {
            if let Some(mut managed) = children.remove(&name) {
                terminate(&mut managed.child).await;
                for task in managed.logs.drain(..) {
                    task.abort();
                }
            }
            remove_pid_file(&state_dir, &name);
            backoff.remove(&name);
            restart_counts.remove(&name);
        }

        // Start missing children, up to the cap.
        for spec in desired.iter().filter(|spec| allowed.contains(&spec.name)) {
            if children.contains_key(&spec.name) {
                continue;
            }
            if let Some(until) = backoff.get(&spec.name)
                && Instant::now() < *until
            {
                continue;
            }
            match spawn_child(&opt, spec).await {
                Ok(spawned) => {
                    let pid = spawned.child.id().unwrap_or_default();
                    write_pid_file(&state_dir, &spec.name, pid);
                    let restarts = restart_counts.get(&spec.name).copied().unwrap_or(0);
                    info!(
                        process = %spec.name,
                        slot = spec.slot,
                        http_port = spec.http_port,
                        raft_port = spec.raft_port,
                        pid,
                        restarts,
                        isolation_error = ?spawned.isolation_error,
                        "child started"
                    );
                    children.insert(
                        spec.name.clone(),
                        Managed {
                            spec: spec.clone(),
                            child: spawned.child,
                            started: Instant::now(),
                            restarts,
                            logs: spawned.logs,
                            isolation_error: spawned.isolation_error,
                        },
                    );
                    backoff.remove(&spec.name);
                }
                Err(error) => {
                    let attempts = {
                        let attempts = restart_counts.entry(spec.name.clone()).or_insert(0);
                        *attempts += 1;
                        *attempts
                    };
                    let delay = restart_delay(opt.restart_base_ms, opt.restart_max_ms, attempts);
                    warn!(
                        process = %spec.name,
                        %error,
                        attempts,
                        delay_ms = delay.as_millis(),
                        "failed to start child"
                    );
                    backoff.insert(spec.name.clone(), Instant::now() + delay);
                }
            }
        }

        let running: Vec<RunningProcess> = children
            .iter()
            .map(|(name, managed)| RunningProcess {
                name: name.clone(),
                slot: managed.spec.slot,
                http_port: managed.spec.http_port,
                raft_port: managed.spec.raft_port,
                pid: managed.child.id().unwrap_or_default(),
                uptime_secs: managed.started.elapsed().as_secs(),
                restarts: managed.restarts,
                isolation_error: managed.isolation_error.clone(),
            })
            .collect();
        *status.lock().await = StatusSnapshot {
            desired: desired.clone(),
            running,
            rejected,
            config_error: last_config_error.clone(),
            supervisor_uptime_secs: started.elapsed().as_secs(),
        };
    }

    for (name, mut managed) in children {
        info!(process = %name, pid = managed.child.id(), "stopping child for shutdown");
        terminate(&mut managed.child).await;
        for task in managed.logs.drain(..) {
            task.abort();
        }
        remove_pid_file(&state_dir, &name);
    }
    http.abort();
    Ok(())
}

fn allowed_names(desired: &[ProcessSpec], max: usize) -> BTreeSet<String> {
    desired
        .iter()
        .take(max)
        .map(|spec| spec.name.clone())
        .collect()
}

fn rejected_processes(desired: &[ProcessSpec], max: usize) -> Vec<RejectedProcess> {
    desired
        .iter()
        .skip(max)
        .map(|spec| RejectedProcess {
            name: spec.name.clone(),
            reason: format!("supervisor max-processes cap is {max}"),
        })
        .collect()
}

fn restart_delay(base_ms: u64, max_ms: u64, attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(16);
    let delay = base_ms
        .saturating_mul(1u64 << shift)
        .min(max_ms.max(base_ms));
    Duration::from_millis(delay)
}

fn pid_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.pid"))
}

fn write_pid_file(state_dir: &Path, name: &str, pid: u32) {
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        warn!(%error, dir = %state_dir.display(), "cannot create supervisor state directory");
        return;
    }
    if let Err(error) = std::fs::write(pid_path(state_dir, name), pid.to_string()) {
        warn!(process = %name, %error, "cannot write child pid file");
    }
}

fn remove_pid_file(state_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(pid_path(state_dir, name));
}

/// Stops children left behind by a previous supervisor instance.
///
/// A clean shutdown removes the PID files itself. This path covers SIGKILL of
/// the supervisor: the engine children are reparented and keep holding their
/// ports and data directories unless the next supervisor stops them. The
/// command line is checked before signalling so a recycled PID is not killed.
async fn cleanup_orphans(state_dir: &Path, engine_bin: &Path) {
    let entries = match std::fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            warn!(%error, dir = %state_dir.display(), "cannot scan supervisor state directory");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok());
        let Some(pid) = pid.filter(|pid| *pid > 1) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if !process_alive(pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !process_matches(pid, name, engine_bin) {
            warn!(
                process = %name,
                pid,
                "stale pid file does not match an engine child; removing without signalling"
            );
            let _ = std::fs::remove_file(&path);
            continue;
        }
        warn!(
            process = %name,
            pid,
            "stopping orphaned child from a previous supervisor"
        );
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        for _ in 0..50 {
            if !process_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if process_alive(pid) {
            warn!(process = %name, pid, "orphaned child ignored SIGTERM; killing");
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn process_matches(pid: i32, name: &str, engine_bin: &Path) -> bool {
    let Some(command) = process_command_line(pid) else {
        return false;
    };
    let engine = engine_bin
        .file_name()
        .and_then(|file| file.to_str())
        .unwrap_or_default();
    if !engine.is_empty() && !command.contains(engine) {
        return false;
    }
    let args: Vec<&str> = command.split_whitespace().collect();
    args.windows(2)
        .any(|pair| pair[0] == "--instance-prefix" && pair[1] == name)
}

#[cfg(target_os = "linux")]
fn process_command_line(pid: i32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

#[cfg(not(target_os = "linux"))]
fn process_command_line(pid: i32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn read_config(path: &Path) -> io::Result<Vec<ProcessSpec>> {
    let bytes = std::fs::read(path)?;
    let config: ProcessConfig = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut names = BTreeSet::new();
    let mut http_ports = BTreeSet::new();
    for spec in &config.processes {
        if spec.name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process name must not be empty",
            ));
        }
        if !names.insert(spec.name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate process name {}", spec.name),
            ));
        }
        if !http_ports.insert(spec.http_port) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate HTTP port {}", spec.http_port),
            ));
        }
        if spec.http_port < 1024 || spec.raft_port != spec.http_port + 1000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "process {} has invalid ports http={} raft={}",
                    spec.name, spec.http_port, spec.raft_port
                ),
            ));
        }
    }
    let mut processes = config.processes;
    processes.sort_by_key(|spec| spec.slot);
    Ok(processes)
}

async fn spawn_child(opt: &Opt, spec: &ProcessSpec) -> io::Result<SpawnedChild> {
    let mut command = Command::new(&opt.engine_bin);
    command
        .arg("--id")
        .arg(opt.id.to_string())
        .arg("--id-offset")
        .arg(spec.raft_id_offset.to_string())
        .arg("--http-addr")
        .arg(format!("{}:{}", opt.pod_fqdn, spec.http_port))
        .arg("--data-dir")
        .arg(&opt.data_dir)
        .arg("--instance-prefix")
        .arg(&spec.name)
        .arg("--snapshot-interval-ms")
        .arg(opt.snapshot_interval_ms.to_string())
        .arg("--read-policy")
        .arg(&opt.read_policy)
        .arg("--max-inflight-writes")
        .arg(opt.max_inflight_writes.to_string())
        .arg("--max-events")
        .arg(opt.max_events.to_string());
    if let Some(market_data) = &spec.market_data {
        let pod_ip = std::env::var("POD_IP").unwrap_or_default();
        let channel = market_data.channel.replace("{pod_ip}", &pod_ip);
        command
            .arg("--market-data-aeron-channel")
            .arg(channel)
            .arg("--market-data-aeron-stream-base")
            .arg(market_data.stream_base.to_string())
            .arg("--market-data-aeron-heartbeat-ms")
            .arg(market_data.heartbeat_ms.to_string())
            .arg("--market-data-status-every")
            .arg(market_data.status_every.to_string());
        if market_data.embedded {
            command.arg("--market-data-aeron-embedded-driver");
        }
        if let Some(dir) = &market_data.dir {
            command.arg("--market-data-aeron-dir").arg(dir);
        }
    }
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    if let Ok(log) = std::env::var("RUST_LOG") {
        command.env("RUST_LOG", log);
    }

    let log_writer = if let Some(dir) = &opt.log_dir {
        let base = dir.join(format!("{}.log", spec.name));
        match RotatingLog::open(base, opt.log_max_bytes, opt.log_max_files).await {
            Ok(log) => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
                Some(Arc::new(Mutex::new(log)))
            }
            Err(error) => {
                warn!(
                    process = %spec.name,
                    %error,
                    "cannot open per-child log; falling back to the container log"
                );
                None
            }
        }
    } else {
        None
    };

    let mut child = command.spawn()?;
    let pid = child.id().unwrap_or_default();
    let isolation_error = if let Some(root) = &opt.cgroup_root {
        match apply_cgroup(
            root,
            &spec.name,
            pid,
            opt.child_memory_max.as_deref(),
            opt.child_cpu_max.as_deref(),
        ) {
            Ok(()) => None,
            Err(error) => {
                warn!(
                    process = %spec.name,
                    pid,
                    %error,
                    "cannot place child in its cgroup; continuing under the container limit"
                );
                Some(error.to_string())
            }
        }
    } else {
        None
    };

    let mut logs = Vec::new();
    if let Some(log) = log_writer {
        if let Some(stdout) = child.stdout.take() {
            logs.push(tokio::spawn(pump_log(stdout, log.clone(), true)));
        }
        if let Some(stderr) = child.stderr.take() {
            logs.push(tokio::spawn(pump_log(stderr, log, false)));
        }
    }
    Ok(SpawnedChild {
        child,
        logs,
        isolation_error,
    })
}

fn apply_cgroup(
    root: &Path,
    name: &str,
    pid: u32,
    memory_max: Option<&str>,
    cpu_max: Option<&str>,
) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    // Best effort: the parent cgroup must delegate the controllers for these writes to work.
    let _ = std::fs::write(root.join("cgroup.subtree_control"), "+memory +cpu");
    let dir = root.join(name);
    std::fs::create_dir_all(&dir)?;
    if let Some(max) = memory_max {
        std::fs::write(dir.join("memory.max"), max)?;
    }
    if let Some(max) = cpu_max {
        std::fs::write(dir.join("cpu.max"), max)?;
    }
    std::fs::write(dir.join("cgroup.procs"), pid.to_string())?;
    Ok(())
}

async fn pump_log<R>(mut reader: R, log: Arc<Mutex<RotatingLog>>, to_stdout: bool)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                {
                    let mut log = log.lock().await;
                    if let Err(error) = log.write(&buffer[..read]).await {
                        warn!(%error, "cannot write per-child log");
                        break;
                    }
                }
                if to_stdout {
                    let _ = tokio::io::stdout().write_all(&buffer[..read]).await;
                } else {
                    let _ = tokio::io::stderr().write_all(&buffer[..read]).await;
                }
            }
            Err(error) => {
                warn!(%error, "cannot read child output");
                break;
            }
        }
    }
}

impl RotatingLog {
    async fn open(base: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = base.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)
            .await?;
        let size = file.metadata().await?.len();
        Ok(Self {
            base,
            max_bytes: max_bytes.max(1),
            max_files: max_files.max(1),
            file,
            size,
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.rotate().await?;
        }
        self.file.write_all(bytes).await?;
        self.size = self.size.saturating_add(bytes.len() as u64);
        Ok(())
    }

    async fn rotate(&mut self) -> io::Result<()> {
        self.file.flush().await?;
        if self.max_files <= 1 {
            let _ = tokio::fs::remove_file(&self.base).await;
        } else {
            let oldest = rotated_path(&self.base, self.max_files - 1);
            let _ = tokio::fs::remove_file(oldest).await;
            for index in (1..self.max_files - 1).rev() {
                let from = rotated_path(&self.base, index);
                let to = rotated_path(&self.base, index + 1);
                let _ = tokio::fs::rename(from, to).await;
            }
            let _ = tokio::fs::rename(&self.base, rotated_path(&self.base, 1)).await;
        }
        self.file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.base)
            .await?;
        self.size = 0;
        Ok(())
    }
}

fn rotated_path(base: &Path, index: usize) -> PathBuf {
    let name = base
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("child.log");
    base.with_file_name(format!("{name}.{index}"))
}

async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        // The engine handles SIGTERM by flushing and shutting down its listeners.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .is_err()
    {
        warn!(pid = child.id(), "child did not stop in 30s; killing");
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

async fn serve(addr: String, status: Arc<Mutex<StatusSnapshot>>, pod_fqdn: String) {
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            warn!(%addr, %error, "cannot bind supervisor HTTP listener");
            return;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "cannot build readiness client");
            return;
        }
    };
    info!(%addr, "supervisor HTTP listening");
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            continue;
        };
        let status = status.clone();
        let client = client.clone();
        let pod_fqdn = pod_fqdn.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let (code, reason, body) = match path {
                "/health" => {
                    let snapshot = status.lock().await.clone();
                    (
                        200,
                        "OK",
                        json!({
                            "status": "ok",
                            "supervisorUptimeSecs": snapshot.supervisor_uptime_secs,
                            "desired": snapshot.desired.len(),
                            "running": snapshot.running.len(),
                        }),
                    )
                }
                "/ready" => {
                    let (code, body) = readiness(&status, &client, &pod_fqdn).await;
                    let reason = if code == 200 {
                        "OK"
                    } else {
                        "Service Unavailable"
                    };
                    (code, reason, body)
                }
                "/processes" => {
                    let snapshot = status.lock().await.clone();
                    (
                        200,
                        "OK",
                        serde_json::to_value(snapshot).unwrap_or(Value::Null),
                    )
                }
                _ => (404, "Not Found", json!({ "error": "not found" })),
            };
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

async fn readiness(
    status: &Arc<Mutex<StatusSnapshot>>,
    client: &reqwest::Client,
    pod_fqdn: &str,
) -> (u16, Value) {
    let snapshot = status.lock().await.clone();
    let mut failures = Vec::new();
    for spec in &snapshot.desired {
        if !snapshot.running.iter().any(|row| row.name == spec.name) {
            failures.push(format!("{} is not running", spec.name));
            continue;
        }
        let url = format!("http://{pod_fqdn}:{}/ready", spec.http_port);
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                failures.push(format!("{} returned HTTP {}", spec.name, response.status()))
            }
            Err(error) => failures.push(format!("{}: {error}", spec.name)),
        }
    }
    if failures.is_empty() {
        (
            200,
            json!({
                "ready": true,
                "desired": snapshot.desired.len(),
                "running": snapshot.running.len(),
            }),
        )
    } else {
        (
            503,
            json!({
                "ready": false,
                "desired": snapshot.desired.len(),
                "running": snapshot.running.len(),
                "failures": failures,
            }),
        )
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "match-supervisor-test-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restart_delay_doubles_and_caps() {
        assert_eq!(restart_delay(100, 1_000, 1), Duration::from_millis(100));
        assert_eq!(restart_delay(100, 1_000, 2), Duration::from_millis(200));
        assert_eq!(restart_delay(100, 1_000, 4), Duration::from_millis(800));
        assert_eq!(restart_delay(100, 1_000, 8), Duration::from_millis(1_000));
    }

    #[test]
    fn cgroup_files_are_written() {
        let root = temp_dir().join("cgroup");
        apply_cgroup(&root, "alpha", 1234, Some("1048576"), Some("50000 100000")).unwrap();
        let dir = root.join("alpha");
        assert_eq!(
            std::fs::read_to_string(dir.join("memory.max")).unwrap(),
            "1048576"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.max")).unwrap(),
            "50000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("cgroup.procs")).unwrap(),
            "1234"
        );
    }

    #[tokio::test]
    async fn rotating_log_keeps_the_requested_number_of_files() {
        let dir = temp_dir();
        let mut log = RotatingLog::open(dir.join("alpha.log"), 10, 3)
            .await
            .unwrap();
        for _ in 0..6 {
            log.write(b"12345").await.unwrap();
        }
        log.file.flush().await.unwrap();
        drop(log);

        assert!(dir.join("alpha.log").exists());
        assert!(dir.join("alpha.log.1").exists());
        assert!(dir.join("alpha.log.2").exists());
        assert!(
            !dir.join("alpha.log.3").exists(),
            "rotation must keep at most max_files files"
        );
    }

    #[test]
    fn parses_and_sorts_processes() {
        let dir = temp_dir();
        let path = dir.join("processes.json");
        std::fs::write(
            &path,
            r#"{"processes":[
                {"name":"gbpusd","slot":1,"httpPort":21002,"raftPort":22002,"raftIdOffset":4},
                {"name":"eurusd","slot":0,"httpPort":21001,"raftPort":22001}
            ]}"#,
        )
        .unwrap();
        let processes = read_config(&path).unwrap();
        assert_eq!(processes[0].name, "eurusd");
        assert_eq!(processes[0].raft_id_offset, 0);
        assert_eq!(processes[1].name, "gbpusd");
        assert_eq!(processes[1].raft_id_offset, 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_market_data_child_config() {
        let dir = temp_dir();
        let path = dir.join("processes.json");
        std::fs::write(
            &path,
            r#"{"processes":[{
                "name":"eurgbp","slot":0,"httpPort":21101,"raftPort":22101,
                "marketData":{
                    "channel":"aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1",
                    "streamBase":2001,
                    "dir":"/dev/shm/aeron-eurgbp",
                    "embedded":true,
                    "heartbeatMs":500,
                    "statusEvery":5
                }
            }]}"#,
        )
        .unwrap();
        let processes = read_config(&path).unwrap();
        let market_data = processes[0].market_data.as_ref().expect("marketData");
        assert_eq!(market_data.stream_base, 2001);
        assert_eq!(market_data.dir.as_deref(), Some("/dev/shm/aeron-eurgbp"));
        assert!(market_data.embedded);
        assert_eq!(market_data.heartbeat_ms, 500);
        assert_eq!(market_data.status_every, 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_duplicate_names_and_ports() {
        let dir = temp_dir();
        let path = dir.join("processes.json");
        std::fs::write(
            &path,
            r#"{"processes":[
                {"name":"eurusd","slot":0,"httpPort":21001,"raftPort":22001},
                {"name":"eurusd","slot":1,"httpPort":21002,"raftPort":22002}
            ]}"#,
        )
        .unwrap();
        assert!(read_config(&path).is_err());

        std::fs::write(
            &path,
            r#"{"processes":[
                {"name":"eurusd","slot":0,"httpPort":21001,"raftPort":22001},
                {"name":"gbpusd","slot":1,"httpPort":21001,"raftPort":22001}
            ]}"#,
        )
        .unwrap();
        assert!(read_config(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_nonstandard_raft_offset() {
        let dir = temp_dir();
        let path = dir.join("processes.json");
        std::fs::write(
            &path,
            r#"{"processes":[{"name":"eurusd","slot":0,"httpPort":21001,"raftPort":9999}]}"#,
        )
        .unwrap();
        assert!(read_config(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
