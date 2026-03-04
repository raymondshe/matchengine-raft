use async_trait::async_trait;
use openraft::error::AppendEntriesError;
use openraft::error::InstallSnapshotError;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::RemoteError;
use openraft::error::VoteError;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use openraft::Node;
use openraft::RaftNetwork;
use openraft::RaftNetworkFactory;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::ExampleNodeId;
use crate::ExampleTypeConfig;

/// Network factory for creating connections to other Raft nodes.
///
/// This struct implements `RaftNetworkFactory` and is responsible for
/// creating connections to target nodes when Raft needs to send RPCs.
/// It maintains a reqwest HTTP client for making requests.
pub struct ExampleNetwork {
    /// The HTTP client used for all outgoing RPC requests.
    client: reqwest::Client,
}

impl ExampleNetwork {
    /// Creates a new network factory with a fresh HTTP client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Sends an RPC request to a target node.
    ///
    /// This is the generic method used by all Raft RPC types. It:
    /// 1. Resolves the target node's address
    /// 2. Serializes the request as JSON
    /// 3. Sends an HTTP POST request
    /// 4. Deserializes the response
    ///
    /// # Arguments
    ///
    /// * `target` - The ID of the target node
    /// * `target_node` - Optional node information containing the address
    /// * `uri` - The URI path for the specific RPC endpoint
    /// * `req` - The request payload to send
    ///
    /// # Returns
    ///
    /// The response from the target node, or an error if the RPC fails.
    pub async fn send_rpc<Req, Resp, Err>(
        &mut self,
        target: ExampleNodeId,
        target_node: Option<&Node>,
        uri: &str,
        req: Req,
    ) -> Result<Resp, RPCError<ExampleTypeConfig, Err>>
    where
        Req: Serialize,
        Err: std::error::Error + DeserializeOwned,
        Resp: DeserializeOwned,
    {
        let addr = match target_node {
            Some(node) => &node.addr,
            None => {
                return Err(RPCError::Network(NetworkError::new(&std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("target node {} not found", target)
                ))));
            }
        };

        let url = format!("http://{}/{}", addr, uri);

        let resp = self.client.post(url).json(&req).send().await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        let res: Result<Resp, Err> = resp.json().await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        res.map_err(|e| RPCError::RemoteError(RemoteError::new(target, e)))
    }
}

impl Default for ExampleNetwork {
    /// Creates a default network factory.
    fn default() -> Self {
        Self::new()
    }
}

/// Factory implementation for creating Raft network connections.
///
/// This implementation creates a new `ExampleNetworkConnection` for each
/// target node. Each connection gets its own HTTP client instance.
// NOTE: This could be implemented also on `Arc<ExampleNetwork>`, but since it's empty, implemented directly.
#[async_trait]
impl RaftNetworkFactory<ExampleTypeConfig> for ExampleNetwork {
    type Network = ExampleNetworkConnection;

    /// Creates a new connection to a specific target node.
    ///
    /// This method is called by Raft when it needs to communicate with
    /// another node in the cluster.
    async fn connect(&mut self, target: ExampleNodeId, node: Option<&Node>) -> Self::Network {
        ExampleNetworkConnection {
            owner: ExampleNetwork::new(),
            target,
            target_node: node.cloned(),
        }
    }
}

/// A dedicated connection to a specific Raft node.
///
/// This struct implements `RaftNetwork` and provides the methods for
/// sending the three types of Raft RPCs: append_entries, install_snapshot,
/// and vote. Each connection is bound to a specific target node.
pub struct ExampleNetworkConnection {
    /// The network instance that created this connection.
    owner: ExampleNetwork,
    /// The ID of the target node this connection is for.
    target: ExampleNodeId,
    /// Optional cached node information including the address.
    target_node: Option<Node>,
}

/// Network implementation for sending Raft RPCs to a specific node.
///
/// This implementation delegates to the generic `send_rpc` method on the
/// owner network instance, providing the appropriate URI for each RPC type.
#[async_trait]
impl RaftNetwork<ExampleTypeConfig> for ExampleNetworkConnection {
    /// Sends an append entries RPC to the target node.
    ///
    /// This is used by the leader to replicate log entries and send
    /// heartbeats to followers.
    async fn send_append_entries(
        &mut self,
        req: AppendEntriesRequest<ExampleTypeConfig>,
    ) -> Result<AppendEntriesResponse<ExampleNodeId>, RPCError<ExampleTypeConfig, AppendEntriesError<ExampleNodeId>>>
    {
        self.owner.send_rpc(self.target, self.target_node.as_ref(), "raft-append", req).await
    }

    /// Sends an install snapshot RPC to the target node.
    ///
    /// This is used by the leader to send entire state machine snapshots
    /// to followers that are too far behind to catch up with just logs.
    async fn send_install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<ExampleTypeConfig>,
    ) -> Result<InstallSnapshotResponse<ExampleNodeId>, RPCError<ExampleTypeConfig, InstallSnapshotError<ExampleNodeId>>>
    {
        self.owner.send_rpc(self.target, self.target_node.as_ref(), "raft-snapshot", req).await
    }

    /// Sends a vote request RPC to the target node.
    ///
    /// This is used by candidates during leader elections to request
    /// votes from other nodes in the cluster.
    async fn send_vote(
        &mut self,
        req: VoteRequest<ExampleNodeId>,
    ) -> Result<VoteResponse<ExampleNodeId>, RPCError<ExampleTypeConfig, VoteError<ExampleNodeId>>> {
        self.owner.send_rpc(self.target, self.target_node.as_ref(), "raft-vote", req).await
    }
}
