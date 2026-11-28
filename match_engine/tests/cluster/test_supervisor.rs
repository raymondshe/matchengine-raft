//! Supervisor-mode add/remove proof: adding a child must not restart existing children.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::sleep;

const SUPERVISOR_ADDR: &str = "127.0.0.1:39090";

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "match-supervisor-it-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        seq
    ))
}

fn script(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("dummy-engine.sh");
    let content = format!(
        r#"#!/bin/sh
dir="{}"
name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --instance-prefix) name="$2"; shift 2 ;;
    *) shift ;;
  esac
done
echo $$ > "$dir/$name.pid"
while :; do sleep 1; done
"#,
        dir.display()
    );
    std::fs::write(&path, content).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn write_processes(path: &std::path::Path, processes: &[(&str, u16)]) {
    let processes: Vec<Value> = processes
        .iter()
        .enumerate()
        .map(|(slot, (name, port))| {
            json!({
                "name": name,
                "slot": slot,
                "httpPort": port,
                "raftPort": port + 1000,
            })
        })
        .collect();
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({ "processes": processes })).unwrap(),
    )
    .unwrap();
}

fn write_processes_with_market_data(
    path: &std::path::Path,
    processes: &[(&str, u16, Option<Value>)],
) {
    let processes: Vec<Value> = processes
        .iter()
        .enumerate()
        .map(|(slot, (name, port, market_data))| {
            let mut process = json!({
                "name": name,
                "slot": slot,
                "httpPort": port,
                "raftPort": port + 1000,
            });
            if let Some(market_data) = market_data {
                process["marketData"] = market_data.clone();
            }
            process
        })
        .collect();
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({ "processes": processes })).unwrap(),
    )
    .unwrap();
}

async fn status(client: &Client, addr: &str) -> Option<Value> {
    client
        .get(format!("http://{addr}/processes"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

async fn running(client: &Client, addr: &str) -> Option<Vec<String>> {
    let body = status(client, addr).await?;
    Some(
        body["running"]
            .as_array()?
            .iter()
            .filter_map(|row| row["name"].as_str().map(str::to_string))
            .collect(),
    )
}

async fn wait_for(client: &Client, addr: &str, names: &[&str], timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(current) = running(client, addr).await
            && current.len() == names.len()
            && names.iter().all(|name| current.iter().any(|n| n == name))
        {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn start_supervisor(
    dir: &std::path::Path,
    config: &std::path::Path,
    engine: &std::path::Path,
    addr: &str,
    extra: &[&str],
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_match_supervisor"));
    command
        .arg("--config")
        .arg(config)
        .arg("--id")
        .arg("1")
        .arg("--pod-fqdn")
        .arg("127.0.0.1")
        .arg("--data-dir")
        .arg(dir)
        .arg("--engine-bin")
        .arg(engine)
        .arg("--supervisor-addr")
        .arg(addr)
        .arg("--poll-interval-ms")
        .arg("100")
        .args(extra)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().expect("start supervisor")
}

async fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn changing_market_data_restarts_only_the_affected_child() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39191), ("beta", 39192)]);
    let addr = "127.0.0.1:39190";

    let mut supervisor = start_supervisor(&dir, &config, &engine, addr, &[]).await;
    let client = Client::builder()
        .timeout(crate::common::wait_secs(2))
        .build()
        .unwrap();
    assert!(
        wait_for(
            &client,
            addr,
            &["alpha", "beta"],
            crate::common::wait_secs(10)
        )
        .await,
        "children did not start"
    );
    assert!(
        wait_for_file(&dir.join("alpha.pid"), crate::common::wait_secs(5)).await,
        "alpha did not write its pid"
    );
    assert!(
        wait_for_file(&dir.join("beta.pid"), crate::common::wait_secs(5)).await,
        "beta did not write its pid"
    );
    let alpha_before = read_pid(&dir.join("alpha.pid")).expect("alpha pid");
    let beta_before = read_pid(&dir.join("beta.pid")).expect("beta pid");

    let market_data = json!({
        "channel": "aeron:udp?endpoint=239.1.2.1:24001|ttl=1",
        "dir": "/dev/shm/test-alpha",
        "embedded": true,
        "heartbeatMs": 1000,
        "statusEvery": 10,
        "streamBase": 2001,
    });
    write_processes_with_market_data(
        &config,
        &[("alpha", 39191, Some(market_data)), ("beta", 39192, None)],
    );

    let start = Instant::now();
    let alpha_after = loop {
        let current = read_pid(&dir.join("alpha.pid"));
        if let Some(current) = current
            && current != alpha_before
            && process_alive(current)
        {
            break current;
        }
        assert!(
            start.elapsed() < crate::common::wait_secs(10),
            "alpha did not restart after its market-data configuration changed"
        );
        sleep(Duration::from_millis(100)).await;
    };
    let beta_after = read_pid(&dir.join("beta.pid")).expect("beta pid after change");
    assert_eq!(
        beta_after, beta_before,
        "beta restarted when alpha's market-data configuration changed"
    );
    assert_ne!(alpha_after, alpha_before, "alpha kept its old pid");
    assert_eq!(
        unsafe { libc::kill(beta_before, 0) },
        0,
        "beta is not alive"
    );

    unsafe {
        libc::kill(supervisor.id().unwrap() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), supervisor.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn adding_a_process_does_not_restart_existing_children() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39101)]);

    let mut supervisor = start_supervisor(&dir, &config, &engine, SUPERVISOR_ADDR, &[]).await;
    let client = Client::builder()
        .timeout(crate::common::wait_secs(2))
        .build()
        .unwrap();

    assert!(
        wait_for(
            &client,
            SUPERVISOR_ADDR,
            &["alpha"],
            crate::common::wait_secs(10)
        )
        .await,
        "alpha did not start"
    );
    assert!(
        wait_for_file(&dir.join("alpha.pid"), crate::common::wait_secs(5)).await,
        "alpha did not write its pid"
    );
    let alpha_pid = std::fs::read_to_string(dir.join("alpha.pid")).expect("alpha pid");
    let alpha_pid = alpha_pid.trim().parse::<i32>().unwrap();

    write_processes(&config, &[("alpha", 39101), ("beta", 39102)]);
    assert!(
        wait_for(
            &client,
            SUPERVISOR_ADDR,
            &["alpha", "beta"],
            crate::common::wait_secs(10)
        )
        .await,
        "beta did not start"
    );
    assert!(
        wait_for_file(&dir.join("beta.pid"), crate::common::wait_secs(5)).await,
        "beta did not write its pid"
    );
    let alpha_after = std::fs::read_to_string(dir.join("alpha.pid"))
        .expect("alpha pid after add")
        .trim()
        .parse::<i32>()
        .unwrap();
    let beta_pid = std::fs::read_to_string(dir.join("beta.pid"))
        .expect("beta pid")
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(
        alpha_pid, alpha_after,
        "alpha was restarted when beta was added"
    );
    assert_eq!(unsafe { libc::kill(alpha_pid, 0) }, 0, "alpha is not alive");

    write_processes(&config, &[("beta", 39102)]);
    assert!(
        wait_for(
            &client,
            SUPERVISOR_ADDR,
            &["beta"],
            crate::common::wait_secs(10)
        )
        .await,
        "alpha was not removed"
    );
    let beta_after = std::fs::read_to_string(dir.join("beta.pid"))
        .expect("beta pid after remove")
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(
        beta_pid, beta_after,
        "beta was restarted when alpha was removed"
    );
    assert_ne!(
        unsafe { libc::kill(alpha_pid, 0) },
        0,
        "alpha is still running after removal"
    );

    unsafe {
        libc::kill(supervisor.id().unwrap() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), supervisor.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}

fn read_pid(path: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_for_restart(
    client: &Client,
    addr: &str,
    name: &str,
    old_pid: u32,
    timeout: Duration,
) -> Option<(u32, u32)> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(body) = status(client, addr).await
            && let Some(row) = body["running"]
                .as_array()?
                .iter()
                .find(|row| row["name"] == name)
        {
            let pid = row["pid"].as_u64().unwrap_or_default() as u32;
            let restarts = row["restarts"].as_u64().unwrap_or_default() as u32;
            if pid != 0 && pid != old_pid && restarts >= 1 {
                return Some((pid, restarts));
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn max_processes_rejects_excess_children() {
    const ADDR: &str = "127.0.0.1:39091";
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39111), ("beta", 39112)]);

    let mut supervisor = start_supervisor(
        &dir,
        &config,
        &engine,
        ADDR,
        &["--max-processes", "1", "--restart-base-ms", "100"],
    )
    .await;
    let client = Client::builder()
        .timeout(crate::common::wait_secs(2))
        .build()
        .unwrap();

    assert!(
        wait_for(&client, ADDR, &["alpha"], crate::common::wait_secs(10)).await,
        "alpha did not start"
    );
    sleep(Duration::from_millis(300)).await;
    let body = status(&client, ADDR).await.expect("status");
    let running: Vec<&str> = body["running"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(running, vec!["alpha"]);
    let rejected: Vec<&str> = body["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(rejected, vec!["beta"]);
    assert!(!dir.join("beta.pid").exists(), "beta must not be started");

    unsafe {
        libc::kill(supervisor.id().unwrap() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), supervisor.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn crashed_child_restarts_with_a_backoff_count() {
    const ADDR: &str = "127.0.0.1:39092";
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39121)]);

    let mut supervisor = start_supervisor(
        &dir,
        &config,
        &engine,
        ADDR,
        &[
            "--restart-base-ms",
            "100",
            "--restart-max-ms",
            "400",
            "--restart-stable-ms",
            "60000",
        ],
    )
    .await;
    let client = Client::builder()
        .timeout(crate::common::wait_secs(2))
        .build()
        .unwrap();

    assert!(
        wait_for(&client, ADDR, &["alpha"], crate::common::wait_secs(10)).await,
        "alpha did not start"
    );
    assert!(
        wait_for_file(&dir.join("alpha.pid"), crate::common::wait_secs(5)).await,
        "alpha did not write its pid"
    );
    let old_pid = read_pid(&dir.join("alpha.pid")).expect("alpha pid");
    unsafe {
        libc::kill(old_pid, libc::SIGKILL);
    }
    let (new_pid, restarts) = wait_for_restart(
        &client,
        ADDR,
        "alpha",
        old_pid as u32,
        crate::common::wait_secs(10),
    )
    .await
    .expect("alpha did not restart");
    assert_ne!(new_pid as i32, old_pid);
    assert!(restarts >= 1);

    unsafe {
        libc::kill(supervisor.id().unwrap() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), supervisor.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn startup_stops_orphans_from_a_killed_supervisor() {
    const FIRST_ADDR: &str = "127.0.0.1:39093";
    const SECOND_ADDR: &str = "127.0.0.1:39094";
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39131)]);
    let extra = ["--restart-base-ms", "100"];

    let mut first = start_supervisor(&dir, &config, &engine, FIRST_ADDR, &extra).await;
    let client = Client::builder()
        .timeout(crate::common::wait_secs(2))
        .build()
        .unwrap();
    assert!(
        wait_for(
            &client,
            FIRST_ADDR,
            &["alpha"],
            crate::common::wait_secs(10)
        )
        .await,
        "alpha did not start"
    );
    assert!(
        wait_for_file(&dir.join("alpha.pid"), crate::common::wait_secs(5)).await,
        "alpha did not write its pid"
    );
    let orphan_pid = read_pid(&dir.join("alpha.pid")).expect("alpha pid");

    unsafe {
        libc::kill(first.id().unwrap() as i32, libc::SIGKILL);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), first.wait()).await;
    assert!(
        process_alive(orphan_pid),
        "the child should outlive a SIGKILLed supervisor"
    );

    let mut second = start_supervisor(&dir, &config, &engine, SECOND_ADDR, &extra).await;
    let start = Instant::now();
    let mut replacement = None;
    while start.elapsed() < Duration::from_secs(15) {
        if let Some(pid) = read_pid(&dir.join("alpha.pid"))
            && pid != orphan_pid
            && process_alive(pid)
        {
            replacement = Some(pid);
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let replacement = replacement.expect("supervisor did not start a replacement child");
    assert_ne!(replacement, orphan_pid);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) && process_alive(orphan_pid) {
        sleep(Duration::from_millis(100)).await;
    }
    assert!(!process_alive(orphan_pid), "orphaned child is still alive");

    unsafe {
        libc::kill(second.id().unwrap() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(crate::common::wait_secs(10), second.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}
