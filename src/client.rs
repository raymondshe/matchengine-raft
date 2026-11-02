use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use openraft::BasicNode;
use openraft::RaftMetrics;
use openraft::RaftTypeConfig;
use openraft::TryAsRef;
use openraft::errors::ClientWriteError;
use openraft::errors::ForwardToLeader;
use openraft::errors::Infallible;
use openraft::errors::InitializeError;
use openraft::errors::LinearizableReadError;
use openraft::errors::NetworkError;
use openraft::errors::RPCError;
use openraft::errors::Unreachable;
use openraft::raft::ClientWriteResponse;
use reqwest::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::network::api::EventsRequest;
use crate::network::api::EventsResponse;
use crate::network::api::EventsStatus;

/// Client transport policy.
///
/// Transient retries are at-least-once: a request may have been applied even
/// when its response was lost. Set `client_order_id` on writes that must not
/// duplicate.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Timeout for one HTTP request.
    pub timeout: Duration,
    /// Retries after a transient transport error (timeout, connection or 5xx).
    pub max_retries: usize,
    /// Retries after a `ForwardToLeader` response, including election settling.
    pub max_forward_retries: usize,
    /// First retry delay; later attempts double it.
    pub initial_backoff: Duration,
    /// Upper bound for one retry delay.
    pub max_backoff: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_retries: 3,
            max_forward_retries: 3,
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(1),
        }
    }
}

/// Typed client failure.
///
/// `RPCError` alone cannot represent an HTTP edge rejection or "no known
/// leader"; callers need to distinguish a retryable transport failure from a
/// rejected request.
#[derive(Debug)]
pub enum ClientError<C: RaftTypeConfig> {
    /// Transport or Raft protocol error.
    Rpc(RPCError<C>),
    /// The HTTP edge rejected the request before Raft changed the book.
    Api { status: u16, body: String },
    /// No leader is known, so a write cannot be forwarded.
    NoLeader,
    /// The request exceeded its configured timeout.
    Timeout,
    /// The response body did not decode.
    Decode(String),
}

impl<C: RaftTypeConfig> ClientError<C> {
    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Rpc(RPCError::Timeout(_) | RPCError::Unreachable(_) | RPCError::Network(_)) => {
                true
            }
            Self::Api { status, .. } => *status == 429 || (500..600).contains(status),
            Self::NoLeader | Self::Decode(_) | Self::Rpc(RPCError::RemoteError(_)) => false,
        }
    }

    /// The HTTP status for an edge rejection.
    pub fn api_status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl<C: RaftTypeConfig> fmt::Display for ClientError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "{error}"),
            Self::Api { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::NoLeader => write!(f, "no known leader"),
            Self::Timeout => write!(f, "request timed out"),
            Self::Decode(message) => write!(f, "decode error: {message}"),
        }
    }
}

impl<C: RaftTypeConfig> std::error::Error for ClientError<C> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rpc(error) => Some(error),
            _ => None,
        }
    }
}

impl<C: RaftTypeConfig> From<RPCError<C>> for ClientError<C> {
    fn from(error: RPCError<C>) -> Self {
        Self::Rpc(error)
    }
}

/// Why a follower read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowerReadErrorKind {
    /// The local node does not know a leader.
    NoLeader,
    /// The leader id is not present in the membership.
    LeaderNotInMembership,
    /// The leader could not be reached.
    LeaderUnreachable,
    /// The leader returned an error or an unreadable response.
    LeaderRejected,
    /// The local state machine did not catch up.
    StateMachine,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct FollowerReadError {
    pub kind: FollowerReadErrorKind,
    pub message: String,
}

impl FollowerReadError {
    pub fn new(kind: FollowerReadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for FollowerReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FollowerReadError {}

pub struct MatchEngineClient<C>
where
    C: RaftTypeConfig,
{
    /// The leader node to send request to.
    ///
    /// All traffic should be sent to the leader in a cluster.
    pub leader: Arc<Mutex<(C::NodeId, String)>>,

    pub inner: Client,

    /// Timeout, retry and backoff policy.
    pub config: ClientConfig,
}

impl<C> MatchEngineClient<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    /// Create a client with the default transport policy.
    pub fn new(leader_id: C::NodeId, leader_addr: String) -> Self {
        Self::with_config(leader_id, leader_addr, ClientConfig::default())
    }

    /// Create a client with an explicit timeout/retry policy.
    pub fn with_config(leader_id: C::NodeId, leader_addr: String, config: ClientConfig) -> Self {
        let inner = Client::builder()
            .no_proxy()
            .timeout(config.timeout)
            .build()
            .expect("HTTP client configuration is valid");
        Self {
            leader: Arc::new(Mutex::new((leader_id, leader_addr))),
            inner,
            config,
        }
    }

    fn backoff(&self, attempt: usize) -> Duration {
        let factor = 1u32 << attempt.min(10) as u32;
        self.config
            .initial_backoff
            .saturating_mul(factor)
            .min(self.config.max_backoff)
    }

    // --- Application API

    /// Submit a write request to the raft cluster.
    ///
    /// The request will be processed by raft protocol: it will be replicated to a quorum and then
    /// will be applied to state machine.
    ///
    /// The result of applying the request will be returned.
    pub async fn write(
        &self,
        req: &C::D,
    ) -> Result<Result<ClientWriteResponse<C>, ClientWriteError<C>>, ClientError<C>> {
        self.send_with_forwarding("write", Some(req), true).await
    }

    /// Submit several requests in one Raft entry.
    ///
    /// The whole slice is applied in order in one `apply` call; the response's
    /// `batch` field holds one response per request.
    pub async fn write_batch(
        &self,
        req: &[C::D],
    ) -> Result<Result<ClientWriteResponse<C>, ClientWriteError<C>>, ClientError<C>> {
        self.send_with_forwarding("write_batch", Some(req), true)
            .await
    }

    /// Read value by key, in an inconsistent mode.
    ///
    /// This method may return stale value because it does not force to read on a legal leader.
    pub async fn read(&self, req: &String) -> Result<String, ClientError<C>> {
        let res = self
            .send_with_retry::<_, _, Infallible>("read", Some(req))
            .await?;
        Ok(res.unwrap())
    }

    /// Consistent Read value by key, in an inconsistent mode.
    ///
    /// This method MUST return consistent value or LinearizableReadError.
    pub async fn linearizable_read(
        &self,
        req: &String,
    ) -> Result<Result<String, LinearizableReadError<C>>, ClientError<C>> {
        self.send_with_forwarding("linearizable_read", Some(req), false)
            .await
    }

    /// Same as linearizable_read() but will automatically forward the request to the current leader
    /// if the node is not a leader.
    pub async fn linearizable_read_auto_forward(
        &self,
        req: &String,
    ) -> Result<Result<String, LinearizableReadError<C>>, ClientError<C>> {
        self.send_with_forwarding("linearizable_read", Some(req), true)
            .await
    }

    /// Read a page of the replicated match-event log.
    ///
    /// The request is forwarded to the current leader when necessary. Resume
    /// from `next_after` and deduplicate on `event_id`.
    pub async fn events(
        &self,
        req: &EventsRequest,
    ) -> Result<Result<EventsResponse, LinearizableReadError<C>>, ClientError<C>> {
        self.send_with_forwarding("events", Some(req), true).await
    }

    /// Retention/position counters for the event log.
    pub async fn events_status(&self) -> Result<EventsStatus, ClientError<C>> {
        let res = self
            .send_with_retry::<_, _, Infallible>("events/status", None::<&()>)
            .await?;
        Ok(res.unwrap())
    }

    /// Perform a linearizable read on a follower.
    ///
    /// This method demonstrates follower reads: it fetches a linearizer from the leader,
    /// waits for the local state machine to catch up, then reads from the local state machine.
    ///
    /// Returns the value if successful, or an error from the follower.
    pub async fn follower_read(
        &self,
        req: &String,
    ) -> Result<Result<String, FollowerReadError>, ClientError<C>> {
        self.send_with_retry("follower_read", Some(req)).await
    }

    // --- Cluster management API

    /// Initialize a cluster of only the node that receives this request.
    ///
    /// This is the first step to initialize a cluster.
    /// With a initialized cluster, new node can be added with [`write`].
    /// Then setup replication with [`add_learner`].
    /// Then make the new node a member with [`change_membership`].
    pub async fn init(&self) -> Result<Result<(), InitializeError<C>>, ClientError<C>> {
        self.send_with_retry("init", Some::<&[(); 0]>(&[])).await
    }

    /// Add a node as learner.
    ///
    /// The node to add has to exist, i.e., being added with `write(MatchEngineRequest::AddNode{})`
    pub async fn add_learner(
        &self,
        req: (C::NodeId, String),
    ) -> Result<Result<ClientWriteResponse<C>, ClientWriteError<C>>, ClientError<C>> {
        self.send_with_forwarding("add-learner", Some(&req), false)
            .await
    }

    /// Change membership to the specified set of nodes.
    ///
    /// All nodes in `req` have to be already added as learner with [`add_learner`],
    /// or an error [`LearnerNotFound`] will be returned.
    pub async fn change_membership(
        &self,
        req: &BTreeSet<C::NodeId>,
    ) -> Result<Result<ClientWriteResponse<C>, ClientWriteError<C>>, ClientError<C>> {
        self.send_with_forwarding("change-membership", Some(req), false)
            .await
    }

    /// Get the metrics about the cluster.
    ///
    /// Metrics contains various information about the cluster, such as current leader,
    /// membership config, replication status etc.
    /// See [`RaftMetrics`].
    pub async fn metrics(&self) -> Result<RaftMetrics<C>, ClientError<C>> {
        let res = self
            .send_with_retry::<_, _, Infallible>("metrics", None::<&()>)
            .await?;
        Ok(res.unwrap())
    }

    // --- Internal methods

    /// Send one request without retrying.
    ///
    /// It sends out a POST request if `req` is Some. Otherwise a GET request.
    /// The remote endpoint must respond a reply in form of `Result<Resp, Err>`.
    /// A non-2xx response is returned as [`ClientError::Api`] with the edge body.
    async fn send<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> Result<Result<Resp, Err>, ClientError<C>>
    where
        Req: Serialize + fmt::Debug + ?Sized + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        tracing::debug!("client-http: start, send to {}; request: {:?}", uri, req);

        let (_leader_id, url) = {
            let t = self.leader.lock().unwrap();
            let target_addr = &t.1;
            (t.0.clone(), format!("http://{}/{}", target_addr, uri))
        };

        let resp = if let Some(r) = req {
            tracing::debug!(
                ">>> client send request to {}: {}",
                url,
                serde_json::to_string_pretty(&r).unwrap()
            );
            self.inner.post(url.clone()).json(r)
        } else {
            tracing::debug!(">>> client send request to {}", url);
            self.inner.get(url.clone())
        }
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ClientError::Timeout
            } else if e.is_connect() {
                // `Unreachable` informs the caller to backoff before retrying.
                ClientError::Rpc(RPCError::Unreachable(Unreachable::new(&e)))
            } else {
                ClientError::Rpc(RPCError::Network(NetworkError::new(&e)))
            }
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let res: Result<Resp, Err> = resp
            .json()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        tracing::debug!(
            "<<< client recv reply from {}: {}",
            url,
            serde_json::to_string_pretty(&res).unwrap()
        );

        Ok(res)
    }

    /// Send one request, retrying transient transport/5xx errors with backoff.
    async fn send_with_retry<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
    ) -> Result<Result<Resp, Err>, ClientError<C>>
    where
        Req: Serialize + fmt::Debug + ?Sized + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        let mut attempt = 0usize;
        loop {
            match self.send(uri, req).await {
                Ok(res) => return Ok(res),
                Err(error) if error.is_retryable() && attempt < self.config.max_retries => {
                    attempt += 1;
                    let delay = self.backoff(attempt);
                    tracing::warn!(
                        uri,
                        attempt,
                        timeout = ?delay,
                        error = %error,
                        "retrying transient client error"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Try the best to send a request to the leader.
    ///
    /// A `ForwardToLeader` response updates the target and, when `forward` is
    /// true, retries up to `max_forward_retries`. Transient transport errors are
    /// retried up to `max_retries` with exponential backoff. A
    /// `ForwardToLeader` without a known leader becomes
    /// [`ClientError::NoLeader`].
    async fn send_with_forwarding<Req, Resp, Err>(
        &self,
        uri: &str,
        req: Option<&Req>,
        forward: bool,
    ) -> Result<Result<Resp, Err>, ClientError<C>>
    where
        Req: Serialize + fmt::Debug + ?Sized + 'static,
        Resp: Serialize + DeserializeOwned,
        Err:
            std::error::Error + Serialize + DeserializeOwned + TryAsRef<ForwardToLeader<C>> + Clone,
    {
        let mut transient_attempts = 0usize;
        let mut forward_attempts = 0usize;
        loop {
            let res: Result<Resp, Err> = match self.send(uri, req).await {
                Ok(res) => res,
                Err(error)
                    if error.is_retryable() && transient_attempts < self.config.max_retries =>
                {
                    transient_attempts += 1;
                    let delay = self.backoff(transient_attempts);
                    tracing::warn!(
                        uri,
                        attempt = transient_attempts,
                        timeout = ?delay,
                        error = %error,
                        "retrying transient client error"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(error) => return Err(error),
            };

            let inner: Err = match res {
                Ok(value) => return Ok(Ok(value)),
                Err(error) => error,
            };

            let rpc_err = match inner.try_as_ref() {
                Some(forward_to) => forward_to,
                None => return Ok(Err(inner)),
            };
            let (Some(leader_id), Some(leader_node)) = (&rpc_err.leader_id, &rpc_err.leader_node)
            else {
                return Err(ClientError::NoLeader);
            };

            // Update target to the new leader.
            {
                let mut t = self.leader.lock().unwrap();
                let api_addr = leader_node.addr.clone();
                *t = (leader_id.clone(), api_addr);
            }

            if forward && forward_attempts < self.config.max_forward_retries {
                forward_attempts += 1;
                let delay = self.backoff(forward_attempts);
                tracing::warn!(
                    uri,
                    attempt = forward_attempts,
                    timeout = ?delay,
                    leader_id = ?leader_id,
                    "retrying request at the new leader"
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            return Ok(Err(inner));
        }
    }
}
