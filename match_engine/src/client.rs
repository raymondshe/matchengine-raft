use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

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
use openraft::membership::RaftMembership;
use openraft::raft::ClientWriteResponse;
use reqwest::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::network::api::EventsRequest;
use crate::network::api::EventsResponse;
use crate::network::api::EventsStatus;
use crate::network::health::PreTradeStatus;
use crate::order_book::SymbolConfig;
use crate::store::LeaderRedirect;
use crate::store::MatchEngineRequest;
use crate::store::MatchEngineResponse;

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

    /// Whether the target node itself could not be reached (as opposed to the
    /// cluster rejecting the request). Only these errors trigger leader
    /// re-discovery: an edge `429`/`5xx` means the node is alive, so rotating
    /// leaders would be wasteful and could ping-pong between members.
    pub fn is_transport_error(&self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Rpc(RPCError::Timeout(_) | RPCError::Unreachable(_) | RPCError::Network(_))
        )
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
    /// The last known leader node (id + HTTP addr).
    ///
    /// This is a **hint, not a pin**: after a transport error the client
    /// re-discovers the leader from `members` and replaces this value. Requests
    /// are sent here on the fast path.
    pub leader: Arc<Mutex<(C::NodeId, String)>>,

    /// Bootstrap members (id + HTTP addr) used for leader discovery.
    ///
    /// Any member can answer `GET /metrics`, which reports the current Raft
    /// leader and the membership address map. The client queries these on a
    /// transport failure and on startup recovery; no Kubernetes leader label or
    /// Service endpoint switch is required. With a single member this reduces
    /// to a plain single-target client.
    pub members: Arc<Vec<(C::NodeId, String)>>,

    pub inner: Client,

    /// Timeout, retry and backoff policy.
    pub config: ClientConfig,
}

impl<C> MatchEngineClient<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    /// Create a client with the default transport policy that talks to one node.
    ///
    /// For multi-node failover use [`MatchEngineClient::with_members`].
    pub fn new(leader_id: C::NodeId, leader_addr: String) -> Self {
        Self::with_config(leader_id, leader_addr, ClientConfig::default())
    }

    /// Create a single-target client with an explicit timeout/retry policy.
    pub fn with_config(leader_id: C::NodeId, leader_addr: String, config: ClientConfig) -> Self {
        Self::with_members(vec![(leader_id, leader_addr)], config)
    }

    /// Create a client that discovers the leader from a set of Raft members.
    ///
    /// The first member is used as the initial leader hint; the rest are
    /// discovery sources. On a transport error the client calls `GET /metrics`
    /// on the members (skipping the address that just failed) to resolve the
    /// current leader's HTTP address, then retries there. This keeps leader
    /// tracking inside Raft — no Kubernetes label/Service changes are needed —
    /// and works identically for dedicated and packed shards.
    pub fn with_members(members: Vec<(C::NodeId, String)>, config: ClientConfig) -> Self {
        assert!(!members.is_empty(), "at least one member is required");
        let inner = Client::builder()
            .no_proxy()
            .timeout(config.timeout)
            .build()
            .expect("HTTP client configuration is valid");
        let initial = members[0].clone();
        Self {
            leader: Arc::new(Mutex::new(initial)),
            members: Arc::new(members),
            inner,
            config,
        }
    }

    /// Address of the current leader hint.
    fn leader_addr(&self) -> String {
        self.leader.lock().unwrap().1.clone()
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

    /// Pre-trade readiness and symbol provisioning state.
    ///
    /// The endpoint is local to the target node and always returns `200`; use
    /// [`MatchEngineClient::provision_symbol`] and
    /// [`MatchEngineClient::wait_for_pretrade_ready`] for the normal
    /// provisioning flow.
    pub async fn pretrade_status(&self) -> Result<PreTradeStatus, ClientError<C>> {
        self.send_raw_with_retry("pretrade/status").await
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

    /// Send one request to an explicit address without retrying.
    ///
    /// It sends out a POST request if `req` is Some. Otherwise a GET request.
    /// The remote endpoint must respond a reply in form of `Result<Resp, Err>`.
    /// A non-2xx response is returned as [`ClientError::Api`] with the edge body.
    async fn send_to<Req, Resp, Err>(
        &self,
        addr: &str,
        uri: &str,
        req: Option<&Req>,
    ) -> Result<Result<Resp, Err>, ClientError<C>>
    where
        Req: Serialize + fmt::Debug + ?Sized + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        tracing::debug!("client-http: start, send to {}; request: {:?}", uri, req);

        let url = format!("http://{}/{}", addr.trim_end_matches('/'), uri);

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

    /// Send one GET request whose response is raw JSON rather than the usual
    /// `Result<Resp, Err>` envelope (`/health`, `/ready`, `/pretrade/status`).
    async fn send_raw_to<Resp>(&self, addr: &str, uri: &str) -> Result<Resp, ClientError<C>>
    where
        Resp: DeserializeOwned,
    {
        let url = format!("http://{}/{}", addr.trim_end_matches('/'), uri);

        let resp = self.inner.get(url).send().await.map_err(|e| {
            if e.is_timeout() {
                ClientError::Timeout
            } else if e.is_connect() {
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

        resp.json()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Resolve the current leader from `GET /metrics` on one member.
    ///
    /// Returns `Ok(None)` when the member reports no known leader (election in
    /// progress) or the leader id is not in the membership. Such a result is
    /// not an error — the caller should try another member.
    async fn discover_from(
        &self,
        addr: &str,
    ) -> Result<Option<(C::NodeId, String)>, ClientError<C>> {
        let res = self
            .send_to::<_, RaftMetrics<C>, Infallible>(addr, "metrics", None::<&()>)
            .await?;
        let metrics = res.unwrap(); // Err = Infallible
        let Some(leader_id) = metrics.current_leader else {
            return Ok(None);
        };
        let membership = metrics.membership_config.membership();
        let Some(node) = membership.get_node(&leader_id) else {
            return Ok(None);
        };
        Ok(Some((leader_id, node.node.addr.clone())))
    }

    /// Re-discover the leader from the configured members.
    ///
    /// The cached leader is queried first (skipping `failed_addr`, which just
    /// produced a transport error), followed by the bootstrap members. The
    /// first member that reports a leader wins and updates the cached hint.
    /// Discovery calls are read-only, so they are safe to repeat on any member.
    async fn discover_leader(&self, failed_addr: Option<&str>) -> Result<(), ClientError<C>> {
        let mut candidates: Vec<String> = Vec::new();
        let hint = self.leader_addr();
        if !hint.is_empty() && Some(hint.as_str()) != failed_addr && !candidates.contains(&hint) {
            candidates.push(hint);
        }
        for (_, addr) in self.members.iter() {
            if Some(addr.as_str()) != failed_addr && !candidates.contains(addr) {
                candidates.push(addr.clone());
            }
        }

        let mut last_error = ClientError::NoLeader;
        for addr in candidates {
            match self.discover_from(&addr).await {
                Ok(Some((id, leader_addr))) => {
                    {
                        let mut t = self.leader.lock().unwrap();
                        *t = (id.clone(), leader_addr.clone());
                    }
                    tracing::info!(source = %addr, leader_id = ?id, leader = %leader_addr, "re-discovered Raft leader");
                    return Ok(());
                }
                Ok(None) => {
                    tracing::debug!(addr = %addr, "no leader known during discovery");
                    last_error = ClientError::NoLeader;
                }
                Err(error) => {
                    tracing::debug!(addr = %addr, %error, "discovery failed on member");
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    /// Recover from a transport error: best-effort leader re-discovery, then
    /// backoff. Discovery is skipped only when every known address is the one
    /// that just failed (a plain single-target client has no alternative). A
    /// client pinned by `ForwardToLeader` to a moved pod still falls back to
    /// its bootstrap member.
    async fn recover_after_error(&self, attempt: usize, failed_addr: &str, error: &ClientError<C>) {
        let has_alternative = self.leader_addr() != failed_addr
            || self.members.iter().any(|(_, addr)| addr != failed_addr);
        if error.is_transport_error()
            && has_alternative
            && let Err(error) = self.discover_leader(Some(failed_addr)).await
        {
            tracing::warn!(%error, "leader re-discovery failed; retrying hinted target");
        }
        let delay = self.backoff(attempt);
        tokio::time::sleep(delay).await;
    }

    /// Raw-GET variant with transient retries and leader failover.
    async fn send_raw_with_retry<Resp>(&self, uri: &str) -> Result<Resp, ClientError<C>>
    where
        Resp: DeserializeOwned,
    {
        let mut attempt = 0usize;
        loop {
            let addr = self.leader_addr();
            match self.send_raw_to(&addr, uri).await {
                Ok(value) => return Ok(value),
                Err(error) if error.is_retryable() && attempt < self.config.max_retries => {
                    attempt += 1;
                    tracing::warn!(
                        uri,
                        attempt,
                        timeout = ?self.backoff(attempt),
                        error = %error,
                        "retrying transient client error"
                    );
                    self.recover_after_error(attempt, &addr, &error).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Send one request, retrying transient transport/5xx errors with backoff
    /// and leader failover.
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
            let addr = self.leader_addr();
            match self.send_to(&addr, uri, req).await {
                Ok(res) => return Ok(res),
                Err(error) if error.is_retryable() && attempt < self.config.max_retries => {
                    attempt += 1;
                    tracing::warn!(
                        uri,
                        attempt,
                        timeout = ?self.backoff(attempt),
                        error = %error,
                        "retrying transient client error"
                    );
                    self.recover_after_error(attempt, &addr, &error).await;
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
            let addr = self.leader_addr();
            let sent = self.send_to(&addr, uri, req).await;
            // A typed leader redirect is a routing signal, not a generic 5xx:
            // update the leader hint and retry at that node exactly like a
            // `ForwardToLeader` envelope. A null leader is `NoLeader`; a plain
            // 503 (for example the min-voter guard) stays an API error.
            let res: Result<Resp, Err> = match sent {
                Err(ClientError::Api { status: 503, body })
                    if serde_json::from_str::<LeaderRedirect>(&body).is_ok() =>
                {
                    let redirect = serde_json::from_str::<LeaderRedirect>(&body).expect("checked");
                    if redirect.error != "not_leader" {
                        return Err(ClientError::Api { status: 503, body });
                    }
                    let (Some(_leader_id), Some(leader_addr)) =
                        (redirect.leader_id, redirect.leader_addr.clone())
                    else {
                        return Err(ClientError::NoLeader);
                    };
                    // Resolve the typed generic node id from the leader's own
                    // `/metrics` rather than assuming u64 in this client.
                    let Some((leader_id, leader_addr)) =
                        self.discover_from(&leader_addr).await.ok().flatten()
                    else {
                        return Err(ClientError::Api { status: 503, body });
                    };
                    {
                        let mut t = self.leader.lock().unwrap();
                        *t = (leader_id.clone(), leader_addr);
                    }
                    if forward && forward_attempts < self.config.max_forward_retries {
                        forward_attempts += 1;
                        let delay = self.backoff(forward_attempts);
                        tracing::warn!(
                            uri,
                            attempt = forward_attempts,
                            timeout = ?delay,
                            ?leader_id,
                            "retrying request at leader from typed 503"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(ClientError::Api { status: 503, body });
                }
                Ok(res) => res,
                Err(error)
                    if error.is_retryable() && transient_attempts < self.config.max_retries =>
                {
                    transient_attempts += 1;
                    tracing::warn!(
                        uri,
                        attempt = transient_attempts,
                        timeout = ?self.backoff(transient_attempts),
                        error = %error,
                        "retrying transient client error"
                    );
                    self.recover_after_error(transient_attempts, &addr, &error)
                        .await;
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

impl<C> MatchEngineClient<C>
where
    C: RaftTypeConfig<Node = BasicNode, D = MatchEngineRequest, R = MatchEngineResponse>,
{
    /// Provisions the one symbol this process/cluster trades.
    ///
    /// Validates the configuration locally, refuses a locked symbol or a
    /// non-empty book, writes `ConfigureSymbol` when the applied configuration
    /// differs, and verifies the applied configuration afterwards. The inner
    /// `Err` is an application refusal (invalid configuration, symbol already
    /// locked, or an applied-state mismatch); the outer error is transport.
    ///
    /// This helper does not wait for full pre-trade readiness; call
    /// [`MatchEngineClient::wait_for_pretrade_ready`] before enabling order
    /// entry when the node may still be recovering or electing a leader.
    pub async fn provision_symbol(
        &self,
        config: SymbolConfig,
    ) -> Result<Result<PreTradeStatus, String>, ClientError<C>> {
        if let Err(error) = config.validate() {
            return Ok(Err(format!("invalid symbol configuration: {error}")));
        }

        let status = self.pretrade_status().await?;
        if status.symbol_locked {
            return Ok(Err(
                "symbol configuration is locked after trading started".to_string()
            ));
        }
        if status.open_orders > 0 || status.open_stops > 0 {
            return Ok(Err(
                "cannot provision a symbol while the book is not empty".to_string()
            ));
        }

        if status.symbol_config != config {
            let response = match self
                .write(&MatchEngineRequest::ConfigureSymbol {
                    config: config.clone(),
                })
                .await?
            {
                Ok(response) => response,
                Err(error) => {
                    return Ok(Err(format!("configure symbol failed: {error}")));
                }
            };
            if let Some(error) = response.data.error {
                return Ok(Err(error));
            }
        }

        let status = self.pretrade_status().await?;
        if status.symbol_config != config {
            return Ok(Err(format!(
                "applied symbol {} does not match requested symbol {}",
                status.symbol_config.symbol_id, config.symbol_id
            )));
        }
        if status.symbol_locked {
            return Ok(Err(
                "symbol configuration became locked before provisioning completed".to_string(),
            ));
        }
        Ok(Ok(status))
    }

    /// Waits until the node is pre-trade ready under `expected`.
    ///
    /// Polls `GET /pretrade/status` until `ready` is true and the applied
    /// configuration matches `expected`, or `timeout` elapses. Transport errors
    /// are retried while the deadline remains; a configuration mismatch is
    /// returned immediately because waiting cannot fix it.
    pub async fn wait_for_pretrade_ready(
        &self,
        expected: &SymbolConfig,
        timeout: Duration,
    ) -> Result<Result<PreTradeStatus, String>, ClientError<C>> {
        let deadline = Instant::now() + timeout;
        let mut attempt = 0usize;

        loop {
            match self.pretrade_status().await {
                Ok(status) if status.symbol_config != *expected => {
                    return Ok(Err(format!(
                        "applied symbol {} does not match expected symbol {}",
                        status.symbol_config.symbol_id, expected.symbol_id
                    )));
                }
                Ok(status) if status.ready => return Ok(Ok(status)),
                Ok(status) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(Err(format!(
                            "pre-trade readiness timed out: {}",
                            status.reasons.join("; ")
                        )));
                    }
                    attempt = attempt.saturating_add(1);
                    tokio::time::sleep(
                        self.backoff(attempt)
                            .min(deadline.saturating_duration_since(now)),
                    )
                    .await;
                }
                Err(error) if error.is_retryable() && Instant::now() < deadline => {
                    attempt = attempt.saturating_add(1);
                    tokio::time::sleep(
                        self.backoff(attempt)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    )
                    .await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}
