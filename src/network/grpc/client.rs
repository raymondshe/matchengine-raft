//! gRPC client side of the Raft transport.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use futures::Future;
use futures::Stream;
use futures::StreamExt;
use openraft::BasicNode;
use openraft::OptionalSend;
use openraft::RaftNetworkV2;
use openraft::RaftTypeConfig;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::base::BoxFuture;
use openraft::base::BoxStream;
use openraft::errors::NetworkError;
use openraft::errors::RPCError;
use openraft::errors::ReplicationClosed;
use openraft::errors::StreamingError;
use openraft::errors::Timeout;
use openraft::errors::Unreachable;
use openraft::network::RPCOption;
use openraft::network::RPCTypes;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::SnapshotResponse;
use openraft::raft::StreamAppendResult;
use openraft::raft::TransferLeaderRequest;
use openraft::raft::TransferLeaderResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

use crate::network::grpc::HTTP2_CONNECTION_WINDOW;
use crate::network::grpc::HTTP2_KEEPALIVE_INTERVAL;
use crate::network::grpc::HTTP2_KEEPALIVE_TIMEOUT;
use crate::network::grpc::HTTP2_STREAM_WINDOW;
use crate::network::grpc::MAX_MESSAGE_SIZE;
use crate::network::grpc::SNAPSHOT_CHANNEL_SIZE;
use crate::network::grpc::SNAPSHOT_CHUNK_SIZE;
use crate::network::grpc::SNAPSHOT_CHUNK_SIZE_MAX;
use crate::network::grpc::STREAM_APPEND_CHANNEL_SIZE;
use crate::network::grpc::decode;
use crate::network::grpc::encode;
use crate::network::grpc::metrics::TransportMetrics;
use crate::network::grpc::pb::AppendRequest;
use crate::network::grpc::pb::SnapshotChunk;
use crate::network::grpc::pb::SnapshotResponse as PbSnapshotResponse;
use crate::network::grpc::pb::TransferLeaderCall;
use crate::network::grpc::pb::VoteCallRequest;
use crate::network::grpc::pb::raft_service_client::RaftServiceClient;
use crate::network::raft_addr_from_http;
use crate::store::state_machine::SnapshotData;

pub struct GrpcNetwork<C>
where
    C: RaftTypeConfig,
{
    local_id: C::NodeId,
    target: C::NodeId,
    role: GrpcChannelRole,
    /// Process-local transport counters.
    metrics: Arc<TransportMetrics>,
    /// `Err` carries a configuration error; every RPC reports it as unreachable.
    channel: Result<Channel, String>,
    _p: PhantomData<C>,
}

/// The three Raft connections OpenRaft asks the factory for.
///
/// Keeping the role explicit makes the connection isolation intentional: each
/// peer gets a separate lazy channel, and heartbeat/snapshot traffic cannot be
/// head-of-line blocked by replication. The profiles only tune connection
/// setup and HTTP/2 windows; RPC deadlines still come from `RPCOption`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrpcChannelRole {
    Replication,
    Heartbeat,
    Snapshot,
}

#[derive(Debug, Clone, Copy)]
struct ChannelProfile {
    connect_timeout: Duration,
    stream_window: u32,
    connection_window: u32,
    keepalive_interval: Duration,
    keepalive_timeout: Duration,
}

const REPLICATION_PROFILE: ChannelProfile = ChannelProfile {
    connect_timeout: Duration::from_secs(2),
    stream_window: HTTP2_STREAM_WINDOW,
    connection_window: HTTP2_CONNECTION_WINDOW,
    keepalive_interval: HTTP2_KEEPALIVE_INTERVAL,
    keepalive_timeout: HTTP2_KEEPALIVE_TIMEOUT,
};

// Heartbeats carry tiny probes. A smaller window keeps per-peer memory bounded
// while preserving the same liveness timers as replication.
const HEARTBEAT_PROFILE: ChannelProfile = ChannelProfile {
    connect_timeout: Duration::from_secs(2),
    stream_window: 256 * 1024,
    connection_window: 1024 * 1024,
    keepalive_interval: HTTP2_KEEPALIVE_INTERVAL,
    keepalive_timeout: HTTP2_KEEPALIVE_TIMEOUT,
};

// A legitimate snapshot can outlive a heartbeat interval, so its channel is
// deliberately more tolerant of idle gaps and a slower connect.
const SNAPSHOT_PROFILE: ChannelProfile = ChannelProfile {
    connect_timeout: Duration::from_secs(5),
    stream_window: HTTP2_STREAM_WINDOW,
    connection_window: HTTP2_CONNECTION_WINDOW,
    keepalive_interval: Duration::from_secs(10),
    keepalive_timeout: Duration::from_secs(5),
};

impl GrpcChannelRole {
    fn profile(self) -> ChannelProfile {
        match self {
            Self::Replication => REPLICATION_PROFILE,
            Self::Heartbeat => HEARTBEAT_PROFILE,
            Self::Snapshot => SNAPSHOT_PROFILE,
        }
    }
}

impl<C> GrpcNetwork<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    pub fn new(local_id: C::NodeId, target: C::NodeId, api_addr: &str) -> Self {
        Self::new_with_role(
            local_id,
            target,
            api_addr,
            GrpcChannelRole::Replication,
            TransportMetrics::shared(),
        )
    }

    fn new_with_role(
        local_id: C::NodeId,
        target: C::NodeId,
        api_addr: &str,
        role: GrpcChannelRole,
        metrics: Arc<TransportMetrics>,
    ) -> Self {
        let (raft_addr, channel) = match raft_addr_from_http(api_addr) {
            Ok(addr) => {
                let channel = make_channel(&addr, role.profile());
                (addr, channel)
            }
            Err(e) => (api_addr.to_string(), Err(e)),
        };

        tracing::debug!(%raft_addr, %target, ?role, "gRPC Raft client created");

        Self {
            local_id,
            target,
            role,
            metrics,
            channel,
            _p: PhantomData,
        }
    }

    fn client(&self) -> Result<RaftServiceClient<Channel>, RPCError<C>> {
        tracing::trace!(role = ?self.role, target = %self.target, "opening gRPC Raft client");
        let channel = self
            .channel
            .clone()
            .map_err(|e| RPCError::Unreachable(Unreachable::from_string(e)))?;
        Ok(RaftServiceClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE))
    }

    fn timeout(&self, action: RPCTypes, ttl: Duration) -> Timeout<C> {
        Timeout {
            action,
            id: self.local_id.clone(),
            target: self.target.clone(),
            timeout: ttl,
        }
    }

    fn map_status(&self, status: Status, action: RPCTypes, ttl: Duration) -> RPCError<C> {
        map_status(
            status,
            action,
            ttl,
            self.local_id.clone(),
            self.target.clone(),
        )
    }

    async fn unary<T, F>(&self, action: RPCTypes, ttl: Duration, fut: F) -> Result<T, RPCError<C>>
    where
        F: Future<Output = Result<tonic::Response<T>, Status>>,
    {
        match tokio::time::timeout(ttl, fut).await {
            Err(_) => Err(RPCError::Timeout(self.timeout(action, ttl))),
            Ok(Err(status)) => Err(self.map_status(status, action, ttl)),
            Ok(Ok(resp)) => Ok(resp.into_inner()),
        }
    }
}

fn make_channel(addr: &str, profile: ChannelProfile) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}"))
        .map_err(|e| format!("invalid gRPC endpoint for `{addr}`: {e}"))?
        .connect_timeout(profile.connect_timeout)
        .tcp_nodelay(true)
        .initial_stream_window_size(profile.stream_window)
        .initial_connection_window_size(profile.connection_window)
        .http2_keep_alive_interval(profile.keepalive_interval)
        .keep_alive_timeout(profile.keepalive_timeout)
        .keep_alive_while_idle(true);
    Ok(endpoint.connect_lazy())
}

fn map_status<C>(
    status: Status,
    action: RPCTypes,
    ttl: Duration,
    id: C::NodeId,
    target: C::NodeId,
) -> RPCError<C>
where
    C: RaftTypeConfig,
{
    match status.code() {
        Code::Unavailable => RPCError::Unreachable(Unreachable::new(&status)),
        Code::DeadlineExceeded => RPCError::Timeout(Timeout {
            action,
            id,
            target,
            timeout: ttl,
        }),
        _ => RPCError::Network(NetworkError::new(&status)),
    }
}

impl<C> RaftNetworkV2<C> for GrpcNetwork<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    /// Snapshots stream as chunks and the follower writes them to a staging
    /// file; the shared [`SnapshotData`] type also carries locally built
    /// snapshots as an `Arc<[u8]>`.
    type SnapshotData = SnapshotData;

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<C>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<C>, RPCError<C>> {
        let payload = encode(&req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let mut client = self.client()?;
        let ttl = option.soft_ttl();

        let resp = self
            .unary(
                RPCTypes::AppendEntries,
                ttl,
                client.append_entries(AppendRequest { payload }),
            )
            .await?;

        let result: Result<AppendEntriesResponse<C>, openraft::errors::RaftError<C>> =
            decode(&resp.payload).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn vote(
        &mut self,
        req: VoteRequest<C>,
        option: RPCOption,
    ) -> Result<VoteResponse<C>, RPCError<C>> {
        let payload = encode(&req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let mut client = self.client()?;
        let ttl = option.soft_ttl();

        let resp = self
            .unary(
                RPCTypes::Vote,
                ttl,
                client.vote(VoteCallRequest { payload }),
            )
            .await?;

        let result: Result<VoteResponse<C>, openraft::errors::RaftError<C>> =
            decode(&resp.payload).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    /// Sends a PreVote. The payload is the same as [`Self::vote`], but the
    /// receiver neither persists a vote nor changes its term.
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn pre_vote(
        &mut self,
        req: VoteRequest<C>,
        option: RPCOption,
    ) -> Result<VoteResponse<C>, RPCError<C>> {
        let payload = encode(&req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let mut client = self.client()?;
        let ttl = option.soft_ttl();

        let resp = self
            .unary(
                RPCTypes::Vote,
                ttl,
                client.pre_vote(VoteCallRequest { payload }),
            )
            .await?;

        let result: Result<VoteResponse<C>, openraft::errors::RaftError<C>> =
            decode(&resp.payload).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    /// Sends a graceful leadership-transfer request to the target peer.
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn transfer_leader(
        &mut self,
        req: TransferLeaderRequest<C>,
        option: RPCOption,
    ) -> Result<TransferLeaderResponse<C>, RPCError<C>> {
        let payload = encode(&req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let mut client = self.client()?;
        let ttl = option.soft_ttl();

        let resp = self
            .unary(
                RPCTypes::TransferLeader,
                ttl,
                client.transfer_leader(TransferLeaderCall { payload }),
            )
            .await?;

        let result: Result<TransferLeaderResponse<C>, openraft::errors::Fatal<C>> =
            decode(&resp.payload).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    /// Streams the snapshot in OpenRaft-configured chunks. The stream is
    /// cancelled with `cancel`, and a stream that ends before the terminal
    /// `done` chunk is discarded by the follower.
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn full_snapshot(
        &mut self,
        vote: VoteOf<C>,
        snapshot: SnapshotOf<C, Self::SnapshotData>,
        cancel: impl Future<Output = ReplicationClosed> + OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<C>, StreamingError<C>> {
        let started = Instant::now();
        let metrics = self.metrics.clone();
        let mut client = self.client().map_err(StreamingError::from)?;
        let ttl = option.soft_ttl();
        let chunk_size = option
            .snapshot_chunk_size()
            .filter(|size| *size > 0)
            .unwrap_or(SNAPSHOT_CHUNK_SIZE)
            .min(SNAPSHOT_CHUNK_SIZE_MAX);

        let meta = snapshot.meta;
        let data: Arc<[u8]> = match snapshot.snapshot {
            SnapshotData::Memory(data) => data,
            SnapshotData::Staged(staged) => {
                tokio::task::spawn_blocking(move || staged.read_verified())
                    .await
                    .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?
                    .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?
                    .into()
            }
        };
        let snapshot_bytes = data.len();
        let vote = encode(&vote).map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        let meta = encode(&meta).map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;

        let (tx, rx) = mpsc::channel(SNAPSHOT_CHANNEL_SIZE);

        let feed = async move {
            let total = data.len();
            let mut pos = 0;
            let mut offset = 0u64;
            let mut first = true;

            while pos < total {
                let end = (pos + chunk_size).min(total);
                let chunk = SnapshotChunk {
                    vote: if first { vote.clone() } else { Vec::new() },
                    meta: if first { meta.clone() } else { Vec::new() },
                    data: data[pos..end].to_vec(),
                    offset,
                    done: false,
                };
                first = false;

                if tx.send(chunk).await.is_err() {
                    // The RPC side already failed; its error is the useful one.
                    return;
                }

                offset += (end - pos) as u64;
                pos = end;
            }

            // Always terminate explicitly: an empty snapshot sends only this
            // chunk, and a stream that stops without it is treated as cancelled.
            let _ = tx
                .send(SnapshotChunk {
                    vote: if first { vote } else { Vec::new() },
                    meta: if first { meta } else { Vec::new() },
                    data: Vec::new(),
                    offset,
                    done: true,
                })
                .await;
        };

        let call = async {
            let (_, response): ((), Result<tonic::Response<PbSnapshotResponse>, Status>) =
                tokio::join!(feed, client.install_snapshot(ReceiverStream::new(rx)));
            let response = response
                .map_err(|status| self.map_status(status, RPCTypes::InstallSnapshot, ttl))?;

            let result: Result<SnapshotResponse<C>, StreamingError<C>> =
                decode(&response.into_inner().payload)
                    .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
            result
        };

        let result = tokio::select! {
            closed = cancel => Err(StreamingError::Closed(closed)),
            result = call => result,
        };
        metrics.record_snapshot_sent(started.elapsed(), snapshot_bytes, result.is_ok());
        result
    }

    /// Pipelined AppendEntries over one bidirectional gRPC stream.
    ///
    /// Requests are fed from `input` independently from responses, so the
    /// follower can be processing request N while request N+1 is in flight.
    fn stream_append<'s, S>(
        &'s mut self,
        input: S,
        option: RPCOption,
    ) -> BoxFuture<'s, Result<BoxStream<'s, Result<StreamAppendResult<C>, RPCError<C>>>, RPCError<C>>>
    where
        S: Stream<Item = AppendEntriesRequest<C>> + OptionalSend + Unpin + 'static,
    {
        Box::pin(async move {
            let mut client = self.client()?;
            let ttl = option.soft_ttl();
            let local_id = self.local_id.clone();
            let target = self.target.clone();
            let heartbeat = self.role == GrpcChannelRole::Heartbeat;
            self.metrics.record_stream(heartbeat);
            let metrics = self.metrics.clone();

            let (tx, rx) = mpsc::channel::<AppendRequest>(STREAM_APPEND_CHANNEL_SIZE);

            // Only time out a wait that has a request behind it. `soft_ttl` is one
            // heartbeat interval, and the heartbeat worker polls for the first
            // response without waiting for the feeder to send, so an unconditional
            // timer would fire on an otherwise healthy stream. With no request in
            // flight the HTTP/2 keep-alive (see `make_channel`) bounds the wait.
            let inflight = Arc::new(AtomicUsize::new(0));
            let inflight_feed = inflight.clone();

            // The request stream is independent of the response stream: without
            // this task the client would alternate send/recv and lose pipelining.
            tokio::spawn(async move {
                let mut input = input;
                while let Some(req) = input.next().await {
                    match encode(&req) {
                        Ok(payload) => {
                            inflight_feed.fetch_add(1, Ordering::SeqCst);
                            metrics.record_append_request(heartbeat);
                            if tx.send(AppendRequest { payload }).await.is_err() {
                                inflight_feed.fetch_sub(1, Ordering::SeqCst);
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!(%e, "failed to encode streamed AppendEntries");
                            break;
                        }
                    }
                }
            });

            let response = tokio::time::timeout(ttl, client.append_stream(ReceiverStream::new(rx)))
                .await
                .map_err(|_| {
                    RPCError::Timeout(Timeout {
                        action: RPCTypes::AppendEntries,
                        id: local_id.clone(),
                        target: target.clone(),
                        timeout: ttl,
                    })
                })?
                .map_err(|status| {
                    map_status(
                        status,
                        RPCTypes::AppendEntries,
                        ttl,
                        local_id.clone(),
                        target.clone(),
                    )
                })?;

            let mut stream = response.into_inner();

            let out = async_stream::stream! {
                loop {
                    let next = if inflight.load(Ordering::SeqCst) > 0 {
                        match tokio::time::timeout(ttl, stream.next()).await {
                            Ok(next) => next,
                            Err(_) => {
                                yield Err(RPCError::Timeout(Timeout {
                                    action: RPCTypes::AppendEntries,
                                    id: local_id.clone(),
                                    target: target.clone(),
                                    timeout: ttl,
                                }));
                                break;
                            }
                        }
                    } else {
                        stream.next().await
                    };

                    match next {
                        None => break,
                        Some(Err(status)) => {
                            yield Err(map_status(
                                status,
                                RPCTypes::AppendEntries,
                                ttl,
                                local_id.clone(),
                                target.clone(),
                            ));
                            break;
                        }
                        Some(Ok(msg)) => {
                            decrement_inflight(&inflight);

                            let result: Result<StreamAppendResult<C>, RPCError<C>> = decode(&msg.payload)
                                .map_err(|e| RPCError::Network(NetworkError::new(&e)));
                            match result {
                                Ok(result) => yield Ok(result),
                                Err(e) => {
                                    yield Err(e);
                                    break;
                                }
                            }
                        }
                    }
                }
            };

            let out: BoxStream<'s, Result<StreamAppendResult<C>, RPCError<C>>> = Box::pin(out);
            Ok(out)
        })
    }
}

/// Decrements the in-flight append count.
///
/// `AtomicUsize::fetch_update` is deprecated on newer toolchains in favour of
/// `try_update`, which is still unstable on this crate's MSRV. A
/// compare-exchange loop works everywhere and keeps the checked subtraction.
fn decrement_inflight(inflight: &AtomicUsize) {
    let mut current = inflight.load(Ordering::SeqCst);
    while let Some(next) = current.checked_sub(1) {
        match inflight.compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// Factory for the gRPC-only Raft network.
pub struct GrpcNetworkFactory {
    local_id: crate::MatchEngineNodeId,
    metrics: Arc<TransportMetrics>,
}

impl GrpcNetworkFactory {
    /// Creates a factory for connections originating at `local_id`.
    pub fn new(local_id: crate::MatchEngineNodeId) -> Self {
        Self {
            local_id,
            metrics: TransportMetrics::shared(),
        }
    }

    /// Creates a factory sharing the node's transport counters.
    pub fn with_metrics(
        local_id: crate::MatchEngineNodeId,
        metrics: Arc<TransportMetrics>,
    ) -> Self {
        Self { local_id, metrics }
    }
}

impl openraft::network::RaftNetworkFactory<crate::MatchEngineTypeConfig> for GrpcNetworkFactory {
    type Network = GrpcNetwork<crate::MatchEngineTypeConfig>;

    async fn new_client(
        &mut self,
        target: crate::MatchEngineNodeId,
        node: &BasicNode,
    ) -> Self::Network {
        GrpcNetwork::new_with_role(
            self.local_id,
            target,
            &node.addr,
            GrpcChannelRole::Replication,
            self.metrics.clone(),
        )
    }

    async fn new_heartbeat_client(
        &mut self,
        target: crate::MatchEngineNodeId,
        node: &BasicNode,
    ) -> Self::Network {
        GrpcNetwork::new_with_role(
            self.local_id,
            target,
            &node.addr,
            GrpcChannelRole::Heartbeat,
            self.metrics.clone(),
        )
    }

    async fn new_snapshot_client(
        &mut self,
        target: crate::MatchEngineNodeId,
        node: &BasicNode,
    ) -> Self::Network {
        GrpcNetwork::new_with_role(
            self.local_id,
            target,
            &node.addr,
            GrpcChannelRole::Snapshot,
            self.metrics.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::network::RaftNetworkFactory;

    #[tokio::test]
    async fn factory_uses_role_specific_channel_profiles() {
        let mut factory = GrpcNetworkFactory::new(1);
        let node = BasicNode::new("127.0.0.1:21001");

        assert_eq!(
            GrpcChannelRole::Replication,
            factory.new_client(2, &node).await.role
        );
        assert_eq!(
            GrpcChannelRole::Heartbeat,
            factory.new_heartbeat_client(2, &node).await.role
        );
        assert_eq!(
            GrpcChannelRole::Snapshot,
            factory.new_snapshot_client(2, &node).await.role
        );
    }
}
