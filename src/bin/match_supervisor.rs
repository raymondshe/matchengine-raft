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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessSpec {
    name: String,
    slot: u16,
    http_port: u16,
    raft_port: u16,
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
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusSnapshot {
    desired: Vec<ProcessSpec>,
    running: Vec<RunningProcess>,
    config_error: Option<String>,
    supervisor_uptime_secs: u64,
}

struct Managed {
    spec: ProcessSpec,
    child: Child,
    started: Instant,
    restarts: u32,
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
    let mut last_config_error: Option<String> = None;
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

        // Stop children that are no longer desired.
        let desired_names: BTreeSet<String> =
            desired.iter().map(|spec| spec.name.clone()).collect();
        let removed: Vec<String> = children
            .keys()
            .filter(|name| !desired_names.contains(*name))
            .cloned()
            .collect();
        for name in removed {
            if let Some(mut managed) = children.remove(&name) {
                info!(process = %name, pid = managed.child.id(), "stopping removed process");
                terminate(&mut managed.child).await;
            }
            backoff.remove(&name);
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
            children.remove(&name);
            warn!(process = %name, ?status, "child exited; restarting with backoff");
            backoff.insert(name, Instant::now() + Duration::from_secs(1));
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

        // Start missing children.
        for spec in &desired {
            if children.contains_key(&spec.name) {
                continue;
            }
            if let Some(until) = backoff.get(&spec.name)
                && Instant::now() < *until
            {
                continue;
            }
            match spawn_child(&opt, spec) {
                Ok(child) => {
                    let pid = child.id().unwrap_or_default();
                    info!(
                        process = %spec.name,
                        slot = spec.slot,
                        http_port = spec.http_port,
                        raft_port = spec.raft_port,
                        pid,
                        "child started"
                    );
                    children.insert(
                        spec.name.clone(),
                        Managed {
                            spec: spec.clone(),
                            child,
                            started: Instant::now(),
                            restarts: 0,
                        },
                    );
                    backoff.remove(&spec.name);
                }
                Err(error) => {
                    warn!(process = %spec.name, %error, "failed to start child");
                    backoff.insert(spec.name.clone(), Instant::now() + Duration::from_secs(1));
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
            })
            .collect();
        *status.lock().await = StatusSnapshot {
            desired: desired.clone(),
            running,
            config_error: last_config_error.clone(),
            supervisor_uptime_secs: started.elapsed().as_secs(),
        };
    }

    for (name, mut managed) in children {
        info!(process = %name, pid = managed.child.id(), "stopping child for shutdown");
        terminate(&mut managed.child).await;
    }
    http.abort();
    Ok(())
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

fn spawn_child(opt: &Opt, spec: &ProcessSpec) -> io::Result<Child> {
    let mut command = Command::new(&opt.engine_bin);
    command
        .arg("--id")
        .arg(opt.id.to_string())
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
        .arg(opt.max_events.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Ok(log) = std::env::var("RUST_LOG") {
        command.env("RUST_LOG", log);
    }
    command.spawn()
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

    #[test]
    fn parses_and_sorts_processes() {
        let dir = std::env::temp_dir().join(format!(
            "match-supervisor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("processes.json");
        std::fs::write(
            &path,
            r#"{"processes":[
                {"name":"gbpusd","slot":1,"httpPort":21002,"raftPort":22002},
                {"name":"eurusd","slot":0,"httpPort":21001,"raftPort":22001}
            ]}"#,
        )
        .unwrap();
        let processes = read_config(&path).unwrap();
        assert_eq!(processes[0].name, "eurusd");
        assert_eq!(processes[1].name, "gbpusd");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_duplicate_names_and_ports() {
        let dir = std::env::temp_dir().join(format!(
            "match-supervisor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!(
            "match-supervisor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
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
