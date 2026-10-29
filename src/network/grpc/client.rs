//! gRPC client side of the Raft transport.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

use crate::network::grpc::MAX_MESSAGE_SIZE;
use crate::network::grpc::SNAPSHOT_CHANNEL_SIZE;
use crate::network::grpc::SNAPSHOT_CHUNK_SIZE;
use crate::network::grpc::STREAM_APPEND_CHANNEL_SIZE;
use crate::network::grpc::decode;
use crate::network::grpc::encode;
use crate::network::grpc::pb::AppendRequest;
use crate::network::grpc::pb::SnapshotChunk;
use crate::network::grpc::pb::SnapshotResponse as PbSnapshotResponse;
use crate::network::grpc::pb::VoteCallRequest;
use crate::network::grpc::pb::raft_service_client::RaftServiceClient;
use crate::network::raft_addr_from_http;

pub struct GrpcNetwork<C>
where
    C: RaftTypeConfig,
{
    local_id: C::NodeId,
    target: C::NodeId,
    /// `Err` carries a configuration error; every RPC reports it as unreachable.
    channel: Result<Channel, String>,
    _p: PhantomData<C>,
}

impl<C> GrpcNetwork<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    pub fn new(local_id: C::NodeId, target: C::NodeId, api_addr: &str) -> Self {
        let (raft_addr, channel) = match raft_addr_from_http(api_addr) {
            Ok(addr) => {
                let channel = make_channel(&addr);
                (addr, channel)
            }
            Err(e) => (api_addr.to_string(), Err(e)),
        };

        tracing::debug!(%raft_addr, %target, "gRPC Raft client created");

        Self {
            local_id,
            target,
            channel,
            _p: PhantomData,
        }
    }

    fn client(&self) -> Result<RaftServiceClient<Channel>, RPCError<C>> {
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

fn make_channel(addr: &str) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}"))
        .map_err(|e| format!("invalid gRPC endpoint for `{addr}`: {e}"))?
        .connect_timeout(Duration::from_secs(2))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(5))
        .keep_alive_timeout(Duration::from_secs(3))
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
    /// Snapshots are streamed as chunks and reassembled by the follower, but the
    /// state machine still installs a contiguous buffer.
    type SnapshotData = std::io::Cursor<Vec<u8>>;

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

    /// Streams the snapshot in fixed-size chunks. The stream is cancelled with
    /// `cancel`, and a stream that ends before the terminal `done` chunk is
    /// discarded by the follower.
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn full_snapshot(
        &mut self,
        vote: VoteOf<C>,
        snapshot: SnapshotOf<C, Self::SnapshotData>,
        cancel: impl Future<Output = ReplicationClosed> + OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<C>, StreamingError<C>> {
        let mut client = self.client().map_err(StreamingError::from)?;
        let ttl = option.soft_ttl();

        let meta = snapshot.meta;
        let data = snapshot.snapshot.into_inner();
        let vote = encode(&vote).map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        let meta = encode(&meta).map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;

        let (tx, rx) = mpsc::channel(SNAPSHOT_CHANNEL_SIZE);

        let feed = async move {
            let total = data.len();
            let mut pos = 0;
            let mut offset = 0u64;
            let mut first = true;

            while pos < total {
                let end = (pos + SNAPSHOT_CHUNK_SIZE).min(total);
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

        tokio::select! {
            closed = cancel => Err(StreamingError::Closed(closed)),
            result = call => result,
        }
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
                            let _ = inflight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));

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
