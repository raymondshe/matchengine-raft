//! RAFT-E03: automatic log-reversion recovery, end to end through
//! `src/log_reversion.rs::spawn()`.
//!
//! A restarted no-fsync follower can lose its unsynced tail while the leader
//! still holds a match index at the newer tail. This test truncates complete
//! WAL frames from node 2's last segment, restarts it on the same storage, and
//! requires the *watcher* (not a test-side trigger) to detect the stale tail,
//! arm `allow_next_revert`, and let OpenRaft repair the follower. A slow
//! heartbeat keeps the first conflict well after the watcher's fast probe, so
//! the one-shot grant is armed before any conflict response can hit the
//! `debug_assert`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::entry::RaftEntry;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR1: TestAddr = TestAddr::new(21731);
const ADDR2: TestAddr = TestAddr::new(21732);
const ADDR3: TestAddr = TestAddr::new(21733);

const SAMPLE_ENV: &str = "MATCHENGINE_REVERSION_SAMPLE_MS";
const REQUIRED_ENV: &str = "MATCHENGINE_REVERSION_REQUIRED_SAMPLES";
const AUTO_ENV: &str = "MATCHENGINE_AUTO_RECOVER_REVERSION";
const TARGET_ENV: &str = "MATCHENGINE_REVERSION_TARGET";

/// Restores the process-wide watcher env vars on drop so other cluster tests
/// keep the production defaults.
struct ReversionEnv {
    sample: Option<OsString>,
    required: Option<OsString>,
    auto: Option<OsString>,
    target: Option<OsString>,
}

impl ReversionEnv {
    fn install_fast() -> Self {
        let previous = Self {
            sample: std::env::var_os(SAMPLE_ENV),
            required: std::env::var_os(REQUIRED_ENV),
            auto: std::env::var_os(AUTO_ENV),
            target: std::env::var_os(TARGET_ENV),
        };
        // SAFETY: edition 2024 marks process-wide env mutation unsafe. These
        // vars are set only while this test builds its nodes and restored
        // immediately afterwards; the watcher captures them at spawn time.
        unsafe {
            std::env::set_var(SAMPLE_ENV, "50");
            std::env::set_var(REQUIRED_ENV, "1");
            std::env::set_var(AUTO_ENV, "1");
            // The watcher runs on the leader (node 1 in this test). Filter by
            // its HTTP address so other tests' watchers keep production timing.
            std::env::set_var(TARGET_ENV, ADDR1.http());
        }
        previous
    }

    fn restore(&mut self) {
        for (key, value) in [
            (SAMPLE_ENV, self.sample.take()),
            (REQUIRED_ENV, self.required.take()),
            (AUTO_ENV, self.auto.take()),
            (TARGET_ENV, self.target.take()),
        ] {
            // SAFETY: see `install_fast`.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

impl Drop for ReversionEnv {
    fn drop(&mut self) {
        self.restore();
    }
}

/// In-memory `tracing` writer, installed as a *thread-local* default around a
/// current-thread runtime. Every task spawned by `build_match_engine_node`,
/// including the reversion watcher, is polled on that same thread and therefore
/// emits into this buffer — without racing the global subscriber another test
/// may have installed.
#[derive(Clone, Default)]
struct SharedLog(Arc<Mutex<Vec<u8>>>);

impl SharedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }

    fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }
}

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLog {
    type Writer = SharedLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn test_config() -> Config {
    let mut config = default_raft_config();
    // A long heartbeat gives the 50 ms watcher a deterministic head start over
    // the first replication conflict after node 2 returns.
    config.heartbeat_interval = 5000;
    config.heartbeat_min_interval = None;
    config.election_timeout_min = 20000;
    config.election_timeout_max = 20001;
    config.snapshot_policy = SnapshotPolicy::Never;
    config
}

async fn start_node(
    id: u64,
    addr: TestAddr,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), test_config(), storage.clone()).await?)
}

fn client(id: u64, addr: TestAddr) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(id, addr.to_string())
}

async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    deadline: Instant,
) -> anyhow::Result<u64> {
    loop {
        for client in clients {
            if let Ok(metrics) = client.metrics().await
                && let Some(leader) = metrics.current_leader
            {
                return Ok(leader);
            }
        }
        if Instant::now() > deadline {
            anyhow::bail!("cluster did not elect a leader");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_sequence(
    client: &MatchEngineClient<C>,
    expected: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
            && value == expected
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("node did not reach sequence {expected}");
        }
        C::sleep(Duration::from_millis(100)).await;
    }
}

async fn place_buy(client: &MatchEngineClient<C>, price: f64) {
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            price,
            1.0,
            0,
        )))
        .await
        .unwrap()
        .unwrap();
}

async fn event_tape(client: &MatchEngineClient<C>) -> String {
    serde_json::to_string(
        &client
            .events(&EventsRequest {
                limit: Some(1000),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap()
            .events,
    )
    .unwrap()
}

fn last_wal_file(log_dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    files.pop().expect("node has at least one WAL segment")
}

/// Drops the last `records` complete frames at a frame boundary (copied from
/// `test_failure.rs::truncate_last_wal_records`). `read_wal_file` returns byte
/// offsets, so the log indexes are read from the decoded `Entry` records.
fn truncate_last_wal_records(path: &Path, records: usize) -> (u64, Option<u64>) {
    fn last_entry_index(records: &[(u64, WalRecord<C>)]) -> Option<u64> {
        records.iter().rev().find_map(|(_, record)| match record {
            WalRecord::Entry(entry) => Some(entry.log_id().index),
            WalRecord::Committed(_) => None,
        })
    }

    let original = read_wal_file::<C>(path).unwrap();
    let last_index = last_entry_index(&original).expect("WAL has an entry");
    let bytes = std::fs::read(path).unwrap();

    let mut starts = Vec::with_capacity(original.len());
    let mut offset = 8; // MRFT header
    while offset + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset + 8 + len;
        if end > bytes.len() {
            break;
        }
        starts.push(offset);
        offset = end;
    }
    assert_eq!(original.len(), starts.len());

    let keep = original.len().saturating_sub(records);
    assert!(
        original[keep..]
            .iter()
            .any(|(_, record)| matches!(record, WalRecord::Entry(_))),
        "tail truncation removed only commit markers"
    );
    let new_len = starts.get(keep).copied().unwrap_or(8);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(new_len as u64)
        .unwrap();
    let kept = read_wal_file::<C>(path).unwrap();
    assert_eq!(keep, kept.len());
    (last_index, last_entry_index(&kept))
}

async fn raw_metrics(addr: TestAddr) -> anyhow::Result<Value> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let response = http.get(format!("http://{addr}/metrics")).send().await?;
    Ok(response.json::<Value>().await?)
}

fn data_index(metrics: &Value, field: &str) -> u64 {
    metrics["Ok"]["data_metrics"][field]["index"]
        .as_u64()
        .unwrap_or(0)
}

#[test]
fn automatic_reversion_watcher_repairs_a_truncated_follower_tail() {
    // A single-threaded runtime makes `with_default` reach every spawned task,
    // including the watcher that emits the line we assert on.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let logs = SharedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            let mut env = ReversionEnv::install_fast();
            let temp = TempDir::new("log-reversion");
            let storage = StorageConfig::new(temp.path())
                .with_instance_prefix("log-reversion")
                .with_snapshot_interval(Duration::ZERO);
            let addrs = [ADDR1, ADDR2, ADDR3];

            let mut nodes = Vec::new();
            for (index, addr) in addrs.iter().enumerate() {
                nodes.push(Some(
                    start_node((index + 1) as u64, *addr, &storage)
                        .await
                        .unwrap(),
                ));
            }
            // All three watchers captured the fast settings; restore the
            // process env so concurrently running tests keep their defaults.
            env.restore();

            let clients = [
                client(1, ADDR1),
                client(2, ADDR2),
                client(3, ADDR3),
            ];
            let body = std::panic::AssertUnwindSafe(async {
                clients[0].init().await.unwrap().unwrap();
                let deadline = Instant::now() + crate::common::wait_secs(15);
                assert_eq!(1, wait_for_leader(&clients, deadline).await.unwrap());
                clients[0]
                    .add_learner((2, ADDR2.to_string()))
                    .await
                    .unwrap()
                    .unwrap();
                clients[0]
                    .add_learner((3, ADDR3.to_string()))
                    .await
                    .unwrap()
                    .unwrap();
                clients[0]
                    .change_membership(&BTreeSet::from([1, 2, 3]))
                    .await
                    .unwrap()
                    .unwrap();
                for i in 0..20u64 {
                    place_buy(&clients[0], 100.0 - i as f64 * 0.1).await;
                }
                let deadline = Instant::now() + crate::common::wait_secs(15);
                for client in &clients {
                    wait_for_sequence(client, "20", deadline).await.unwrap();
                }

                // The leader still believes node 2 matched this index.
                let matched = clients[0]
                    .metrics()
                    .await?
                    .replication
                    .as_ref()
                    .and_then(|replication| replication.get(&2))
                    .and_then(|entry| entry.as_ref().map(|id| id.index))
                    .expect("leader has replication progress for node 2");
                assert!(matched >= 20, "unexpectedly low match index {matched}");

                // Stop node 2 and cut a few *complete* frames from its last
                // segment so its persisted tail is below the leader's match.
                nodes[1].take().unwrap().shutdown().await.unwrap();
                let wal = last_wal_file(&storage.log_dir(2));
                let (original, kept) = truncate_last_wal_records(&wal, 6);
                let kept = kept.unwrap();
                assert!(
                    kept < matched,
                    "truncated tail {kept} did not fall below match {matched} (was {original})"
                );
                println!("truncated node 2 WAL tail {original} -> {kept}; leader match {matched}");

                // Restart the same id/addr on the same storage. The leader's
                // match index is still stale-high; only the watcher can grant
                // the reversion before the first conflict response.
                nodes[1] = Some(start_node(2, ADDR2, &storage).await.unwrap());

                let deadline = Instant::now() + crate::common::wait_secs(15);
                loop {
                    if logs.contains("follower log reverted; allowing one replication reset") {
                        break;
                    }
                    if Instant::now() > deadline {
                        panic!(
                            "watcher never armed allow_next_revert; captured log:\n{}",
                            logs.text()
                        );
                    }
                    C::sleep(Duration::from_millis(50)).await;
                }

                // No false reset for the healthy peer: every arm line must be
                // for peer 2, never peer 3.
                for line in logs
                    .text()
                    .lines()
                    .filter(|line| line.contains("follower log reverted"))
                {
                    assert!(
                        line.contains("peer=2") && !line.contains("peer=3"),
                        "false reversion reset for a healthy peer: {line}"
                    );
                }

                // Drive the conflict through the granted reversion. The watcher
                // already sends a probe heartbeat on arming; an extra trigger
                // makes the repair independent of the 5 s heartbeat timer.
                nodes[0]
                    .as_ref()
                    .unwrap()
                    .raft
                    .trigger()
                    .heartbeat()
                    .await
                    .unwrap();

                let leader_commit = clients[0]
                    .metrics()
                    .await?
                    .last_log_index
                    .unwrap_or(0);
                let deadline = Instant::now() + crate::common::wait_secs(30);
                let recovered = loop {
                    let follower = raw_metrics(ADDR2).await?;
                    let last_log = data_index(&follower, "last_log");
                    let last_applied = data_index(&follower, "last_applied");
                    if last_log >= leader_commit && last_applied >= leader_commit {
                        break (last_log, last_applied);
                    }
                    if Instant::now() > deadline {
                        panic!(
                            "node 2 did not recover: log={last_log} applied={last_applied} leader={leader_commit}"
                        );
                    }
                    C::sleep(Duration::from_millis(100)).await;
                };
                println!(
                    "node 2 recovered log/applied {:?} against leader commit {leader_commit}",
                    recovered
                );

                let deadline = Instant::now() + crate::common::wait_secs(15);
                for client in &clients {
                    wait_for_sequence(client, "20", deadline).await.unwrap();
                }
                let tape = event_tape(&clients[0]).await;
                for client in &clients {
                    assert_eq!(tape, event_tape(client).await);
                }

                let response = clients[0]
                    .write(&MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        50.0,
                        1.0,
                        0,
                    )))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(Some(20), response.data.sequence);
                let deadline = Instant::now() + crate::common::wait_secs(15);
                for client in &clients {
                    wait_for_sequence(client, "21", deadline).await.unwrap();
                }
                let tape = event_tape(&clients[0]).await;
                for client in &clients {
                    assert_eq!(tape, event_tape(client).await);
                }

                Ok::<(), anyhow::Error>(())
            })
            .catch_unwind()
            .await;

            for node in nodes.into_iter().flatten() {
                if let Err(error) = node.shutdown().await {
                    eprintln!("node shutdown failed: {error}");
                }
            }
            match body {
                Ok(Ok(())) => {}
                Ok(Err(error)) => panic!("test failed: {error:?}"),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
    });
}
