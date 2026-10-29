use std::fmt::Display;
use std::io::Cursor;
use std::marker::PhantomData;

use openraft::BasicNode;
use openraft::OptionalSend;
use openraft::RaftNetworkV2;
use openraft::RaftTypeConfig;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::errors::NetworkError;
use openraft::errors::RPCError;
use openraft::errors::RaftError;
use openraft::errors::ReplicationClosed;
use openraft::errors::StreamingError;
use openraft::errors::Unreachable;
use openraft::network::RPCOption;
use openraft::network::RaftNetworkFactory;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::SnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use reqwest::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::network::raft::FullSnapshotRequest;

pub struct NetworkFactory {}

impl<C> RaftNetworkFactory<C> for NetworkFactory
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    type Network = Network<C>;

    #[tracing::instrument(level = "debug", skip_all)]
    async fn new_client(&mut self, _target: C::NodeId, node: &BasicNode) -> Self::Network {
        Network::new(node.addr.clone())
    }
}

pub struct Network<C>
where
    C: RaftTypeConfig,
{
    addr: String,
    client: Client,
    _p: PhantomData<C>,
}

impl<C> Network<C>
where
    C: RaftTypeConfig,
{
    pub fn new(addr: String) -> Self {
        let client = Client::builder().no_proxy().build().unwrap();

        Self {
            addr,
            client,
            _p: PhantomData,
        }
    }

    async fn request<Req, Resp, Err>(
        &mut self,
        uri: impl Display,
        req: Req,
        option: &RPCOption,
    ) -> Result<Result<Resp, Err>, RPCError<C>>
    where
        Req: Serialize + 'static,
        Resp: Serialize + DeserializeOwned,
        Err: std::error::Error + Serialize + DeserializeOwned,
    {
        let url = format!("http://{}/{}", self.addr, uri);

        let resp = self
            .client
            .post(url.clone())
            .timeout(option.soft_ttl())
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    // `Unreachable` informs the caller to backoff for a short while to avoid error log flush.
                    RPCError::Unreachable(Unreachable::new(&e))
                } else {
                    RPCError::Network(NetworkError::new(&e))
                }
            })?;

        let res: Result<Resp, Err> = resp.json().await.map_err(|e| NetworkError::new(&e))?;

        Ok(res)
    }
}

impl<C> RaftNetworkV2<C> for Network<C>
where
    C: RaftTypeConfig<Node = BasicNode>,
{
    /// Snapshot data is transmitted as a single JSON payload, therefore it has to be fully
    /// buffered: the follower rebuilds it into a `Cursor<Vec<u8>>` before installing it.
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<C>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<C>, RPCError<C>> {
        // A raft-level rejection (higher term, conflicting log, ...) is reported as a transport
        // error: `RPCError<C>` carries only timeout/unreachable/network variants.
        match self
            .request::<_, _, RaftError<C>>("append", req, &option)
            .await?
        {
            Ok(resp) => Ok(resp),
            Err(e) => Err(RPCError::Network(NetworkError::new(&e))),
        }
    }

    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn vote(
        &mut self,
        req: VoteRequest<C>,
        option: RPCOption,
    ) -> Result<VoteResponse<C>, RPCError<C>> {
        match self
            .request::<_, _, RaftError<C>>("vote", req, &option)
            .await?
        {
            Ok(resp) => Ok(resp),
            Err(e) => Err(RPCError::Network(NetworkError::new(&e))),
        }
    }

    /// Sends the whole snapshot in one request and lets the follower install it.
    ///
    /// `cancel` is not honoured: there is a single request in flight and no local resource to
    /// release, so cancelling mid-flight would only discard work the follower already did.
    #[tracing::instrument(level = "debug", skip_all, err(Debug))]
    async fn full_snapshot(
        &mut self,
        vote: VoteOf<C>,
        snapshot: SnapshotOf<C, Self::SnapshotData>,
        _cancel: impl futures::Future<Output = ReplicationClosed> + OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<C>, StreamingError<C>> {
        let req = FullSnapshotRequest::<C> {
            vote,
            meta: snapshot.meta,
            data: snapshot.snapshot.into_inner(),
        };

        let res: Result<SnapshotResponse<C>, StreamingError<C>> = self
            .request("snapshot", req, &option)
            .await
            .map_err(StreamingError::from)?;

        res
    }
}
