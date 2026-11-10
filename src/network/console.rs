//! Operational console: static WebUI plus a small JSON API.
//!
//! The console is internal-only and inherits the plaintext HTTP listener's
//! trust model. TLS, operator authentication and gateway authorization are
//! outside this process. The UI assets are embedded so the binary is
//! self-contained.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use actix_web::HttpRequest;
use actix_web::HttpResponse;
use actix_web::get;
use actix_web::http::StatusCode;
use actix_web::post;
use actix_web::web::Data;
use actix_web::web::Json;
use futures::StreamExt;
use openraft::Instant as _;
use openraft::async_runtime::WatchReceiver;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::MutexGuard;

use crate::MatchEngineNodeId;
use crate::app::MatchEngineApp;
use crate::match_result::EventId;
use crate::network::grpc::metrics::TransportMetricsSnapshot;
use crate::network::health;
use crate::network::health::LeadershipHealth;
use crate::network::health::StorageHealth;
use crate::order_book::Fixed;

const INDEX_HTML: &str = include_str!("../../web/console/index.html");
const APP_CSS: &str = include_str!("../../web/console/app.css");
const APP_JS: &str = include_str!("../../web/console/app.js");

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
                   form-action 'self'; frame-ancestors 'none'";
const ACTION_HEADER: &str = "x-console-action";
const MAX_RECENT_ACTIONS: usize = 50;
const PEER_TIMEOUT: Duration = Duration::from_millis(750);
const PEER_CONCURRENCY: usize = 8;

/// Shared console state: start time, one-at-a-time action lock and a small
/// in-memory action history.
#[derive(Clone, Debug)]
pub struct ConsoleState {
    started_at_ms: u64,
    action_lock: Arc<AsyncMutex<()>>,
    recent_actions: Arc<Mutex<VecDeque<ActionRecord>>>,
    peer_client: reqwest::Client,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleState {
    /// Creates console state for a newly built node.
    pub fn new() -> Self {
        Self {
            started_at_ms: now_ms(),
            action_lock: Arc::new(AsyncMutex::new(())),
            recent_actions: Arc::new(Mutex::new(VecDeque::new())),
            peer_client: reqwest::Client::builder()
                .no_proxy()
                .timeout(PEER_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// Shared client for peer console requests.
    pub fn peer_client(&self) -> &reqwest::Client {
        &self.peer_client
    }

    /// Unix milliseconds when the node was built.
    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    async fn action_guard(&self) -> Result<MutexGuard<'_, ()>, ()> {
        self.action_lock.try_lock().map_err(|_| ())
    }

    fn record(&self, action: ActionRecord) {
        let mut records = self.actions_guard();
        records.push_front(action);
        while records.len() > MAX_RECENT_ACTIONS {
            records.pop_back();
        }
    }

    fn recent(&self) -> Vec<ActionRecord> {
        self.actions_guard().iter().cloned().collect()
    }

    fn actions_guard(&self) -> std::sync::MutexGuard<'_, VecDeque<ActionRecord>> {
        self.recent_actions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One recent Raft action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub at_ms: u64,
    pub action: String,
    pub target: Option<MatchEngineNodeId>,
    pub result: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Stable node snapshot served by `GET /console/api/node`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub schema_version: u32,
    pub node_id: MatchEngineNodeId,
    pub version: String,
    pub started_at_ms: u64,
    pub http_addr: String,
    pub grpc_addr: String,
    pub healthy: bool,
    pub ready: bool,
    pub grpc_listening: bool,
    pub leadership: LeadershipHealth,
    pub storage: StorageHealth,
    pub raft: RaftSnapshot,
    pub transport: TransportMetricsSnapshot,
    pub events: EventsSnapshot,
    pub market_data: Option<MarketDataSnapshot>,
    pub orderbook: OrderbookSnapshot,
    pub capabilities: Vec<String>,
    pub errors: Vec<String>,
}

/// Raft log and membership state for the console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftSnapshot {
    pub last_log_index: Option<u64>,
    pub local_committed_index: Option<u64>,
    pub cluster_committed_index: Option<u64>,
    pub last_applied_index: Option<u64>,
    pub snapshot_index: Option<u64>,
    pub purged_index: Option<u64>,
    pub voters: Vec<MatchEngineNodeId>,
    pub learners: Vec<MatchEngineNodeId>,
    pub replication: Vec<ReplicationSnapshot>,
}

/// One leader replication target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationSnapshot {
    pub node_id: MatchEngineNodeId,
    pub matched_index: Option<u64>,
    pub lag: Option<u64>,
    pub last_ack_ms_ago: Option<u64>,
}

/// Replicated event-window state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsSnapshot {
    pub retained_events: usize,
    pub oldest_event_id: Option<EventId>,
    pub last_event_id: Option<EventId>,
    pub evictions_total: u64,
    pub retention_limit: usize,
    pub submission_evictions_total: u64,
    pub submission_retention_limit: usize,
}

/// Market-data tap counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDataSnapshot {
    pub publishing: bool,
    pub frames: u64,
    pub trades: u64,
    pub trade_messages: u64,
    pub bbo: u64,
    pub gaps: u64,
    pub dropped_frames: u64,
    pub queued_frames: u64,
    pub queue_capacity: u64,
    pub last_market_seq: u64,
    pub last_log_index: u64,
    pub last_ts_ms: u64,
}

/// Small order-book summary; never returns the full order list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookSnapshot {
    pub sequence: u64,
    pub spread: Option<Fixed>,
    pub last_trade_price: Option<Fixed>,
    pub bid_levels: usize,
    pub ask_levels: usize,
    pub parked_stops: usize,
}

/// Cluster aggregate served by `GET /console/api/cluster`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub local_node_id: MatchEngineNodeId,
    pub leader_id: Option<MatchEngineNodeId>,
    pub term: u64,
    pub nodes: Vec<ClusterNode>,
    pub warnings: Vec<String>,
}

/// One member in the cluster aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNode {
    pub node_id: MatchEngineNodeId,
    pub addr: String,
    pub role: String,
    pub local: bool,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub snapshot: Option<NodeSnapshot>,
    pub error: Option<String>,
}

/// Action response returned by the action endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResponse {
    pub accepted: bool,
    pub action: String,
    pub target: Option<MatchEngineNodeId>,
    pub node_id: MatchEngineNodeId,
    pub leader_id: Option<MatchEngineNodeId>,
    pub term: u64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct TransferLeaderAction {
    pub target: MatchEngineNodeId,
}

/// Purge this node's local log up to a snapshot-covered index.
///
/// `upto` defaults to the current snapshot index. OpenRaft clamps and may delay
/// the purge while replication is in flight.
#[derive(Debug, Deserialize)]
pub struct PurgeLogAction {
    #[serde(default)]
    pub upto: Option<u64>,
}

/// Node-local OpenRaft runtime switches.
///
/// All fields are optional; at least one must be present. The action is refused
/// on the leader so a maintenance toggle cannot break heartbeats/leases for the
/// whole cluster.
#[derive(Debug, Default, Deserialize)]
pub struct RuntimeConfigAction {
    #[serde(default)]
    pub tick: Option<bool>,
    #[serde(default)]
    pub heartbeat: Option<bool>,
    #[serde(default)]
    pub elect: Option<bool>,
    #[serde(default)]
    pub pre_vote: Option<bool>,
}

impl RuntimeConfigAction {
    fn is_empty(&self) -> bool {
        self.tick.is_none()
            && self.heartbeat.is_none()
            && self.elect.is_none()
            && self.pre_vote.is_none()
    }

    fn description(&self) -> String {
        let mut changes = Vec::new();
        if let Some(value) = self.tick {
            changes.push(format!("tick={value}"));
        }
        if let Some(value) = self.heartbeat {
            changes.push(format!("heartbeat={value}"));
        }
        if let Some(value) = self.elect {
            changes.push(format!("elect={value}"));
        }
        if let Some(value) = self.pre_vote {
            changes.push(format!("pre_vote={value}"));
        }
        changes.join(", ")
    }
}

/// Redirects `/console` to `/console/` so relative asset paths are stable.
#[get("/console")]
pub async fn redirect() -> HttpResponse {
    HttpResponse::Found()
        .insert_header(("Location", "/console/"))
        .finish()
}

/// Serves the embedded HTML shell.
#[get("/console/")]
pub async fn index() -> HttpResponse {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

/// Serves the embedded CSS.
#[get("/console/app.css")]
pub async fn css() -> HttpResponse {
    asset("text/css; charset=utf-8", APP_CSS)
}

/// Serves the embedded JavaScript.
#[get("/console/app.js")]
pub async fn js() -> HttpResponse {
    asset("text/javascript; charset=utf-8", APP_JS)
}

/// Returns the local node snapshot.
#[get("/console/api/node")]
pub async fn node(app: Data<MatchEngineApp>) -> HttpResponse {
    HttpResponse::Ok().json(collect_node_snapshot(&app).await)
}

/// Returns the local node plus every member in the local membership.
#[get("/console/api/cluster")]
pub async fn cluster(app: Data<MatchEngineApp>) -> HttpResponse {
    let local = collect_node_snapshot(&app).await;
    let metrics = app.raft.metrics().borrow_watched().clone();
    let membership = metrics.membership_config.membership();
    let voter_ids: BTreeSet<_> = membership.voter_ids().collect();
    let mut members: Vec<(MatchEngineNodeId, String, bool)> = membership
        .nodes()
        .map(|(id, member)| (*id, member.addr.clone(), voter_ids.contains(id)))
        .collect();

    let mut warnings = Vec::new();
    let initialized = !members.is_empty();
    if !initialized {
        warnings.push("cluster is not initialized".to_string());
        members.push((app.id, app.addr.clone(), false));
    }
    members.sort_by_key(|(id, _, _)| *id);

    let client = app.console.peer_client().clone();

    let local_id = app.id;
    let local_addr = app.addr.clone();
    let peers: Vec<_> = members
        .iter()
        .filter(|(id, _, _)| *id != local_id)
        .cloned()
        .collect();

    let fetched = futures::stream::iter(peers.into_iter().map(|(id, addr, voter)| {
        let client = client.clone();
        async move {
            let started = Instant::now();
            let result = match client
                .get(format!("http://{addr}/console/api/node"))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    match response.json::<NodeSnapshot>().await {
                        Ok(node_snapshot) => Ok(node_snapshot),
                        Err(error) => Err(format!("invalid node snapshot: {error}")),
                    }
                }
                Ok(response) => Err(format!("peer returned HTTP {}", response.status())),
                Err(error) => Err(error.to_string()),
            };
            (
                id,
                addr,
                voter,
                started.elapsed().as_millis() as u64,
                result,
            )
        }
    }))
    .buffer_unordered(PEER_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut nodes = Vec::with_capacity(members.len());
    for (id, addr, voter) in members {
        let role = if !initialized {
            "unknown"
        } else if voter {
            "voter"
        } else {
            "learner"
        };
        if id == local_id {
            nodes.push(ClusterNode {
                node_id: id,
                addr: local_addr.clone(),
                role: role.to_string(),
                local: true,
                reachable: true,
                latency_ms: Some(0),
                snapshot: Some(local.clone()),
                error: None,
            });
            continue;
        }

        let Some((_, _, _, latency_ms, result)) =
            fetched.iter().find(|(peer_id, _, _, _, _)| *peer_id == id)
        else {
            nodes.push(ClusterNode {
                node_id: id,
                addr,
                role: role.to_string(),
                local: false,
                reachable: false,
                latency_ms: None,
                snapshot: None,
                error: Some("peer result missing".to_string()),
            });
            continue;
        };

        match result {
            Ok(node_snapshot) => nodes.push(ClusterNode {
                node_id: id,
                addr,
                role: role.to_string(),
                local: false,
                reachable: true,
                latency_ms: Some(*latency_ms),
                snapshot: Some(node_snapshot.clone()),
                error: None,
            }),
            Err(error) => {
                warnings.push(format!("node {id}: {error}"));
                nodes.push(ClusterNode {
                    node_id: id,
                    addr,
                    role: role.to_string(),
                    local: false,
                    reachable: false,
                    latency_ms: Some(*latency_ms),
                    snapshot: None,
                    error: Some(error.clone()),
                });
            }
        }
    }

    if metrics.current_leader.is_none() {
        warnings.push("no cluster leader is known".to_string());
    }
    if !local.ready {
        warnings.push("local node is not ready".to_string());
    }

    let cluster_snapshot = ClusterSnapshot {
        schema_version: 1,
        generated_at_ms: now_ms(),
        local_node_id: local_id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        nodes,
        warnings,
    };
    let body = serde_json::to_vec(&cluster_snapshot).unwrap();
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body)
}

/// Returns recent in-memory action records.
#[get("/console/api/actions")]
pub async fn actions(app: Data<MatchEngineApp>) -> HttpResponse {
    HttpResponse::Ok().json(app.console.recent())
}

/// Gracefully transfers leadership to a voter. Leader-only.
#[post("/console/api/actions/transfer-leader")]
pub async fn transfer_leader(
    app: Data<MatchEngineApp>,
    request: HttpRequest,
    body: Json<TransferLeaderAction>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return action_error(
            StatusCode::BAD_REQUEST,
            "missing X-Console-Action header or cross-origin request",
        );
    }

    let target = body.target;
    let metrics = app.raft.metrics().borrow_watched().clone();
    let membership = metrics.membership_config.membership();

    if metrics.current_leader != Some(app.id) {
        return action_error(
            StatusCode::CONFLICT,
            format!(
                "this node is not the leader; current leader is {:?}",
                metrics.current_leader
            ),
        );
    }
    if target == app.id {
        return action_error(StatusCode::BAD_REQUEST, "target must be a different node");
    }
    let voter_ids: BTreeSet<_> = membership.voter_ids().collect();
    if !voter_ids.contains(&target) {
        return action_error(
            StatusCode::CONFLICT,
            format!("node {target} is not a voter in the current membership"),
        );
    }

    let target_lag = metrics
        .replication
        .as_ref()
        .and_then(|replication| replication.get(&target))
        .and_then(|matched| matched.as_ref())
        .zip(metrics.last_log_index)
        .map(|(matched, last)| last.saturating_sub(matched.index));
    if let Some(lag) = target_lag
        && lag > 0
    {
        return action_error(
            StatusCode::CONFLICT,
            format!("target node {target} is lagging by {lag} entries"),
        );
    }

    let guard = match app.console.action_guard().await {
        Ok(guard) => guard,
        Err(()) => {
            return action_error(
                StatusCode::TOO_MANY_REQUESTS,
                "another console action is in progress",
            );
        }
    };

    let result = app.raft.trigger().transfer_leader(target).await;
    let (accepted, message) = match &result {
        Ok(()) => (
            true,
            "leadership transfer accepted; poll the cluster view for the new leader".to_string(),
        ),
        Err(error) => (false, format!("leadership transfer failed: {error}")),
    };
    app.console.record(ActionRecord {
        at_ms: now_ms(),
        action: "transfer-leader".to_string(),
        target: Some(target),
        result: if accepted { "accepted" } else { "failed" }.to_string(),
        error: (!accepted).then(|| message.clone()),
        detail: None,
    });
    drop(guard);

    let response = ActionResponse {
        accepted,
        action: "transfer-leader".to_string(),
        target: Some(target),
        node_id: app.id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        message,
    };
    if accepted {
        HttpResponse::Accepted().json(response)
    } else {
        HttpResponse::ServiceUnavailable().json(response)
    }
}

/// Requests a local election with Pre-Vote enabled. Best-effort fallback.
#[post("/console/api/actions/elect")]
pub async fn elect(app: Data<MatchEngineApp>, request: HttpRequest) -> HttpResponse {
    if !action_headers_ok(&request) {
        return action_error(
            StatusCode::BAD_REQUEST,
            "missing X-Console-Action header or cross-origin request",
        );
    }

    let metrics = app.raft.metrics().borrow_watched().clone();
    let voter_ids: BTreeSet<_> = metrics.membership_config.membership().voter_ids().collect();
    if !voter_ids.contains(&app.id) {
        return action_error(
            StatusCode::CONFLICT,
            "this node is not a voter in the current membership",
        );
    }

    let guard = match app.console.action_guard().await {
        Ok(guard) => guard,
        Err(()) => {
            return action_error(
                StatusCode::TOO_MANY_REQUESTS,
                "another console action is in progress",
            );
        }
    };

    let result = app.raft.trigger().elect(true).await;
    let (accepted, message) = match &result {
        Ok(()) => (
            true,
            "Pre-Vote election requested; a healthy leader may reject it".to_string(),
        ),
        Err(error) => (false, format!("election request failed: {error}")),
    };
    app.console.record(ActionRecord {
        at_ms: now_ms(),
        action: "elect".to_string(),
        target: Some(app.id),
        result: if accepted { "accepted" } else { "failed" }.to_string(),
        error: (!accepted).then(|| message.clone()),
        detail: None,
    });
    drop(guard);

    let response = ActionResponse {
        accepted,
        action: "elect".to_string(),
        target: Some(app.id),
        node_id: app.id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        message,
    };
    if accepted {
        HttpResponse::Accepted().json(response)
    } else {
        HttpResponse::ServiceUnavailable().json(response)
    }
}

/// Requests a local snapshot.
#[post("/console/api/actions/snapshot")]
pub async fn snapshot(app: Data<MatchEngineApp>, request: HttpRequest) -> HttpResponse {
    if !action_headers_ok(&request) {
        return action_error(
            StatusCode::BAD_REQUEST,
            "missing X-Console-Action header or cross-origin request",
        );
    }

    let metrics = app.raft.metrics().borrow_watched().clone();
    let guard = match app.console.action_guard().await {
        Ok(guard) => guard,
        Err(()) => {
            return action_error(
                StatusCode::TOO_MANY_REQUESTS,
                "another console action is in progress",
            );
        }
    };

    let result = app.raft.trigger().snapshot().await;
    let (accepted, message) = match &result {
        Ok(()) => (true, "snapshot requested".to_string()),
        Err(error) => (false, format!("snapshot request failed: {error}")),
    };
    app.console.record(ActionRecord {
        at_ms: now_ms(),
        action: "snapshot".to_string(),
        target: None,
        result: if accepted { "accepted" } else { "failed" }.to_string(),
        error: (!accepted).then(|| message.clone()),
        detail: None,
    });
    drop(guard);

    let response = ActionResponse {
        accepted,
        action: "snapshot".to_string(),
        target: None,
        node_id: app.id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        message,
    };
    if accepted {
        HttpResponse::Accepted().json(response)
    } else {
        HttpResponse::ServiceUnavailable().json(response)
    }
}

/// Purges this node's local Raft log up to a snapshot-covered index.
///
/// The action is local: it never changes another node's log. OpenRaft refuses
/// to purge entries not covered by a snapshot and may delay the purge while a
/// replication task is still reading those entries.
#[post("/console/api/actions/purge-log")]
pub async fn purge_log(
    app: Data<MatchEngineApp>,
    request: HttpRequest,
    body: Json<PurgeLogAction>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return action_error(
            StatusCode::BAD_REQUEST,
            "missing X-Console-Action header or cross-origin request",
        );
    }

    let metrics = app.raft.metrics().borrow_watched().clone();
    let Some(snapshot_log) = metrics.snapshot else {
        return action_error(
            StatusCode::CONFLICT,
            "no snapshot exists; trigger a snapshot and wait for it to complete before purging",
        );
    };
    let upto = body.upto.unwrap_or(snapshot_log.index);
    if upto == 0 {
        return action_error(StatusCode::BAD_REQUEST, "purge index must be positive");
    }
    if upto > snapshot_log.index {
        return action_error(
            StatusCode::BAD_REQUEST,
            format!(
                "cannot purge index {upto}: the current snapshot only covers {}",
                snapshot_log.index
            ),
        );
    }

    let guard = match app.console.action_guard().await {
        Ok(guard) => guard,
        Err(()) => {
            return action_error(
                StatusCode::TOO_MANY_REQUESTS,
                "another console action is in progress",
            );
        }
    };
    let result = app.raft.trigger().purge_log(upto).await;
    let (accepted, message) = match &result {
        Ok(()) => (
            true,
            format!(
                "purge scheduled up to {upto} on this node; OpenRaft may delay it while replication is in flight"
            ),
        ),
        Err(error) => (false, format!("purge request failed: {error}")),
    };
    app.console.record(ActionRecord {
        at_ms: now_ms(),
        action: "purge-log".to_string(),
        target: None,
        result: if accepted { "accepted" } else { "failed" }.to_string(),
        error: (!accepted).then(|| message.clone()),
        detail: Some(format!("upto={upto}; snapshot={}", snapshot_log.index)),
    });
    drop(guard);

    let response = ActionResponse {
        accepted,
        action: "purge-log".to_string(),
        target: None,
        node_id: app.id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        message,
    };
    if accepted {
        HttpResponse::Accepted().json(response)
    } else {
        HttpResponse::ServiceUnavailable().json(response)
    }
}

/// Applies node-local OpenRaft runtime switches.
///
/// This is deliberately refused on the leader: disabling `tick` or `heartbeat`
/// on the leader can stop leases and replication for the whole cluster. Use the
/// transfer-leader action first, then tune the follower.
#[post("/console/api/actions/runtime-config")]
pub async fn runtime_config(
    app: Data<MatchEngineApp>,
    request: HttpRequest,
    body: Json<RuntimeConfigAction>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return action_error(
            StatusCode::BAD_REQUEST,
            "missing X-Console-Action header or cross-origin request",
        );
    }
    if body.is_empty() {
        return action_error(
            StatusCode::BAD_REQUEST,
            "runtime-config needs at least one of tick/heartbeat/elect/pre_vote",
        );
    }

    let metrics = app.raft.metrics().borrow_watched().clone();
    if metrics.current_leader == Some(app.id) {
        return action_error(
            StatusCode::CONFLICT,
            "runtime config is node-local and is refused on the leader; transfer leadership first",
        );
    }

    let guard = match app.console.action_guard().await {
        Ok(guard) => guard,
        Err(()) => {
            return action_error(
                StatusCode::TOO_MANY_REQUESTS,
                "another console action is in progress",
            );
        }
    };
    let runtime = app.raft.runtime_config();
    if let Some(value) = body.tick {
        runtime.tick(value);
    }
    if let Some(value) = body.heartbeat {
        runtime.heartbeat(value);
    }
    if let Some(value) = body.elect {
        runtime.elect(value);
    }
    if let Some(value) = body.pre_vote {
        runtime.pre_vote(value);
    }
    let detail = body.description();
    let message = format!("applied node-local runtime config: {detail}");
    app.console.record(ActionRecord {
        at_ms: now_ms(),
        action: "runtime-config".to_string(),
        target: Some(app.id),
        result: "accepted".to_string(),
        error: None,
        detail: Some(detail.clone()),
    });
    drop(guard);

    let response = ActionResponse {
        accepted: true,
        action: "runtime-config".to_string(),
        target: Some(app.id),
        node_id: app.id,
        leader_id: metrics.current_leader,
        term: metrics.current_term,
        message,
    };
    HttpResponse::Accepted().json(response)
}

async fn collect_node_snapshot(app: &MatchEngineApp) -> NodeSnapshot {
    let health = health::collect_health(app).await;
    let metrics = app.raft.metrics().borrow_watched().clone();
    let membership = metrics.membership_config.membership();
    let voters = membership.voter_ids().collect::<Vec<_>>();
    let learners = membership.non_write_voter_ids().collect::<Vec<_>>();
    let replication = metrics
        .replication
        .as_ref()
        .map(|replication| {
            replication
                .iter()
                .map(|(node_id, matched)| {
                    let matched_index = matched.map(|log_id| log_id.index);
                    let lag = metrics
                        .last_log_index
                        .zip(matched_index)
                        .map(|(last, matched)| last.saturating_sub(matched));
                    let last_ack_ms_ago = metrics
                        .heartbeat
                        .as_ref()
                        .and_then(|heartbeat| heartbeat.get(node_id))
                        .and_then(|instant| instant.as_ref())
                        .map(|instant| instant.elapsed().as_millis() as u64);
                    ReplicationSnapshot {
                        node_id: *node_id,
                        matched_index,
                        lag,
                        last_ack_ms_ago,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let (events, market_data, orderbook) = {
        let inner = app.state_machine_store.inner().lock().await;
        let events = EventsSnapshot {
            retained_events: inner.retained_event_count(),
            oldest_event_id: inner.oldest_event_id(),
            last_event_id: inner.last_event_id(),
            evictions_total: inner.events_evicted(),
            retention_limit: inner.event_retention_limit(),
            submission_evictions_total: inner.submissions_evicted(),
            submission_retention_limit: inner.submission_retention_limit(),
        };
        let market_data = inner.market_data.as_ref().map(|tap| {
            let stats = tap.stats();
            MarketDataSnapshot {
                publishing: stats.publishing.load(Ordering::Relaxed),
                frames: stats.frames.load(Ordering::Relaxed),
                trades: stats.trades.load(Ordering::Relaxed),
                trade_messages: stats.trade_messages.load(Ordering::Relaxed),
                bbo: stats.bbo.load(Ordering::Relaxed),
                gaps: stats.gaps.load(Ordering::Relaxed),
                dropped_frames: stats.dropped_frames.load(Ordering::Relaxed),
                queued_frames: stats.queued_frames.load(Ordering::Relaxed),
                queue_capacity: stats.queue_capacity.load(Ordering::Relaxed),
                last_market_seq: stats.last_market_seq.load(Ordering::Relaxed),
                last_log_index: stats.last_log_index.load(Ordering::Relaxed),
                last_ts_ms: stats.last_ts_ms.load(Ordering::Relaxed),
            }
        });
        let orderbook = OrderbookSnapshot {
            sequence: inner.orderbook.sequence,
            spread: inner.orderbook.spread(),
            last_trade_price: inner.orderbook.last_trade_price,
            bid_levels: count_price_levels(inner.orderbook.bids.keys().map(|key| key.price)),
            ask_levels: count_price_levels(inner.orderbook.asks.keys().map(|key| key.price)),
            parked_stops: inner.orderbook.stops.len(),
        };
        (events, market_data, orderbook)
    };

    NodeSnapshot {
        schema_version: 1,
        node_id: health.node_id,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at_ms: app.console.started_at_ms(),
        http_addr: health.http_addr.clone(),
        grpc_addr: health.grpc_addr.clone(),
        healthy: health.healthy,
        ready: health.ready,
        grpc_listening: health.grpc_listening,
        leadership: health.leadership.clone(),
        storage: health.storage.clone(),
        raft: RaftSnapshot {
            last_log_index: metrics.last_log_index,
            local_committed_index: metrics.local_committed.map(|log_id| log_id.index),
            cluster_committed_index: metrics.cluster_committed.map(|log_id| log_id.index),
            last_applied_index: metrics.last_applied.map(|log_id| log_id.index),
            snapshot_index: metrics.snapshot.map(|log_id| log_id.index),
            purged_index: metrics.purged.map(|log_id| log_id.index),
            voters,
            learners,
            replication,
        },
        transport: app.transport.snapshot(),
        events,
        market_data,
        orderbook,
        capabilities: vec![
            "console_v1".to_string(),
            "transfer_leader".to_string(),
            "purge_log".to_string(),
            "runtime_config".to_string(),
        ],
        errors: health.errors.clone(),
    }
}

fn action_headers_ok(request: &HttpRequest) -> bool {
    if request.headers().get(ACTION_HEADER).is_none() {
        return false;
    }
    if let Some(origin) = request.headers().get("origin") {
        let origin = origin.to_str().unwrap_or_default();
        let origin_host = origin
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
            .unwrap_or(origin);
        let host = request.connection_info().host().to_string();
        if origin_host != host {
            return false;
        }
    }
    true
}

fn action_error(status: StatusCode, message: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(serde_json::json!({
        "accepted": false,
        "error": message.into(),
    }))
}

fn asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(content_type)
        .insert_header(("Content-Security-Policy", CSP))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .insert_header(("Referrer-Policy", "no-referrer"))
        .insert_header(("Cache-Control", "no-cache"))
        .body(body)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Counts distinct adjacent prices in an ordered map iterator.
///
/// The order book is keyed by `(price, sequence)`, so `bids.len()` counts
/// resting orders, not price levels.
fn count_price_levels(prices: impl Iterator<Item = Fixed>) -> usize {
    let mut count = 0usize;
    let mut previous = None;
    for price in prices {
        if previous != Some(price) {
            count += 1;
            previous = Some(price);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_records_are_bounded_newest_first() {
        let state = ConsoleState::new();
        for sequence in 0..(MAX_RECENT_ACTIONS + 5) {
            state.record(ActionRecord {
                at_ms: sequence as u64,
                action: "snapshot".to_string(),
                target: None,
                result: "accepted".to_string(),
                error: None,
                detail: None,
            });
        }
        let recent = state.recent();
        assert_eq!(MAX_RECENT_ACTIONS, recent.len());
        assert_eq!(
            (MAX_RECENT_ACTIONS + 4) as u64,
            recent.first().unwrap().at_ms
        );
    }

    #[test]
    fn price_levels_are_not_order_count() {
        let prices = [
            Fixed::from(100),
            Fixed::from(100),
            Fixed::from(100),
            Fixed::from(99),
            Fixed::from(98),
            Fixed::from(98),
        ];
        assert_eq!(3, count_price_levels(prices.into_iter()));
    }
}
