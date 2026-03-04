use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

use openraft::error::AddLearnerError;
use openraft::error::CheckIsLeaderError;
use openraft::error::ClientWriteError;
use openraft::error::ForwardToLeader;
use openraft::error::Infallible;
use openraft::error::InitializeError;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::RemoteError;
use openraft::raft::AddLearnerResponse;
use openraft::raft::ClientWriteResponse;
use openraft::RaftMetrics;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::ExampleNodeId;
use crate::ExampleRequest;
use crate::ExampleTypeConfig;

/// Empty payload for requests that don't need any data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Empty {}

/// A smart client for interacting with the Raft cluster.
///
/// This client automatically tracks the current leader and redirects
/// requests as needed. If a request is sent to a non-leader node, the
/// client will automatically update its leader information and retry.
pub struct ExampleClient {
    /// The current leader node to send requests to.
    ///
    /// All write requests must be sent to the leader in a Raft cluster.
    /// This field is updated automatically when a `ForwardToLeader` error
    /// is received from a node.
    pub leader: Arc<Mutex<(ExampleNodeId, String)>>,

    /// The underlying HTTP client used for making requests.
    pub inner: Client,
}

impl ExampleClient {
    /// Creates a new client with an initial known leader.
    ///
    /// The client will start by sending requests to the specified leader,
    /// but will automatically update to the actual leader if it receives
    /// a `ForwardToLeader` error response.
    ///
    /// # Arguments
    ///
    /// * `leader_id` - The ID of the initial known leader node
    /// * `leader_addr` - The network address (host:port) of the initial leader
    pub fn new(leader_id: ExampleNodeId, leader_addr: String) -> Self {
        Self {
            leader: Arc::new(Mutex::new((leader_id, leader_addr))),
            inner: reqwest::Client::new(),
        }
    }

    // --- Application API

    /// Submits a write request to the Raft cluster.
    ///
    /// The request will go through the Raft consensus protocol: it will be
    /// replicated to a quorum of nodes and then applied to the state machine
    /// before a response is returned. This ensures strong consistency.
    ///
    /// # Arguments
    ///
    /// * `req` - The request to apply (Set, Place, or Cancel)
    ///
    /// # Returns
    ///
    /// The result of applying the request, including the log ID and
    /// membership configuration at the time of application.
    pub async fn write(
        &self,
        req: &ExampleRequest,
    ) -> Result<ClientWriteResponse<ExampleTypeConfig>, RPCError<ExampleTypeConfig, ClientWriteError<ExampleNodeId>>>
    {
        self.send_rpc_to_leader("write", Some(req)).await
    }

    /// Reads a value from the state machine with possible stale data.
    ///
    /// This method reads directly from whichever node is currently believed
    /// to be the leader, without verifying leadership. It may return stale
    /// data if a new leader has been elected but the client hasn't learned
    /// about it yet.
    ///
    /// # Arguments
    ///
    /// * `req` - The key to read, or special keys like "orderbook_orders"
    ///
    /// # Returns
    ///
    /// The value as a string, or an empty string if not found.
    pub async fn read(&self, req: &String) -> Result<String, RPCError<ExampleTypeConfig, Infallible>> {
        self.do_send_rpc_to_leader("read", Some(req)).await
    }

    /// Reads a value with linearizable consistency.
    ///
    /// This method first verifies that the node is still the leader before
    /// returning data. This ensures the read is consistent with all previous
    /// writes, but incurs additional latency from the leader check.
    ///
    /// # Arguments
    ///
    /// * `req` - The key to read
    ///
    /// # Returns
    ///
    /// The value as a string, or an error if the node is not the leader.
    pub async fn consistent_read(
        &self,
        req: &String,
    ) -> Result<String, RPCError<ExampleTypeConfig, CheckIsLeaderError<ExampleNodeId>>> {
        self.do_send_rpc_to_leader("consistent_read", Some(req)).await
    }

    // --- Cluster management API

    /// Initializes a single-node Raft cluster.
    ///
    /// This must be called once on a fresh cluster to bootstrap the Raft
    /// protocol. It creates an initial configuration with only this node
    /// and marks it as the leader.
    ///
    /// After initialization, new nodes can be added with `add_learner`
    /// followed by `change_membership`.
    pub async fn init(&self) -> Result<(), RPCError<ExampleTypeConfig, InitializeError<ExampleNodeId>>> {
        self.do_send_rpc_to_leader("init", Some(&Empty {})).await
    }

    /// Adds a node as a learner (non-voting member).
    ///
    /// A learner receives log replication from the leader but does not
    /// participate in quorum decisions or leader elections. This allows
    /// the new node to catch up with the leader's state before being
    /// promoted to a full voting member.
    ///
    /// # Arguments
    ///
    /// * `req` - A tuple of (node_id, address) for the new learner
    pub async fn add_learner(
        &self,
        req: (ExampleNodeId, String),
    ) -> Result<AddLearnerResponse<ExampleNodeId>, RPCError<ExampleTypeConfig, AddLearnerError<ExampleNodeId>>> {
        self.send_rpc_to_leader("add-learner", Some(&req)).await
    }

    /// Changes the cluster membership to the specified set of nodes.
    ///
    /// This reconfigures which nodes are voting members of the cluster.
    /// All nodes in the new membership must have already been added as
    /// learners and caught up with replication.
    ///
    /// Members not in the new set will be removed from the cluster.
    ///
    /// # Arguments
    ///
    /// * `req` - The set of node IDs that should be voting members
    pub async fn change_membership(
        &self,
        req: &BTreeSet<ExampleNodeId>,
    ) -> Result<ClientWriteResponse<ExampleTypeConfig>, RPCError<ExampleTypeConfig, ClientWriteError<ExampleNodeId>>>
    {
        self.send_rpc_to_leader("change-membership", Some(req)).await
    }

    /// Gets the current metrics from the Raft node.
    ///
    /// Metrics include comprehensive status information such as:
    /// - Current leader ID
    /// - Current term
    /// - Last applied log index
    /// - Cluster membership configuration
    /// - Replication status for each follower
    ///
    /// This is useful for monitoring and debugging the cluster.
    pub async fn metrics(&self) -> Result<RaftMetrics<ExampleTypeConfig>, RPCError<ExampleTypeConfig, Infallible>> {
        self.do_send_rpc_to_leader("metrics", None::<&()>).await
    }

    // --- Internal methods

    /// Sends an RPC request to the currently known leader.
    ///
    /// This is the low-level method that sends an HTTP request without
    /// any automatic retry or leader redirection.
    ///
    /// If `req` is `Some`, sends a POST request with the serialized JSON.
    /// If `req` is `None`, sends a GET request.
    ///
    /// # Arguments
    ///
    /// * `uri` - The URI path to send the request to
    /// * `req` - Optional request payload to serialize as JSON
    async fn do_send_rpc_to_leader<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> Result<Resp, RPCError<ExampleTypeConfig, Err>>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        let (leader_id, url) = {
            let t = self.leader.lock().unwrap();
            let target_addr = &t.1;
            (t.0, format!("http://{}/{}", target_addr, uri))
        };

        let resp = if let Some(r) = req {
            println!(
                ">>> client send request to {}: {}",
                url,
                serde_json::to_string_pretty(&r).unwrap()
            );
            self.inner.post(url.clone()).json(r)
        } else {
            println!(">>> client send request to {}", url,);
            self.inner.get(url.clone())
        }
        .send()
        .await
        .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        let res: Result<Resp, Err> = resp.json().await.map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        println!(
            "<<< client recv reply from {}: {}",
            url,
            serde_json::to_string_pretty(&res).unwrap()
        );

        res.map_err(|e| RPCError::RemoteError(RemoteError::new(leader_id, e)))
    }

    /// Sends a request to the leader with automatic redirection and retry.
    ///
    /// This method wraps `do_send_rpc_to_leader` with automatic leader
    /// discovery and retry logic. If the contacted node is not the leader,
    /// it extracts the leader information from the `ForwardToLeader` error,
    /// updates its internal leader tracking, and retries.
    ///
    /// Retries at most 3 times before giving up.
    ///
    /// # Arguments
    ///
    /// * `uri` - The URI path to send the request to
    /// * `req` - Optional request payload to serialize as JSON
    async fn send_rpc_to_leader<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> Result<Resp, RPCError<ExampleTypeConfig, Err>>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned + TryInto<ForwardToLeader<ExampleNodeId>> + Clone,
    {
        // Retry at most 3 times to find a valid leader.
        let mut n_retry = 3;

        loop {
            let res: Result<Resp, RPCError<ExampleTypeConfig, Err>> = self.do_send_rpc_to_leader(uri, req).await;

            let rpc_err = match res {
                Ok(x) => return Ok(x),
                Err(rpc_err) => rpc_err,
            };

            if let RPCError::RemoteError(remote_err) = &rpc_err {
                let forward_err_res =
                    <Err as TryInto<ForwardToLeader<ExampleNodeId>>>::try_into(remote_err.source.clone());

                if let Ok(ForwardToLeader {
                    leader_id: Some(leader_id),
                    leader_node: Some(leader_node),
                    ..
                }) = forward_err_res
                {
                    // Update target to the new leader.
                    {
                        let mut t = self.leader.lock().unwrap();
                        *t = (leader_id, leader_node.addr);
                    }

                    n_retry -= 1;
                    if n_retry > 0 {
                        continue;
                    }
                }
            }

            return Err(rpc_err);
        }
    }
}
