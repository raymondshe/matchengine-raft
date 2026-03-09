use crate::ExampleNodeId;
use crate::ExampleRaft;
use crate::StateMachineStore;

/// Application state that holds all core components of a Raft node.
///
/// This struct serves as the central hub that ties together the Raft instance,
/// storage layer, and configuration. It is shared with HTTP request handlers
/// via Actix-web's application data mechanism.
///
/// An instance of `ExampleApp` is created for each Raft node at startup and
/// persists for the lifetime of the node.
pub struct ExampleApp {
    /// The unique identifier of this Raft node.
    ///
    /// Node IDs must be unique across the entire cluster and typically
    /// start from 1 and increment sequentially.
    pub id: ExampleNodeId,

    /// The network address (host:port) where this node listens for HTTP requests.
    ///
    /// This address is used by other nodes in the cluster to communicate
    /// with this node via Raft RPCs.
    pub addr: String,

    /// The Raft consensus algorithm instance.
    ///
    /// This is the core of the Raft implementation, handling leader election,
    /// log replication, and consensus management.
    pub raft: ExampleRaft,

    /// The state machine storage.
    ///
    /// This wraps the state machine implementation with our matching engine.
    pub state_machine_store: StateMachineStore,
}
