//! RAFT-E01: quorum loss and recovery.
//!
//! Existing failover tests kill exactly one voter. This case stops the leader
//! *and* another voter (RaftCore only, so HTTP stays up to observe the
//! survivor), proves the remaining 1-of-3 node refuses service instead of
//! guessing, then restarts one stopped voter on its original storage and
//! requires the cluster to converge on one history with no divergent commit.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::ClientConfig;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR1: TestAddr = TestAddr::new(21711);
const ADDR2: TestAddr = TestAddr::new(21712);
const ADDR3: TestAddr = TestAddr::new(21713);

fn test_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1000;
    config.heartbeat_min_interval = None;
    // Keep every committed entry in the WAL so the event tape is comparable.
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

async fn event_tape(client: &MatchEngineClient<C>) -> Vec<MatchEvent> {
    client
        .events(&EventsRequest {
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap()
        .events
}

async fn orders_json(client: &MatchEngineClient<C>) -> String {
    client.read(&"orderbook_orders".to_string()).await.unwrap()
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
fn quorum_loss_rejects_writes_and_recovers_without_divergence() {
    C::run(async {
        let temp = TempDir::new("quorum");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("quorum")
            .with_snapshot_interval(Duration::ZERO);

        let mut nodes = vec![
            Some(start_node(1, ADDR1, &storage).await.unwrap()),
            Some(start_node(2, ADDR2, &storage).await.unwrap()),
            Some(start_node(3, ADDR3, &storage).await.unwrap()),
        ];
        let clients = [client(1, ADDR1), client(2, ADDR2), client(3, ADDR3)];

        let result = std::panic::AssertUnwindSafe(async {
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
            {
                let deadline = Instant::now() + crate::common::wait_secs(15);
                for client in &clients {
                    wait_for_sequence(client, "20", deadline).await.unwrap();
                }
            }
            let leader_tape = event_tape(&clients[0]).await;
            let leader_orders = orders_json(&clients[0]).await;
            let survivor_applied_before = data_index(&raw_metrics(ADDR2).await?, "last_applied");

            // Stop the leader and one other voter. Only RaftCore stops: the
            // HTTP/gRPC listeners stay up so the survivor can be observed and
            // its `/write` endpoint can be exercised directly.
            nodes[0].as_ref().unwrap().raft.shutdown().await.unwrap();
            nodes[2].as_ref().unwrap().raft.shutdown().await.unwrap();

            // Two election timeouts (500-1000 ms) plus a margin: no 2-of-3
            // quorum exists, so node 2 must never campaign successfully.
            C::sleep(Duration::from_secs(3)).await;

            let survivor = &clients[1];
            let metrics = survivor.metrics().await?;
            println!(
                "quorum-lost survivor leader={:?} term={} applied={:?}",
                metrics.current_leader,
                metrics.current_term,
                metrics.last_applied.map(|id| id.index)
            );
            assert_ne!(
                Some(2),
                metrics.current_leader,
                "the 1-of-3 survivor must not elect itself"
            );

            // A low-retry client keeps the negative assertion quick and local:
            // no forwarding to the dead voters, no long backoff.
            let quiet = MatchEngineClient::<C>::with_config(
                2,
                ADDR2.to_string(),
                ClientConfig {
                    timeout: Duration::from_millis(500),
                    max_retries: 0,
                    max_forward_retries: 0,
                    ..Default::default()
                },
            );
            let single = MatchEngineRequest::place(Order::new(OrderSide::Buy, 50.0, 1.0, 0));
            let write = quiet.write(&single).await;
            assert!(
                !matches!(write, Ok(Ok(_))),
                "single write was accepted without quorum"
            );
            let batch = quiet.write_batch(&[single.clone(), single.clone()]).await;
            assert!(
                !matches!(batch, Ok(Ok(_))),
                "batch write was accepted without quorum"
            );

            let survivor_applied_after = data_index(&raw_metrics(ADDR2).await?, "last_applied");
            assert_eq!(
                survivor_applied_before, survivor_applied_after,
                "applied index advanced without a quorum"
            );
            assert_eq!(
                "20",
                survivor.read(&"orderbook_sequence".to_string()).await?,
                "sequence changed without a quorum"
            );
            // Local `/read` still serves the last committed book; the
            // linearizable `/events` path cannot succeed without a leader, so
            // the tape is compared only after the quorum is restored below.
            assert_eq!(leader_orders, orders_json(survivor).await);
            assert!(!leader_tape.is_empty());

            // Restart the stopped *non-leader* voter on its original storage
            // and id. Both dead RaftCores still hold their ports, so fully
            // shut down node 3 before rebinding its gRPC listener.
            nodes[2].take().unwrap().shutdown().await.unwrap();
            nodes[2] = Some(start_node(3, ADDR3, &storage).await.unwrap());

            let live = [client(2, ADDR2), client(3, ADDR3)];
            // Node 2 can keep reporting the *stale* leader id 1 until a real
            // election completes, so require both live nodes to agree on a
            // leader that is actually one of them.
            let deadline = Instant::now() + crate::common::wait_secs(20);
            loop {
                let mut leaders = Vec::new();
                for client in &live {
                    if let Ok(metrics) = client.metrics().await {
                        leaders.push(metrics.current_leader);
                    }
                }
                if leaders.len() == live.len()
                    && leaders[0].is_some()
                    && leaders[0] == leaders[1]
                    && (leaders[0] == Some(2) || leaders[0] == Some(3))
                {
                    break;
                }
                assert!(
                    Instant::now() <= deadline,
                    "no live leader after recovery: {leaders:?}"
                );
                C::sleep(Duration::from_millis(100)).await;
            }

            let deadline = Instant::now() + crate::common::wait_secs(20);
            for client in &live {
                wait_for_sequence(client, "20", deadline).await.unwrap();
            }
            // A fresh write gets exactly 21 and reaches both live nodes. The
            // first elected leader can step down before this write lands, so
            // follow the current leader through a member-aware client rather
            // than the id observed by the election loop.
            let leader_client = MatchEngineClient::<C>::with_members(
                vec![(2, ADDR2.to_string()), (3, ADDR3.to_string())],
                ClientConfig {
                    timeout: Duration::from_millis(1_000),
                    max_retries: 2,
                    max_forward_retries: 6,
                    ..Default::default()
                },
            );
            let response = leader_client
                .write(&MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    49.0,
                    1.0,
                    0,
                )))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Some(20), response.data.sequence);
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &live {
                wait_for_sequence(client, "21", deadline).await.unwrap();
            }

            // Durable state, not just log lines: identical book and tape on
            // both live nodes after the quorum loss and recovery.
            let tape = event_tape(&live[0]).await;
            assert_eq!(tape, event_tape(&live[1]).await);
            assert_eq!(orders_json(&live[0]).await, orders_json(&live[1]).await);
            assert_eq!(tape.len(), 21, "expected one event per committed order");

            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        for node in nodes.into_iter().flatten() {
            if let Err(error) = node.shutdown().await {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}

// ---------------------------------------------------------------------------
// RAFT-E13 / RAFT-E20
// ---------------------------------------------------------------------------

/// E13 uses five voters on dedicated ports; E20 uses four.
const ADDR_FIVE: [TestAddr; 5] = [
    TestAddr::new(21761),
    TestAddr::new(21762),
    TestAddr::new(21763),
    TestAddr::new(21764),
    TestAddr::new(21765),
];
const ADDR_FOUR: [TestAddr; 4] = [
    TestAddr::new(21766),
    TestAddr::new(21767),
    TestAddr::new(21768),
    TestAddr::new(21769),
];

/// E20 performs several elections; shorter timeouts keep it inside its ~20 s
/// budget without changing the safety properties under test.
fn fast_config() -> Config {
    let mut config = test_config();
    config.heartbeat_interval = 100;
    config.election_timeout_min = 250;
    config.election_timeout_max = 500;
    // Each stop removes WAL frames from a node that is still a voter, so the
    // leader must be allowed to reset that peer's replication progress instead
    // of panicking on the reverted tail.
    config.allow_log_reversion = Some(true);
    config
}

async fn start_node_fast(
    id: u64,
    addr: TestAddr,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), fast_config(), storage.clone()).await?)
}

/// Waits until a leader that is a member of `allowed` is visible from one of
/// the `clients` at the allowed ids. Stopped RaftCores keep serving stale
/// metrics, so only the explicitly live ids are consulted.
async fn leader_among(
    clients: &[MatchEngineClient<C>],
    allowed: &[u64],
    deadline: Instant,
) -> anyhow::Result<u64> {
    loop {
        for (index, client) in clients.iter().enumerate() {
            let id = index as u64 + 1;
            if !allowed.contains(&id) {
                continue;
            }
            if let Ok(metrics) = client.metrics().await
                && let Some(leader) = metrics.current_leader
                && allowed.contains(&leader)
            {
                return Ok(leader);
            }
        }
        anyhow::ensure!(Instant::now() <= deadline, "no leader among {allowed:?}");
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_all_sequence(
    clients: &[MatchEngineClient<C>],
    allowed: &[u64],
    expected: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        let mut ready = 0;
        for (index, client) in clients.iter().enumerate() {
            let id = index as u64 + 1;
            if !allowed.contains(&id) {
                continue;
            }
            if let Ok(value) = client.read(&"orderbook_sequence".to_string()).await
                && value == expected
            {
                ready += 1;
            }
        }
        if ready == allowed.len() {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() <= deadline,
            "only {ready}/{} live nodes reached sequence {expected}",
            allowed.len()
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

/// Writes one keyed order through whichever allowed node currently reports the
/// leader, retrying short transient failures across elections. Asserts the
/// exact sequence assigned, so a lost or duplicated commit is impossible to
/// miss.
async fn write_keyed(
    clients: &[MatchEngineClient<C>],
    allowed: &[u64],
    key: &str,
    price: f64,
    expected: u64,
    deadline: Instant,
) -> anyhow::Result<()> {
    let request = MatchEngineRequest::place_with_client_id(
        Order::new(OrderSide::Buy, price, 1.0, 0),
        key.to_string(),
    );
    loop {
        let leader = leader_among(clients, allowed, deadline).await?;
        match clients[(leader - 1) as usize].write(&request).await {
            Ok(Ok(response)) => {
                anyhow::ensure!(
                    response.data.sequence == Some(expected),
                    "key {key} got sequence {:?}, expected {expected}",
                    response.data.sequence
                );
                return Ok(());
            }
            Ok(Err(error)) => println!("write {key} rejected by {leader}: {error}"),
            Err(error) => println!("write {key} transport error via {leader}: {error}"),
        }
        anyhow::ensure!(Instant::now() <= deadline, "write {key} never committed");
        C::sleep(Duration::from_millis(50)).await;
    }
}

/// Initialize a fresh cluster with every listed voter in one `/init` request,
/// avoiding the extra learner/membership log entries.
async fn init_all_voters(addrs: &[TestAddr]) -> anyhow::Result<()> {
    let membership: Vec<(u64, String)> = addrs
        .iter()
        .enumerate()
        .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
        .collect();
    let http = reqwest::Client::builder().no_proxy().build()?;
    let response = http
        .post(format!("{}/init", addrs[0].http_url()))
        .json(&membership)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "init failed: {status} {body}");
    Ok(())
}

/// Highest entry index durably stored in a node's WAL segments.
fn persisted_wal_index(log_dir: &Path) -> u64 {
    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no WAL segment in {}", log_dir.display());
    files
        .iter()
        .flat_map(|path| read_wal_file::<C>(path).unwrap())
        .filter_map(|(_, record)| match record {
            WalRecord::Entry(entry) => Some(entry.log_id.index),
            WalRecord::Committed(_) => None,
        })
        .max()
        .unwrap_or(0)
}

/// Drops the last WAL frame that holds an `Entry` (plus any frames after it)
/// from the newest segment containing an entry. This models a power cut that
/// lost an unsynced appended entry while keeping every frame decodable.
fn truncate_last_wal_entry(log_dir: &Path) -> (u64, u64) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no WAL segment in {}", log_dir.display());

    let mut target: Option<(PathBuf, usize, u64)> = None;
    for path in &files {
        let frames = read_wal_file::<C>(path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        let mut starts = Vec::with_capacity(frames.len());
        let mut offset = 8usize; // MRFT header
        while offset + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let end = offset + 8 + len;
            if end > bytes.len() {
                break;
            }
            starts.push(offset);
            offset = end;
        }
        assert_eq!(frames.len(), starts.len(), "WAL frames/bytes disagree");
        for (frame_offset, (_, record)) in starts.iter().zip(frames.iter()) {
            if let WalRecord::Entry(entry) = record {
                let index = entry.log_id.index;
                if target.as_ref().is_none_or(|(_, _, best)| index >= *best) {
                    target = Some((path.clone(), *frame_offset, index));
                }
            }
        }
    }
    let (path, new_len, removed_index) =
        target.expect("a WAL segment with at least one Entry frame");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(new_len as u64)
        .unwrap();
    let kept = persisted_wal_index(log_dir);
    assert!(kept < removed_index, "truncation removed no entry");
    (removed_index, kept)
}

/// RAFT-E13: five voters survive the loss of two (quorum is three), refuse
/// service when a third voter is lost (two is not a quorum), and recover on the
/// original storage with no divergent history.
#[test]
fn five_voters_survive_two_then_lose_quorum_and_recover() {
    C::run(async {
        let temp = TempDir::new("quorum-five");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("quorum-five")
            .with_snapshot_interval(Duration::ZERO);
        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (index, addr) in ADDR_FIVE.iter().enumerate() {
            nodes.push(Some(
                start_node(index as u64 + 1, *addr, &storage).await.unwrap(),
            ));
        }
        let clients: Vec<_> = ADDR_FIVE
            .iter()
            .enumerate()
            .map(|(index, addr)| client(index as u64 + 1, *addr))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            // Library callers default to min_voters=1, so a single-node init
            // followed by learner promotion is enough to form five voters.
            clients[0].init().await.unwrap().unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(15);
            assert_eq!(1, leader_among(&clients, &[1], deadline).await.unwrap());
            for id in 2..=5u64 {
                clients[0]
                    .add_learner((id, ADDR_FIVE[(id - 1) as usize].to_string()))
                    .await
                    .unwrap()
                    .unwrap();
            }
            clients[0]
                .change_membership(&BTreeSet::from([1, 2, 3, 4, 5]))
                .await
                .unwrap()
                .unwrap();

            for i in 0..20u64 {
                place_buy(&clients[0], 100.0 - i as f64 * 0.1).await;
            }
            let deadline = Instant::now() + crate::common::wait_secs(20);
            wait_all_sequence(&clients, &[1, 2, 3, 4, 5], "20", deadline)
                .await
                .unwrap();

            // Lose the leader and one follower: 3 of 5 voters remain, which is
            // exactly the quorum needed to keep committing.
            nodes[0].as_ref().unwrap().raft.shutdown().await.unwrap();
            nodes[4].as_ref().unwrap().raft.shutdown().await.unwrap();
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let new_leader = leader_among(&clients, &[2, 3, 4], deadline)
                .await
                .unwrap();
            assert!(
                (2..=4).contains(&new_leader),
                "leader after a 5-voter, 2-node loss must be a live voter, got {new_leader}"
            );
            println!("E13: 5 voters, leader {new_leader} survives two stopped nodes");
            for i in 0..10u64 {
                write_keyed(
                    &clients,
                    &[2, 3, 4],
                    &format!("five-survivor-{i}"),
                    50.0 - i as f64 * 0.1,
                    20 + i,
                    deadline,
                )
                .await
                .unwrap();
            }
            let deadline = Instant::now() + crate::common::wait_secs(20);
            wait_all_sequence(&clients, &[2, 3, 4], "30", deadline)
                .await
                .unwrap();

            // Stop the current leader as the third loss. The two survivors are
            // both followers, so neither can lead a committed quorum.
            nodes[(new_leader - 1) as usize]
                .as_ref()
                .unwrap()
                .raft
                .shutdown()
                .await
                .unwrap();
            let survivors: Vec<u64> = [2u64, 3, 4]
                .into_iter()
                .filter(|id| *id != new_leader)
                .collect();
            assert_eq!(2, survivors.len(), "expected exactly two live voters");
            C::sleep(Duration::from_secs(3)).await;
            for id in &survivors {
                let metrics = clients[(id - 1) as usize].metrics().await?;
                assert_ne!(
                    Some(*id),
                    metrics.current_leader,
                    "2-of-5 survivor {id} must not elect itself"
                );
            }
            let survivor_index = (survivors[0] - 1) as usize;
            let applied_before = data_index(&raw_metrics(ADDR_FIVE[survivor_index]).await?, "last_applied");
            let quiet = MatchEngineClient::<C>::with_config(
                survivors[0],
                ADDR_FIVE[survivor_index].to_string(),
                ClientConfig {
                    timeout: Duration::from_millis(500),
                    max_retries: 0,
                    max_forward_retries: 0,
                    ..Default::default()
                },
            );
            let single = MatchEngineRequest::place(Order::new(OrderSide::Buy, 50.0, 1.0, 0));
            assert!(
                !matches!(quiet.write(&single).await, Ok(Ok(_))),
                "single write accepted with only 2 of 5 voters"
            );
            assert!(
                !matches!(quiet.write_batch(&[single.clone(), single]).await, Ok(Ok(_))),
                "batch write accepted with only 2 of 5 voters"
            );
            let applied_after =
                data_index(&raw_metrics(ADDR_FIVE[survivor_index]).await?, "last_applied");
            assert_eq!(
                applied_before, applied_after,
                "applied index advanced without a 3-of-5 quorum"
            );
            for id in &survivors {
                assert_eq!(
                    "30",
                    clients[(id - 1) as usize]
                        .read(&"orderbook_sequence".to_string())
                        .await?,
                    "sequence advanced without a 3-of-5 quorum"
                );
            }
            println!("E13: 2 of 5 voters refuse writes; applied index and sequence frozen at 30");

            // Restore any two stopped voters (the old leader and node 5) on
            // their original ids and storage. Four live voters is a quorum.
            for id in [1u64, 5] {
                let index = (id - 1) as usize;
                nodes[index].take().unwrap().shutdown().await.unwrap();
                C::sleep(Duration::from_millis(100)).await;
                nodes[index] = Some(start_node(id, ADDR_FIVE[index], &storage).await.unwrap());
            }
            let mut live = vec![1u64, 5];
            live.extend(survivors.iter().copied());
            live.sort_unstable();
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let recovered_leader = leader_among(&clients, &live, deadline).await.unwrap();
            wait_all_sequence(&clients, &live, "30", deadline)
                .await
                .unwrap();
            write_keyed(
                &clients,
                &live,
                "five-recovered",
                49.0,
                30,
                deadline,
            )
            .await
            .unwrap();
            wait_all_sequence(&clients, &live, "31", deadline)
                .await
                .unwrap();

            let tape = event_tape(&clients[0]).await;
            assert_eq!(31, tape.len(), "one event per committed order");
            let orders = orders_json(&clients[0]).await;
            for id in live.iter().skip(1) {
                let index = (*id - 1) as usize;
                let other_tape = event_tape(&clients[index]).await;
                let other_orders = orders_json(&clients[index]).await;
                assert_eq!(tape, other_tape, "event tapes diverged on node {id}");
                assert_eq!(orders, other_orders, "order books diverged on node {id}");
            }
            println!(
                "E13: recovered leader {recovered_leader}, sequence 31 and 31 identical events on {:?}",
                live
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        for node in nodes.into_iter().flatten() {
            if let Err(error) = node.shutdown().await {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}

/// RAFT-E20: four no-fsync voters are stopped one at a time with different
/// unsynced WAL tails, then the whole cluster restarts. Raft must converge on
/// one history chosen by the quorum, with every acknowledged write intact.
///
/// With four voters a majority is three, so at most one node can be offline
/// while commits continue. Each node is therefore taken down at a different
/// write count, its tail is truncated, and it is brought back to restore the
/// quorum before the next batch. The final whole-cluster restart leaves the
/// last node's tail behind the committed prefix; three nodes still hold the
/// full history, so no acknowledged response can be lost.
#[test]
fn whole_cluster_restart_with_unsynced_tails_converges_to_one_history() {
    C::run(async {
        let temp = TempDir::new("quorum-tails");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("quorum-tails")
            .with_wal_fsync(None)
            .with_snapshot_interval(Duration::ZERO);
        let mut nodes: Vec<Option<MatchEngineNode>> = Vec::new();
        for (index, addr) in ADDR_FOUR.iter().enumerate() {
            nodes.push(Some(
                start_node_fast(index as u64 + 1, *addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients: Vec<_> = ADDR_FOUR
            .iter()
            .enumerate()
            .map(|(index, addr)| client(index as u64 + 1, *addr))
            .collect();

        let result = std::panic::AssertUnwindSafe(async {
            init_all_voters(&ADDR_FOUR).await?;
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let all = [1u64, 2, 3, 4];
            let mut acked: Vec<u64> = Vec::new();
            for sequence in 0..12u64 {
                write_keyed(
                    &clients,
                    &all,
                    &format!("tail-{sequence}"),
                    100.0 - sequence as f64,
                    sequence,
                    deadline,
                )
                .await?;
                acked.push(sequence);
            }
            wait_all_sequence(&clients, &all, "12", deadline).await?;

            // Node 1 goes down after 12 acked writes and loses one tail entry.
            nodes[0].take().unwrap().shutdown().await?;
            let (removed, kept) = truncate_last_wal_entry(&storage.log_dir(1));
            println!("E20: node 1 WAL tail {removed} -> {kept}");
            let live = [2u64, 3, 4];
            for sequence in 12..15u64 {
                write_keyed(
                    &clients,
                    &live,
                    &format!("tail-{sequence}"),
                    100.0 - sequence as f64,
                    sequence,
                    deadline,
                )
                .await?;
                acked.push(sequence);
            }
            wait_all_sequence(&clients, &live, "15", deadline).await?;

            // Node 2 goes down after 15 and loses a different slice of its tail.
            nodes[1].take().unwrap().shutdown().await?;
            let (removed, kept) = truncate_last_wal_entry(&storage.log_dir(2));
            println!("E20: node 2 WAL tail {removed} -> {kept}");
            C::sleep(Duration::from_millis(100)).await;
            nodes[0] = Some(start_node_fast(1, ADDR_FOUR[0], &storage).await?);
            let live = [1u64, 3, 4];
            wait_all_sequence(&clients, &live, "15", deadline).await?;
            for sequence in 15..18u64 {
                write_keyed(
                    &clients,
                    &live,
                    &format!("tail-{sequence}"),
                    100.0 - sequence as f64,
                    sequence,
                    deadline,
                )
                .await?;
                acked.push(sequence);
            }
            wait_all_sequence(&clients, &live, "18", deadline).await?;

            // Node 3 goes down after 18; node 2 returns with its truncated WAL.
            nodes[2].take().unwrap().shutdown().await?;
            let (removed, kept) = truncate_last_wal_entry(&storage.log_dir(3));
            println!("E20: node 3 WAL tail {removed} -> {kept}");
            C::sleep(Duration::from_millis(100)).await;
            nodes[1] = Some(start_node_fast(2, ADDR_FOUR[1], &storage).await?);
            let live = [1u64, 2, 4];
            wait_all_sequence(&clients, &live, "18", deadline).await?;
            for sequence in 18..21u64 {
                write_keyed(
                    &clients,
                    &live,
                    &format!("tail-{sequence}"),
                    100.0 - sequence as f64,
                    sequence,
                    deadline,
                )
                .await?;
                acked.push(sequence);
            }
            wait_all_sequence(&clients, &live, "21", deadline).await?;

            // Node 4 goes down after 21 with its own tail loss, then node 3
            // returns. The last node is deliberately left behind the commit.
            nodes[3].take().unwrap().shutdown().await?;
            let (removed, kept) = truncate_last_wal_entry(&storage.log_dir(4));
            println!("E20: node 4 WAL tail {removed} -> {kept}");
            C::sleep(Duration::from_millis(100)).await;
            nodes[2] = Some(start_node_fast(3, ADDR_FOUR[2], &storage).await?);
            let live = [1u64, 2, 3];
            wait_all_sequence(&clients, &live, "21", deadline).await?;

            let tails: Vec<u64> = (1..=4u64)
                .map(|id| persisted_wal_index(&storage.log_dir(id)))
                .collect();
            let longest = *tails.iter().max().unwrap();
            assert!(
                tails.iter().any(|tail| *tail < longest),
                "expected different unsynced tails before the restart, got {tails:?}"
            );
            println!("E20: persisted WAL indexes before whole-cluster restart: {tails:?}");

            // Whole-cluster restart: every node stops and reopens its own
            // storage and id. Which node wins the election is informational.
            for node in nodes.iter_mut() {
                if let Some(node) = node.take() {
                    node.shutdown().await?;
                }
            }
            C::sleep(Duration::from_millis(100)).await;
            for (index, addr) in ADDR_FOUR.iter().enumerate() {
                nodes[index] = Some(start_node_fast(index as u64 + 1, *addr, &storage).await?);
            }
            let deadline = Instant::now() + crate::common::wait_secs(20);
            let leader = leader_among(&clients, &all, deadline).await?;
            wait_all_sequence(&clients, &all, "21", deadline).await?;
            println!("E20: elected leader {leader} after the whole-cluster restart");

            // Safety: one history, no lost ack, no gaps, no duplicates.
            let tape = event_tape(&clients[0]).await;
            let sequences: Vec<u64> = tape.iter().map(|event| event.order_sequence).collect();
            assert_eq!(
                sequences, acked,
                "event tape does not match the acknowledged sequence prefix"
            );
            let expected = "21";
            for client in &clients {
                assert_eq!(
                    expected,
                    client.read(&"orderbook_sequence".to_string()).await?,
                    "final sequence diverged"
                );
                assert_eq!(tape, event_tape(client).await, "event tapes diverged");
            }
            let orders = orders_json(&clients[0]).await;
            assert_eq!(
                21,
                serde_json::from_str::<Vec<Order>>(&orders)?.len(),
                "final book does not hold every acknowledged order"
            );
            for client in clients.iter().skip(1) {
                assert_eq!(
                    orders,
                    orders_json(client).await,
                    "final order books diverged"
                );
            }
            println!("E20: 21 acked orders, identical tapes and books on all four nodes");
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        for node in nodes.into_iter().flatten() {
            if let Err(error) = node.shutdown().await {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}
