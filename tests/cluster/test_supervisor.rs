//! Supervisor-mode add/remove proof: adding a child must not restart existing children.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::sleep;

const SUPERVISOR_ADDR: &str = "127.0.0.1:39090";

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "match-supervisor-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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
exec sleep 300
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

async fn running(client: &Client) -> Option<Vec<String>> {
    let body: Value = client
        .get(format!("http://{SUPERVISOR_ADDR}/processes"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        body["running"]
            .as_array()?
            .iter()
            .filter_map(|row| row["name"].as_str().map(str::to_string))
            .collect(),
    )
}

async fn wait_for(client: &Client, names: &[&str], timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(current) = running(client).await
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
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_match_supervisor"))
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
        .arg(SUPERVISOR_ADDR)
        .arg("--poll-interval-ms")
        .arg("100")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start supervisor")
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
async fn adding_a_process_does_not_restart_existing_children() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("processes.json");
    let engine = script(&dir);
    write_processes(&config, &[("alpha", 39101)]);

    let mut supervisor = start_supervisor(&dir, &config, &engine).await;
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    assert!(
        wait_for(&client, &["alpha"], Duration::from_secs(10)).await,
        "alpha did not start"
    );
    assert!(
        wait_for_file(&dir.join("alpha.pid"), Duration::from_secs(5)).await,
        "alpha did not write its pid"
    );
    let alpha_pid = std::fs::read_to_string(dir.join("alpha.pid")).expect("alpha pid");
    let alpha_pid = alpha_pid.trim().parse::<i32>().unwrap();

    write_processes(&config, &[("alpha", 39101), ("beta", 39102)]);
    assert!(
        wait_for(&client, &["alpha", "beta"], Duration::from_secs(10)).await,
        "beta did not start"
    );
    assert!(
        wait_for_file(&dir.join("beta.pid"), Duration::from_secs(5)).await,
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
        wait_for(&client, &["beta"], Duration::from_secs(10)).await,
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
    let _ = tokio::time::timeout(Duration::from_secs(10), supervisor.wait()).await;
    let _ = std::fs::remove_dir_all(dir);
}
