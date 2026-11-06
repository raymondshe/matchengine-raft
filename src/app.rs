use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::LogStore;
use crate::MatchEngineNodeId;
use crate::MatchEngineRaft;
use crate::StateMachineStore;
use crate::network::console::ConsoleState;
use crate::network::health::GrpcListenerHealth;

/// Application state that holds all core components of a Raft node.
///
/// This struct serves as the central hub that ties together the Raft instance,
/// storage layer, and configuration. It is shared with HTTP request handlers
/// via Actix-web's application data mechanism.
///
/// An instance of `MatchEngineApp` is created for each Raft node at startup and
/// persists for the lifetime of the node.
pub struct MatchEngineApp {
    /// The unique identifier of this Raft node.
    ///
    /// Node IDs must be unique across the entire cluster and typically
    /// start from 1 and increment sequentially.
    pub id: MatchEngineNodeId,

    /// The network address (host:port) where this node serves its HTTP API.
    ///
    /// Peers receive this address in their `BasicNode`, and the Raft transport
    /// port is derived from it (`network::raft_addr_from_http`).
    pub addr: String,

    /// The Raft consensus algorithm instance.
    ///
    /// This is the core of the Raft implementation, handling leader election,
    /// log replication, and consensus management.
    pub raft: MatchEngineRaft,

    /// The state machine storage.
    ///
    /// This wraps the state machine implementation with our matching engine.
    pub state_machine_store: StateMachineStore,

    /// The durable Raft log, used by the health endpoint to report the WAL
    /// frontier without going through Raft metrics.
    pub log_store: LogStore,

    /// Whether the gRPC Raft listener task is still serving.
    pub grpc_health: GrpcListenerHealth,

    /// Set once `Raft::wait_for_recovery` confirms that the state machine has
    /// applied the cluster commit covering this node's durable log tail.
    ///
    /// The WAL commit marker is not fsynced on every commit; gating readiness
    /// on this flag prevents a restarted node from serving a reverted read
    /// before recovery catches up.
    pub recovered: Arc<AtomicBool>,

    /// Minimum voter count required for initialization, membership changes and
    /// writes. Set to 4 by the binary when WAL fsync is disabled.
    pub min_voters: usize,

    /// Operational-console state: action lock and recent action history.
    pub console: ConsoleState,
}
