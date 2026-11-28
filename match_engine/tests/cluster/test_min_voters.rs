//! The no-fsync durability policy requires at least four voters.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use matchengine_raft::MatchEngineNode;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::ClientConfig;
use matchengine_raft::client::ClientError;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::BasicNode;
use openraft::Config;
use openraft::ServerState;
use openraft::SnapshotPolicy;
use openraft::async_runtime::WatchReceiver;
use openraft::type_config::TypeConfigExt;
use reqwest::StatusCode;

use crate::common::TempDir;
use crate::common::TestAddr;

type C = MatchEngineTypeConfig;

const ADDR: TestAddr = TestAddr::new(21925);

#[test]
fn no_fsync_requires_four_voters() {
    C::run(async {
        let temp = TempDir::new("min-voters");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("min-voters")
            .with_snapshot_interval(Duration::ZERO)
            .with_min_voters(4);
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::with_config(
            1,
            ADDR.to_string(),
            ClientConfig {
                max_retries: 0,
                max_forward_retries: 0,
                ..ClientConfig::default()
            },
        );

        // An empty init creates a one-voter cluster, below the minimum.
        let error = client.init().await.unwrap_err();
        assert!(
            matches!(error, ClientError::Api { status: 400, .. }),
            "{error:?}"
        );

        // Bypass the HTTP init guard to create a one-voter cluster, then prove
        // that writes are refused until the voter count reaches four.
        let nodes = BTreeMap::from([(
            1,
            BasicNode {
                addr: ADDR.to_string(),
            },
        )]);
        node.raft.initialize(nodes).await.unwrap();
        let deadline = Instant::now() + crate::common::wait_secs(10);
        loop {
            let metrics = node.raft.metrics();
            if metrics.borrow_watched().current_leader == Some(1) {
                break;
            }
            if Instant::now() > deadline {
                panic!("node 1 never became leader");
            }
            C::sleep(Duration::from_millis(50)).await;
        }

        let result = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            )))
            .await;
        assert!(
            matches!(result, Err(ClientError::Api { status: 503, .. })),
            "{result:?}"
        );

        let error = client
            .change_membership(&BTreeSet::from([1]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ClientError::Api { status: 400, .. }),
            "{error:?}"
        );

        node.shutdown().await.unwrap();
    });
}

// E06 needs four voters and its own port range. The single-node test above
// owns 21925 and the 219xx block is shared with many other cluster cases, so
// this live four-voter case uses 21751-21754. All respect
// `MATCHENGINE_TEST_PORT_OFFSET`.
const ADDR1: TestAddr = TestAddr::new(21751);
const ADDR2: TestAddr = TestAddr::new(21752);
const ADDR3: TestAddr = TestAddr::new(21753);
const ADDR4: TestAddr = TestAddr::new(21754);
const ADDRS: [TestAddr; 4] = [ADDR1, ADDR2, ADDR3, ADDR4];

fn live_config() -> Config {
    let mut config = default_raft_config();
    config.heartbeat_interval = 200;
    config.election_timeout_min = 500;
    config.election_timeout_max = 1000;
    config.heartbeat_min_interval = None;
    // Keep every entry in the WAL so the event tapes are comparable.
    config.snapshot_policy = SnapshotPolicy::Never;
    config
}

async fn start_live_node(
    id: u64,
    addr: TestAddr,
    storage: &StorageConfig,
) -> anyhow::Result<MatchEngineNode> {
    Ok(build_match_engine_node(id, addr.to_string(), live_config(), storage.clone()).await?)
}

fn live_client(id: u64, addr: TestAddr) -> MatchEngineClient<C> {
    MatchEngineClient::<C>::new(id, addr.to_string())
}

/// Initialize all four voters in one request: with `min_voters(4)` an empty
/// `/init` (a one-voter cluster) is correctly rejected by the same guard.
async fn init_four_voters(http: &reqwest::Client) -> anyhow::Result<()> {
    let membership: Vec<(u64, String)> = ADDRS
        .iter()
        .enumerate()
        .map(|(index, addr)| (index as u64 + 1, addr.to_string()))
        .collect();
    let response = http
        .post(format!("{}/init", ADDRS[0].http_url()))
        .json(&membership)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "four-voter /init failed: {status} {body}"
    );
    Ok(())
}

async fn wait_for_leader(
    clients: &[MatchEngineClient<C>],
    deadline: Instant,
) -> anyhow::Result<u64> {
    loop {
        for (index, client) in clients.iter().enumerate() {
            if let Ok(metrics) = client.metrics().await
                && metrics.current_leader == Some(index as u64 + 1)
                && metrics.state == ServerState::Leader
            {
                // Wait until the four-voter config replicated to every node:
                // a freshly initialized cluster can briefly have only node 1
                // configured, so an early write has no quorum.
                let mut configured = 0;
                for peer in clients {
                    if let Ok(peer_metrics) = peer.metrics().await
                        && peer_metrics.membership_config.voter_ids().count() == 4
                    {
                        configured += 1;
                    }
                }
                if configured == clients.len() {
                    return Ok(index as u64 + 1);
                }
            }
        }
        anyhow::ensure!(Instant::now() <= deadline, "no stable four-voter leader");
        C::sleep(Duration::from_millis(50)).await;
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
        anyhow::ensure!(
            Instant::now() <= deadline,
            "node did not reach sequence {expected}"
        );
        C::sleep(Duration::from_millis(50)).await;
    }
}

async fn place_buy(client: &MatchEngineClient<C>, price: f64) -> anyhow::Result<()> {
    client
        .write(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            price,
            1.0,
            0,
        )))
        .await??;
    Ok(())
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

async fn ready_is_ok(http: &reqwest::Client, addr: TestAddr) -> anyhow::Result<bool> {
    let response = http
        .get(format!("{}/ready", addr.http_url()))
        .send()
        .await?;
    Ok(response.status() == StatusCode::OK)
}

/// E06 — the four-voter floor is a *configured-voter* gate, not a live-pod
/// gate: it refuses a membership rewrite below four voters, but it does not
/// require every configured voter to be running. Once four voters are in the
/// config, one stopped voter leaves a 3/4 quorum that keeps committing.
#[test]
fn live_no_fsync_cluster_rejects_shrink_below_four_voters() {
    C::run(async {
        let temp = TempDir::new("min-voters-live");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("min-voters-live")
            .with_snapshot_interval(Duration::ZERO)
            .with_min_voters(4);
        let mut nodes = Vec::new();
        for (index, addr) in ADDRS.iter().enumerate() {
            nodes.push(Some(
                start_live_node(index as u64 + 1, *addr, &storage)
                    .await
                    .unwrap(),
            ));
        }
        let clients: Vec<_> = ADDRS
            .iter()
            .enumerate()
            .map(|(index, addr)| live_client(index as u64 + 1, *addr))
            .collect();

        let outcome = AssertUnwindSafe(async {
            let http = reqwest::Client::builder().no_proxy().build()?;
            init_four_voters(&http).await?;
            let deadline = Instant::now() + crate::common::wait_secs(15);
            let leader = wait_for_leader(&clients, deadline).await?;
            println!("min-voters-live: initial leader {leader}");
            let leader_client = &clients[(leader - 1) as usize];
            for (index, target) in clients.iter().enumerate() {
                if let Ok(metrics) = target.metrics().await {
                    println!(
                        "min-voters-live: node {} leader={:?} state={:?} voters={:?}",
                        index + 1,
                        metrics.current_leader,
                        metrics.state,
                        metrics.membership_config.voter_ids().collect::<Vec<_>>()
                    );
                }
            }

            for i in 0..10u64 {
                place_buy(leader_client, 100.0 - i as f64)
                    .await
                    .map_err(|error| anyhow::anyhow!("write {i} failed: {error:?}"))?;
            }
            println!("min-voters-live: 10 writes committed");
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                wait_for_sequence(client, "10", deadline).await?;
            }

            // The typed client and a raw HTTP request both hit the edge guard.
            // The guard's body names the voter floor ("at least 4 are required
            // while WAL fsync is disabled"); the configuration field itself,
            // `min_voters`, is not echoed in the HTTP error text.
            let shrink = BTreeSet::from([1u64, 2, 3]);
            let error = leader_client.change_membership(&shrink).await.unwrap_err();
            let detail = format!("{error:?}");
            anyhow::ensure!(
                matches!(&error, ClientError::Api { status: 400, .. })
                    && detail.contains("voters")
                    && detail.contains("at least 4"),
                "shrink below four voters was not rejected by the min-voter guard: {detail}"
            );
            let raw = http
                .post(format!(
                    "{}/change-membership",
                    ADDRS[(leader - 1) as usize].http_url()
                ))
                .json(&shrink)
                .send()
                .await?;
            let raw_status = raw.status();
            let raw_body = raw.text().await.unwrap_or_default();
            anyhow::ensure!(
                raw_status == StatusCode::BAD_REQUEST
                    && raw_body.contains("voters")
                    && raw_body.contains("at least 4"),
                "raw shrink was not rejected: {raw_status} {raw_body}"
            );
            println!("min-voters-live: shrink rejected ({raw_body})");

            let voters: BTreeSet<u64> = leader_client
                .metrics()
                .await?
                .membership_config
                .voter_ids()
                .collect();
            anyhow::ensure!(
                voters == BTreeSet::from([1, 2, 3, 4]),
                "rejected shrink changed membership: {voters:?}"
            );
            for addr in ADDRS {
                anyhow::ensure!(
                    ready_is_ok(&http, addr).await?,
                    "node {addr} was not ready after the rejected shrink"
                );
            }

            // Writes keep committing after the rejected change.
            place_buy(leader_client, 90.0).await?;
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for client in &clients {
                wait_for_sequence(client, "11", deadline).await?;
            }

            // Stop one configured voter. The floor is on the config, so the
            // remaining three voters still form a writable quorum.
            let victim = (1..=4u64).find(|id| *id != leader).unwrap();
            println!("min-voters-live: stopping voter {victim}, leader is {leader}");
            nodes[(victim - 1) as usize]
                .take()
                .unwrap()
                .shutdown()
                .await
                .unwrap();
            let voters: BTreeSet<u64> = leader_client
                .metrics()
                .await?
                .membership_config
                .voter_ids()
                .collect();
            anyhow::ensure!(
                voters == BTreeSet::from([1, 2, 3, 4]),
                "stopping a voter changed membership: {voters:?}"
            );
            place_buy(leader_client, 89.0).await?;
            let deadline = Instant::now() + crate::common::wait_secs(15);
            for (index, client) in clients.iter().enumerate() {
                if index as u64 + 1 != victim {
                    wait_for_sequence(client, "12", deadline).await?;
                }
            }
            for addr in ADDRS
                .iter()
                .filter(|addr| **addr != ADDRS[(victim - 1) as usize])
            {
                anyhow::ensure!(
                    ready_is_ok(&http, *addr).await?,
                    "surviving node {addr} was not ready after the stop"
                );
            }

            let live_clients: Vec<_> = clients
                .iter()
                .enumerate()
                .filter(|(index, _)| *index as u64 + 1 != victim)
                .map(|(_, client)| client)
                .collect();
            let tapes = event_tape(leader_client).await?;
            anyhow::ensure!(tapes.len() == 12, "expected 12 events, got {}", tapes.len());
            for client in &live_clients {
                anyhow::ensure!(
                    event_tape(client).await? == tapes,
                    "event tapes diverged after the quorum shrink"
                );
            }
            println!("min-voters-live: 12 committed events agree on every live node");
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        for node in nodes.into_iter().flatten() {
            if let Err(error) = node.shutdown().await {
                eprintln!("node shutdown failed: {error}");
            }
        }
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("test failed: {error:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
}
