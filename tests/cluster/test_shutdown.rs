//! Graceful SIGTERM shutdown for the containers-mode engine binary.
//!
//! The engine is PID 1 in a one-container pod, where the kernel ignores signals
//! with the default disposition. Without an explicit handler a rollout waits the
//! full termination grace period and then SIGKILLs the process.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::time::sleep;

use crate::common::{TempDir, TestAddr};

const ADDR: TestAddr = TestAddr::new(21940);

#[tokio::test]
async fn sigterm_stops_the_node_promptly() {
    let dir = TempDir::new("shutdown");
    let mut child = Command::new(env!("CARGO_BIN_EXE_matchengine-raft"))
        .args([
            "--id",
            "0",
            "--http-addr",
            &ADDR.http(),
            "--instance-prefix",
            "shutdown",
        ])
        .arg("--data-dir")
        .arg(dir.path())
        .env("RAFT_WAL_FSYNC_MS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start engine binary");

    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut healthy = false;
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("{}/health", ADDR.http_url()))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            && response.status().is_success()
        {
            healthy = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "engine did not answer /health before shutdown");

    let pid = child.id().expect("child pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let started = Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("engine ignored SIGTERM for 10s")
        .expect("wait for engine");
    assert!(
        status.success(),
        "engine exited with {status:?} instead of a clean shutdown"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "engine shutdown took {:?}",
        started.elapsed()
    );
}
