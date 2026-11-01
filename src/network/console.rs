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
        let mut records = self.recent_actions.lock().unwrap();
        records.push_front(action);
        while records.len() > MAX_RECENT_ACTIONS {
            records.pop_back();
        }
    }

    fn recent(&self) -> Vec<ActionRecord> {
        self.recent_actions
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
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

async fn collect_node_snapshot(app: &MatchEngineApp) -> NodeSnapshot {
    let health = health::collect_health(app).await;
    let metrics = app.raft.metrics().borrow_watched().clone();
    let membership = metrics.membership_config.membership();
    let voters = membership.voter_ids().collect::<Vec<_>>();
    let learners = membership.learner_ids().collect::<Vec<_>>();
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
            bid_levels: inner.orderbook.bids.len(),
            ask_levels: inner.orderbook.asks.len(),
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
        events,
        market_data,
        orderbook,
        capabilities: vec!["console_v1".to_string(), "transfer_leader".to_string()],
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
            });
        }
        let recent = state.recent();
        assert_eq!(MAX_RECENT_ACTIONS, recent.len());
        assert_eq!(
            (MAX_RECENT_ACTIONS + 4) as u64,
            recent.first().unwrap().at_ms
        );
    }
}
