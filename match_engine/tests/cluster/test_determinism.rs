//! RAFT-E14 / RAFT-E15: deterministic replicated history.
//!
//! E14 replays one order stream on two independent three-node clusters. One
//! loses its leader mid-stream; the other takes a snapshot, purges its WAL and
//! has one follower's unsynced tail truncated. The final event tapes must be
//! identical after normalising only the node-local Raft log index.
//!
//! E15 sends `PurgeExpired` with a `day_close_ms` boundary while leadership is
//! transferred. Every replica must expire exactly the same Day orders once.

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
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::store::MatchEngineRequest;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::type_config::TypeConfigExt;
use serde_json::Value;
use serde_json::json;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR_A: [TestAddr; 3] = [
    TestAddr::new(21771),
    TestAddr::new(21772),
    TestAddr::new(21773),
];
const ADDR_B: [TestAddr; 3] = [
    TestAddr::new(21774),
    TestAddr::new(21775),
    TestAddr::new(21776),
];
const ADDR_DAY: [TestAddr; 3] = [
    TestAddr::new(21777),
    TestAddr::new(21778),
    TestAddr::new(21779),
];

fn config(snapshot: &SnapshotPolicy) -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 150;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1_000;
    config.heartbeat_min_interval = None;
    config.snapshot_policy = snapshot.clone();
    config.max_in_snapshot_log_to_keep = 0;
    // E14 deliberately removes committed WAL frames from a follower that is
    // still a voter. The engine's production reversion watcher needs seconds
    // to arm `allow_next_revert`; the test opts into openraft's permanent
    // equivalent so recovery is immediate and deterministic.
    config.allow_log_reversion = Some(true);
    config
}

/// A three-node cluster plus the `TempDir` that owns its durable state.
struct Cluster {
    nodes: Vec<Option<MatchEngineNode>>,
    clients: Vec<MatchEngineClient<C>>,
    storage: StorageConfig,
    _temp: TempDir,
}

async fn start_cluster(label: &str, addrs: &[TestAddr], snapshot: SnapshotPolicy) -> Cluster {
    let temp = TempDir::new(label);
    let storage = StorageConfig::new(temp.path())
        .with_instance_prefix(label)
        .with_snapshot_interval(Duration::ZERO);
    let mut nodes = Vec::new();
    for (index, addr) in addrs.iter().enumerate() {
        nodes.push(Some(
            build_match_engine_node(
                index as u64 + 1,
                addr.to_string(),
                config(&snapshot),
                storage.clone(),
            )
            .await
            .unwrap(),
        ));
    }
    let clients = addrs
        .iter()
        .enumerate()
        .map(|(index, addr)| MatchEngineClient::<C>::new(index as u64 + 1, addr.to_string()))
        .collect();
    Cluster {
        nodes,
        clients,
        storage,
        _temp: temp,
    }
}

fn load_client(addrs: &[TestAddr]) -> MatchEngineClient<C> {
    let members = addrs
        .iter()
        .enumerate()
        .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
        .collect();
    MatchEngineClient::<C>::with_members(
        members,
        ClientConfig {
            timeout: Duration::from_millis(1_500),
            max_retries: 3,
            max_forward_retries: 6,
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
        },
    )
}

async fn init_cluster(cluster: &Cluster, addrs: &[TestAddr]) -> anyhow::Result<u64> {
    cluster.clients[0].init().await??;
    let deadline = Instant::now() + Duration::from_secs(15);
    let leader = leader_among(&cluster.clients, &[1, 2, 3], deadline).await?;
    anyhow::ensure!(
        leader == 1,
        "fresh cluster elected node {leader}, expected 1"
    );
    for id in 2..=3u64 {
        cluster.clients[0]
            .add_learner((id, addrs[(id - 1) as usize].to_string()))
            .await??;
    }
    cluster.clients[0]
        .change_membership(&BTreeSet::from([1, 2, 3]))
        .await??;
    wait_all_sequence(&cluster.clients, &[1, 2, 3], "0", deadline).await?;
    Ok(leader)
}

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
            "only {ready}/{} nodes reached sequence {expected}",
            allowed.len()
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn write_exact(
    client: &MatchEngineClient<C>,
    request: &MatchEngineRequest,
    expected: u64,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        match client.write(request).await {
            Ok(Ok(response)) => {
                anyhow::ensure!(
                    response.data.sequence == Some(expected),
                    "expected sequence {expected}, got {:?}",
                    response.data.sequence
                );
                return Ok(());
            }
            Ok(Err(error)) => println!("write rejected for sequence {expected}: {error}"),
            Err(error) => println!("transport error for sequence {expected}: {error}"),
        }
        anyhow::ensure!(
            Instant::now() <= deadline,
            "sequence {expected} never committed"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn event_tape(client: &MatchEngineClient<C>) -> anyhow::Result<Vec<MatchEvent>> {
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .events(&EventsRequest {
                after,
                limit: Some(1000),
                wait_ms: None,
            })
            .await??;
        events.extend(page.events);
        if !page.has_more {
            return Ok(events);
        }
        after = page.next_after;
        anyhow::ensure!(after.is_some(), "paged /events without a cursor");
    }
}

/// The only node-local field on an event is `event_id.log_index`: a new leader
/// appends its own initial entry, and a snapshot/purge changes the physical log
/// position. Normalise just that field, then compare the whole tape byte for
/// byte as JSON as well as field by field for a useful failure message.
fn normalise(event: &MatchEvent) -> Value {
    let mut value = serde_json::to_value(event).unwrap();
    value["event_id"]["log_index"] = json!(0);
    value
}

fn normalised_tape(tape: &[MatchEvent]) -> String {
    Value::Array(tape.iter().map(normalise).collect()).to_string()
}

/// `Order.timestamp` is stamped by the receiving HTTP node at intake, so it is
/// the other node-local/wall-clock field in the final book view.
fn normalised_orders(json: &str) -> String {
    let mut orders: Vec<Value> = serde_json::from_str(json).unwrap();
    for order in &mut orders {
        order["timestamp"] = json!(0);
    }
    Value::Array(orders).to_string()
}

fn assert_same_tape(left: &[MatchEvent], right: &[MatchEvent], label: &str) {
    assert_eq!(
        left.len(),
        right.len(),
        "{label}: event tape lengths differ ({} vs {})",
        left.len(),
        right.len()
    );
    for (position, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        assert_eq!(
            a.event_id.ordinal, b.event_id.ordinal,
            "{label}: ordinal at {position}"
        );
        assert_eq!(
            a.kind, b.kind,
            "{label}: kind at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.order_sequence, b.order_sequence,
            "{label}: order_sequence at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.side, b.side,
            "{label}: side at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.counterparty_sequence, b.counterparty_sequence,
            "{label}: counterparty_sequence at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.price, b.price,
            "{label}: price at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.volume, b.volume,
            "{label}: volume at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(
            a.quote_volume, b.quote_volume,
            "{label}: quote_volume at {position}: {a:?} vs {b:?}"
        );
        assert_eq!(a.taker_fee, b.taker_fee, "{label}: taker_fee at {position}");
        assert_eq!(a.maker_fee, b.maker_fee, "{label}: maker_fee at {position}");
        assert_eq!(a.error, b.error, "{label}: error set at {position}");
        assert_eq!(a.reason, b.reason, "{label}: reason at {position}");
    }
    assert_eq!(
        normalised_tape(left),
        normalised_tape(right),
        "{label}: normalised JSON event tapes differ"
    );
}

/// Deterministic 60-order stream: 30 resting buys followed by 30 sells that
/// cross them, so both clusters produce fills and a meaningful last trade.
fn stream_order(index: u64) -> (MatchEngineRequest, String) {
    let (order, kind) = if index < 30 {
        (
            Order::new(OrderSide::Buy, 100.0 - index as f64 * 0.1, 2.0, 0),
            "buy",
        )
    } else {
        (
            Order::new(OrderSide::Sell, 90.0 + (index - 30) as f64 * 0.1, 1.0, 0),
            "sell",
        )
    };
    let key = format!("determinism-{kind}-{index}");
    (
        MatchEngineRequest::place_with_client_id(order, key),
        kind.to_string(),
    )
}

/// Highest entry index durably stored in a node's WAL.
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

/// Removes the last WAL frame holding an `Entry` (and anything after it) from
/// the newest segment that has one. Copied from the late-join/failure helpers.
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
    let (path, new_len, removed) = target.expect("a WAL segment with an Entry frame");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(new_len as u64)
        .unwrap();
    let kept = persisted_wal_index(log_dir);
    assert!(kept < removed, "truncation removed no WAL entry");
    (removed, kept)
}

async fn wait_snapshot_purge(cluster: &Cluster, leader: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let metrics = cluster.clients[(leader - 1) as usize].metrics().await?;
        if let (Some(snapshot), Some(purged)) = (metrics.snapshot, metrics.purged)
            && purged.index >= snapshot.index
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() <= deadline,
            "snapshot/purge did not complete"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn shutdown_cluster(cluster: &mut Cluster) {
    for node in cluster.nodes.iter_mut() {
        if let Some(node) = node.take()
            && let Err(error) = node.shutdown().await
        {
            eprintln!("node shutdown failed: {error}");
        }
    }
}

#[test]
fn identical_order_stream_yields_identical_event_tapes_after_failover_snapshot_and_truncation() {
    C::run(async {
        let mut cluster_a = start_cluster("determinism-a", &ADDR_A, SnapshotPolicy::Never).await;
        let mut cluster_b =
            start_cluster("determinism-b", &ADDR_B, SnapshotPolicy::LogsSinceLast(10)).await;

        let outcome = std::panic::AssertUnwindSafe(async {
            let deadline = Instant::now() + Duration::from_secs(20);
            let leader_a = init_cluster(&cluster_a, &ADDR_A).await?;
            let leader_b = init_cluster(&cluster_b, &ADDR_B).await?;
            let writer_a = load_client(&ADDR_A);
            let writer_b = load_client(&ADDR_B);

            // First half of the stream into both clusters.
            for (sequence, index) in (0..30u64).enumerate() {
                let (request, _) = stream_order(index);
                write_exact(&writer_a, &request, sequence as u64, deadline).await?;
                write_exact(&writer_b, &request, sequence as u64, deadline).await?;
            }
            wait_all_sequence(&cluster_a.clients, &[1, 2, 3], "30", deadline).await?;
            wait_all_sequence(&cluster_b.clients, &[1, 2, 3], "30", deadline).await?;

            // Cluster A: kill the leader mid-stream and continue elsewhere.
            println!("E14: cluster A stops leader {leader_a} at sequence 30");
            cluster_a.nodes[(leader_a - 1) as usize]
                .take()
                .unwrap()
                .shutdown()
                .await?;
            let live_a: Vec<u64> = [1u64, 2, 3]
                .into_iter()
                .filter(|id| *id != leader_a)
                .collect();
            let new_leader_a = leader_among(&cluster_a.clients, &live_a, deadline).await?;
            println!("E14: cluster A new leader {new_leader_a}");

            // Cluster B: snapshot + purge, one more order to leave a fresh WAL
            // entry, then truncate a follower's tail and let it recover.
            cluster_b.nodes[(leader_b - 1) as usize]
                .as_ref()
                .unwrap()
                .raft
                .trigger()
                .snapshot()
                .await
                .unwrap();
            wait_snapshot_purge(&cluster_b, leader_b).await?;
            let (request, _) = stream_order(30);
            write_exact(&writer_b, &request, 30, deadline).await?;
            // Ensure the fresh post-purge entry reached the follower before its
            // WAL tail is cut, so there is an Entry frame to remove.
            wait_all_sequence(&cluster_b.clients, &[1, 2, 3], "31", deadline).await?;
            let follower = [1u64, 2, 3].into_iter().find(|id| *id != leader_b).unwrap();
            cluster_b.nodes[(follower - 1) as usize]
                .take()
                .unwrap()
                .shutdown()
                .await?;
            let (removed, kept) = truncate_last_wal_entry(&cluster_b.storage.log_dir(follower));
            println!("E14: cluster B follower {follower} WAL tail {removed} -> {kept}");
            C::sleep(Duration::from_millis(100)).await;
            cluster_b.nodes[(follower - 1) as usize] = Some(
                build_match_engine_node(
                    follower,
                    ADDR_B[(follower - 1) as usize].to_string(),
                    config(&SnapshotPolicy::LogsSinceLast(10)),
                    cluster_b.storage.clone(),
                )
                .await?,
            );
            wait_all_sequence(&cluster_b.clients, &[1, 2, 3], "31", deadline).await?;

            // Second half of the stream: cluster A starts at 30 (index 30 was
            // not written), cluster B skips index 30 (already written above).
            let indices_a: Vec<u64> = (30..60).collect();
            let indices_b: Vec<u64> = (31..60).collect();
            for (expected, index) in (30u64..).zip(indices_a) {
                let (request, _) = stream_order(index);
                write_exact(&writer_a, &request, expected, deadline).await?;
            }
            for (expected, index) in (31u64..).zip(indices_b) {
                let (request, _) = stream_order(index);
                write_exact(&writer_b, &request, expected, deadline).await?;
            }
            wait_all_sequence(&cluster_a.clients, &live_a, "60", deadline).await?;
            wait_all_sequence(&cluster_b.clients, &[1, 2, 3], "60", deadline).await?;

            // Durable comparison: one surviving cluster-A node against every
            // cluster-B node, with event_id.log_index normalised away.
            let reference = &cluster_a.clients[(live_a[0] - 1) as usize];
            let tape_a = event_tape(reference).await?;
            let orders_a = reference.read(&"orderbook_orders".to_string()).await?;
            let sequence_a = reference.read(&"orderbook_sequence".to_string()).await?;
            let last_a = reference
                .read(&"orderbook_last_trade_price".to_string())
                .await?;
            assert_eq!("60", sequence_a, "cluster A final sequence");
            assert_eq!(60, tape_a.len(), "cluster A event count");

            for client in &cluster_b.clients {
                let tape_b = event_tape(client).await?;
                assert_same_tape(&tape_a, &tape_b, "cluster A vs cluster B");
                assert_eq!(
                    normalised_orders(&orders_a),
                    normalised_orders(&client.read(&"orderbook_orders".to_string()).await?),
                    "final order books diverged across clusters (timestamps normalised)"
                );
                assert_eq!(
                    sequence_a,
                    client.read(&"orderbook_sequence".to_string()).await?,
                    "final sequences diverged across clusters"
                );
                assert_eq!(
                    last_a,
                    client
                        .read(&"orderbook_last_trade_price".to_string())
                        .await?,
                    "final last trade prices diverged across clusters"
                );
            }
            println!(
                "E14: 60 identical events, sequence 60 and last trade {last_a} on both clusters"
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        shutdown_cluster(&mut cluster_a).await;
        shutdown_cluster(&mut cluster_b).await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}

#[test]
fn day_close_across_a_leadership_transfer_expires_once_everywhere() {
    C::run(async {
        let mut cluster = start_cluster("day-transfer", &ADDR_DAY, SnapshotPolicy::Never).await;

        let outcome = std::panic::AssertUnwindSafe(async {
            let deadline = Instant::now() + Duration::from_secs(20);
            let leader = init_cluster(&cluster, &ADDR_DAY).await?;
            let writer = load_client(&ADDR_DAY);

            // Ten Day orders, all committed on every replica before the race.
            for index in 0..10u64 {
                let mut order = Order::new(OrderSide::Buy, 100.0 - index as f64, 1.0, 0);
                order.tif = TimeInForce::Day;
                write_exact(
                    &writer,
                    &MatchEngineRequest::place_with_client_id(order, format!("day-{index}")),
                    index,
                    deadline,
                )
                .await?;
            }
            wait_all_sequence(&cluster.clients, &[1, 2, 3], "10", deadline).await?;

            // Send the day-close purges around a leadership transfer. The
            // writer re-discovers the new leader from the member list.
            let purge_writer = writer;
            let target = if leader == 2 { 3u64 } else { 2u64 };
            let transfer = cluster.nodes[(leader - 1) as usize]
                .as_ref()
                .unwrap()
                .raft
                .trigger()
                .transfer_leader(target)
                .await;
            anyhow::ensure!(
                transfer.is_ok(),
                "transfer to {target} failed: {:?}",
                transfer.err()
            );
            for offset in 0..6u64 {
                write_exact(
                    &purge_writer,
                    &MatchEngineRequest::PurgeExpired {
                        now_ms: 1_000,
                        day_close_ms: Some(1_000),
                    },
                    10 + offset,
                    deadline,
                )
                .await?;
            }
            loop {
                let metrics = cluster.clients[(target - 1) as usize].metrics().await?;
                if metrics.current_leader == Some(target) {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() <= deadline,
                    "transfer to {target} did not settle"
                );
                C::sleep(Duration::from_millis(50)).await;
            }
            wait_all_sequence(&cluster.clients, &[1, 2, 3], "16", deadline).await?;
            println!("E15: leadership {leader} -> {target}; six purges committed");

            let tape = event_tape(&cluster.clients[0]).await?;
            assert_eq!(20, tape.len(), "10 rests + 10 expiries expected");
            let rested: Vec<u64> = tape
                .iter()
                .filter(|event| event.kind == MatchEventKind::Rested)
                .map(|event| event.order_sequence)
                .collect();
            let expired: Vec<u64> = tape
                .iter()
                .filter(|event| event.kind == MatchEventKind::Expired)
                .map(|event| event.order_sequence)
                .collect();
            assert_eq!(
                rested,
                (0..10).collect::<Vec<_>>(),
                "wrong Day orders rested"
            );
            assert_eq!(
                expired,
                (0..10).collect::<Vec<_>>(),
                "each Day order must expire exactly once"
            );
            let mut counts = std::collections::BTreeMap::new();
            for sequence in &expired {
                *counts.entry(*sequence).or_insert(0) += 1;
            }
            assert!(
                counts.values().all(|count| *count == 1),
                "an order expired more than once: {counts:?}"
            );

            for client in cluster.clients.iter().skip(1) {
                assert_same_tape(&tape, &event_tape(client).await?, "expiry tapes");
                assert_eq!(
                    "16",
                    client.read(&"orderbook_sequence".to_string()).await?,
                    "final sequence diverged after the transfer"
                );
                let orders: Vec<Order> =
                    serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await?)?;
                assert!(orders.is_empty(), "Day orders survived the close");
            }
            let orders: Vec<Order> = serde_json::from_str(
                &cluster.clients[0]
                    .read(&"orderbook_orders".to_string())
                    .await?,
            )?;
            assert!(orders.is_empty(), "Day orders survived the close");
            println!("E15: all three replicas expired sequences 0..9 exactly once");
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        shutdown_cluster(&mut cluster).await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}
