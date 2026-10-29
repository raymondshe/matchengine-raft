//! Runtime-selected Raft network.
//!
//! One factory type keeps `Raft::new` monomorphic while `--transport` decides
//! which implementation each new peer connection gets.

use std::future::Future;
use std::io::Cursor;

use futures::Stream;
use openraft::BasicNode;
use openraft::OptionalSend;
use openraft::RaftNetworkV2;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::base::BoxFuture;
use openraft::base::BoxStream;
use openraft::errors::RPCError;
use openraft::errors::ReplicationClosed;
use openraft::errors::StreamingError;
use openraft::network::RPCOption;
use openraft::network::RaftNetworkFactory;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::SnapshotResponse;
use openraft::raft::StreamAppendResult;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;

use crate::MatchEngineNodeId;
use crate::MatchEngineTypeConfig;
use crate::network::Transport;
use crate::network::grpc::client::GrpcNetwork;
use crate::network::raft_network_impl::Network as HttpNetwork;

pub struct TransportNetworkFactory {
    local_id: MatchEngineNodeId,
    transport: Transport,
}

impl TransportNetworkFactory {
    pub fn new(local_id: MatchEngineNodeId, transport: Transport) -> Self {
        Self {
            local_id,
            transport,
        }
    }
}

impl RaftNetworkFactory<MatchEngineTypeConfig> for TransportNetworkFactory {
    type Network = TransportNetwork;

    async fn new_client(&mut self, target: MatchEngineNodeId, node: &BasicNode) -> Self::Network {
        match self.transport {
            Transport::Http => TransportNetwork::Http(HttpNetwork::new(node.addr.clone())),
            Transport::Grpc => {
                TransportNetwork::Grpc(GrpcNetwork::new(self.local_id, target, &node.addr))
            }
        }
    }
}

pub enum TransportNetwork {
    Http(HttpNetwork<MatchEngineTypeConfig>),
    Grpc(GrpcNetwork<MatchEngineTypeConfig>),
}

impl RaftNetworkV2<MatchEngineTypeConfig> for TransportNetwork {
    type SnapshotData = Cursor<Vec<u8>>;

    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<MatchEngineTypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<MatchEngineTypeConfig>, RPCError<MatchEngineTypeConfig>> {
        match self {
            TransportNetwork::Http(n) => RaftNetworkV2::append_entries(n, req, option).await,
            TransportNetwork::Grpc(n) => RaftNetworkV2::append_entries(n, req, option).await,
        }
    }

    async fn vote(
        &mut self,
        req: VoteRequest<MatchEngineTypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<MatchEngineTypeConfig>, RPCError<MatchEngineTypeConfig>> {
        match self {
            TransportNetwork::Http(n) => RaftNetworkV2::vote(n, req, option).await,
            TransportNetwork::Grpc(n) => RaftNetworkV2::vote(n, req, option).await,
        }
    }

    async fn full_snapshot(
        &mut self,
        vote: VoteOf<MatchEngineTypeConfig>,
        snapshot: SnapshotOf<MatchEngineTypeConfig, Self::SnapshotData>,
        cancel: impl Future<Output = ReplicationClosed> + OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<MatchEngineTypeConfig>, StreamingError<MatchEngineTypeConfig>>
    {
        match self {
            TransportNetwork::Http(n) => {
                RaftNetworkV2::full_snapshot(n, vote, snapshot, cancel, option).await
            }
            TransportNetwork::Grpc(n) => {
                RaftNetworkV2::full_snapshot(n, vote, snapshot, cancel, option).await
            }
        }
    }

    fn stream_append<'s, S>(
        &'s mut self,
        input: S,
        option: RPCOption,
    ) -> BoxFuture<
        's,
        Result<
            BoxStream<
                's,
                Result<StreamAppendResult<MatchEngineTypeConfig>, RPCError<MatchEngineTypeConfig>>,
            >,
            RPCError<MatchEngineTypeConfig>,
        >,
    >
    where
        S: Stream<Item = AppendEntriesRequest<MatchEngineTypeConfig>>
            + OptionalSend
            + Unpin
            + 'static,
    {
        match self {
            TransportNetwork::Http(n) => RaftNetworkV2::stream_append(n, input, option),
            TransportNetwork::Grpc(n) => RaftNetworkV2::stream_append(n, input, option),
        }
    }
}
