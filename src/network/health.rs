//! Health and readiness reporting for a running node.
//!
//! The HTTP and gRPC listeners are plaintext and unauthenticated by design;
//! this endpoint is intended for an internal orchestrator or gateway, not for
//! public exposure. It reports the gRPC Raft listener state, the node's Raft
//! role and the durable WAL/state-machine frontier.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use actix_web::HttpResponse;
use actix_web::get;
use actix_web::http::StatusCode;
use actix_web::web::Data;
use openraft::ServerState;
use openraft::alias::LogIdOf;
use openraft::async_runtime::WatchReceiver;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftStateMachine;
use serde::Deserialize;
use serde::Serialize;

use crate::MatchEngineNodeId;
use crate::MatchEngineTypeConfig;
use crate::app::MatchEngineApp;

/// Shared health flag for the gRPC Raft listener.
///
/// The listener is bound before Raft starts, so the flag begins as `true` and
/// is cleared when the tonic server future finishes. A dead listener is fatal
/// to the node (`MatchEngineNode::run` stops HTTP and Raft), but a health probe
/// that races the shutdown still sees the transition.
#[derive(Clone, Debug)]
pub struct GrpcListenerHealth {
    addr: String,
    listening: Arc<AtomicBool>,
}

impl GrpcListenerHealth {
    /// Creates a health flag for a listener that is already bound.
    pub fn new(addr: String) -> Self {
        Self {
            addr,
            listening: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The bound gRPC Raft address.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Whether the gRPC server task is still serving.
    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    /// Marks the listener as stopped. Called when the server future exits.
    pub fn mark_stopped(&self) {
        self.listening.store(false, Ordering::Release);
    }
}

/// One health snapshot returned by `GET /health` and `GET /ready`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// `"ok"` when the listener, Raft core and storage are usable.
    pub status: String,
    /// Liveness: the gRPC listener, Raft core and storage are healthy.
    pub healthy: bool,
    /// Readiness: `healthy` and a cluster leader is known.
    pub ready: bool,
    /// This node's Raft id.
    pub node_id: MatchEngineNodeId,
    /// The public HTTP API address.
    pub http_addr: String,
    /// The derived gRPC Raft transport address.
    pub grpc_addr: String,
    /// Whether the gRPC server task is still serving.
    pub grpc_listening: bool,
    /// Raft role and leader information.
    pub leadership: LeadershipHealth,
    /// Durable WAL and state-machine state.
    pub storage: StorageHealth,
    /// Non-fatal diagnostics; empty on a healthy node.
    pub errors: Vec<String>,
}

/// Raft leadership state as seen by this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeadershipHealth {
    /// `Leader`, `Follower`, `Candidate`, `Learner` or `Shutdown`.
    pub state: ServerState,
    /// Whether this node is the current leader.
    pub is_leader: bool,
    /// The current cluster leader, if one is known.
    pub leader_id: Option<MatchEngineNodeId>,
    /// The current Raft term.
    pub term: u64,
    /// Display form of the persisted vote.
    pub vote: String,
}

/// Durable storage state reported by the WAL and the state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealth {
    /// WAL state, or `None` when it could not be read.
    pub wal: Option<WalHealth>,
    /// State-machine applied state, or `None` when it could not be read.
    pub state_machine: Option<StateMachineHealth>,
    /// Last log index covered by a snapshot, from Raft metrics.
    pub snapshot: Option<LogIdOf<MatchEngineTypeConfig>>,
    /// Last log index purged from the WAL, from Raft metrics.
    pub purged: Option<LogIdOf<MatchEngineTypeConfig>>,
    /// Locally committed log id, from Raft metrics.
    pub local_committed: Option<LogIdOf<MatchEngineTypeConfig>>,
    /// Cluster-committed log id, from Raft metrics.
    pub cluster_committed: Option<LogIdOf<MatchEngineTypeConfig>>,
}

/// WAL frontier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalHealth {
    /// Last record in the durable WAL.
    pub last_log_id: Option<LogIdOf<MatchEngineTypeConfig>>,
    /// Last log id deleted by a purge.
    pub last_purged_log_id: Option<LogIdOf<MatchEngineTypeConfig>>,
}

/// State-machine frontier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMachineHealth {
    /// Last log id applied to the state machine.
    pub last_applied: Option<LogIdOf<MatchEngineTypeConfig>>,
}

/// Liveness probe: reports whether the node's listener, Raft core and storage
/// are usable. It returns `200` while the node can serve and `503` when a
/// component is unavailable. Leadership is reported but is not required for
/// liveness.
#[get("/health")]
pub async fn health(app: Data<MatchEngineApp>) -> HttpResponse {
    let report = collect_health(&app).await;
    let status = if report.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    HttpResponse::build(status).json(report)
}

/// Readiness probe: returns `200` only when the node is healthy and knows a
/// cluster leader, otherwise `503`. A follower with a known leader is ready for
/// reads; writes still have to be sent to the leader.
#[get("/ready")]
pub async fn ready(app: Data<MatchEngineApp>) -> HttpResponse {
    let report = collect_health(&app).await;
    let status = if report.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    HttpResponse::build(status).json(report)
}

pub(crate) async fn collect_health(app: &MatchEngineApp) -> HealthResponse {
    let metrics = app.raft.metrics().borrow_watched().clone();
    let grpc_listening = app.grpc_health.is_listening();
    let mut errors = Vec::new();

    if !grpc_listening {
        errors.push("gRPC Raft listener is not running".to_string());
    }
    if let Err(error) = &metrics.running_state {
        errors.push(format!("Raft core stopped: {error:?}"));
    }

    let mut log_store = app.log_store.clone();
    let wal = match log_store.get_log_state().await {
        Ok(state) => Some(WalHealth {
            last_log_id: state.last_log_id,
            last_purged_log_id: state.last_purged_log_id,
        }),
        Err(error) => {
            errors.push(format!("WAL state unavailable: {error}"));
            None
        }
    };

    let mut state_machine = app.state_machine_store.clone();
    let state_machine = match state_machine.applied_state().await {
        Ok((last_applied, _membership)) => Some(StateMachineHealth { last_applied }),
        Err(error) => {
            errors.push(format!("state-machine state unavailable: {error}"));
            None
        }
    };

    let healthy = errors.is_empty();
    HealthResponse {
        status: if healthy { "ok" } else { "unhealthy" }.to_string(),
        healthy,
        ready: healthy && metrics.current_leader.is_some(),
        node_id: app.id,
        http_addr: app.addr.clone(),
        grpc_addr: app.grpc_health.addr().to_string(),
        grpc_listening,
        leadership: LeadershipHealth {
            state: metrics.state,
            is_leader: metrics.state.is_leader(),
            leader_id: metrics.current_leader,
            term: metrics.current_term,
            vote: format!("{}", metrics.vote),
        },
        storage: StorageHealth {
            wal,
            state_machine,
            snapshot: metrics.snapshot,
            purged: metrics.purged,
            local_committed: metrics.local_committed,
            cluster_committed: metrics.cluster_committed,
        },
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_health_flips_when_the_server_stops() {
        let listener = GrpcListenerHealth::new("127.0.0.1:22001".to_string());
        assert_eq!("127.0.0.1:22001", listener.addr());
        assert!(listener.is_listening());

        let clone = listener.clone();
        listener.mark_stopped();
        assert!(!clone.is_listening());
    }
}
