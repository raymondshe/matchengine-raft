//! gRPC server side of the Raft transport.
//!
//! The service is deliberately thin: it decodes one envelope, calls the same
//! `Raft` API the HTTP handlers call, and encodes the result. Consensus
//! semantics stay in openraft.

use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures::Stream;
use futures::StreamExt;
use openraft::alias::SnapshotMetaOf;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::errors::NetworkError;
use openraft::errors::StreamingError;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::SnapshotResponse;
use openraft::raft::VoteRequest;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;

use crate::MatchEngineRaft;
use crate::MatchEngineTypeConfig;
use crate::network::MAX_SNAPSHOT_BYTES;
use crate::network::grpc::STREAM_APPEND_CHANNEL_SIZE;
use crate::network::grpc::decode;
use crate::network::grpc::encode;
use crate::network::grpc::pb::AppendRequest;
use crate::network::grpc::pb::AppendResponse;
use crate::network::grpc::pb::SnapshotChunk;
use crate::network::grpc::pb::VoteCallRequest;
use crate::network::grpc::pb::VoteCallResponse;
use crate::network::grpc::pb::raft_service_server::RaftService;

pub struct RaftGrpcService {
    raft: MatchEngineRaft,
}

impl RaftGrpcService {
    pub fn new(raft: MatchEngineRaft) -> Self {
        Self { raft }
    }
}

/// Appends one snapshot chunk while enforcing the total receive budget.
///
/// The state machine needs a contiguous buffer, so the cap must be checked
/// before extending: an offset-bearing peer could otherwise stream forever.
fn append_snapshot_data(
    data: &mut Vec<u8>,
    received: &mut u64,
    chunk: &[u8],
) -> Result<(), Status> {
    let next = received
        .checked_add(chunk.len() as u64)
        .ok_or_else(|| Status::resource_exhausted("snapshot size overflow"))?;
    if next > MAX_SNAPSHOT_BYTES as u64 {
        return Err(Status::resource_exhausted(format!(
            "snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    data.extend_from_slice(chunk);
    *received = next;
    Ok(())
}

#[tonic::async_trait]
impl RaftService for RaftGrpcService {
    type AppendStreamStream =
        Pin<Box<dyn Stream<Item = Result<AppendResponse, Status>> + Send + 'static>>;

    async fn append_entries(
        &self,
        request: Request<AppendRequest>,
    ) -> Result<Response<AppendResponse>, Status> {
        let req: AppendEntriesRequest<MatchEngineTypeConfig> =
            decode(&request.into_inner().payload)
                .map_err(|e| Status::invalid_argument(format!("decode AppendEntries: {e}")))?;

        let result = self.raft.append_entries(req).await;
        let payload = encode(&result)
            .map_err(|e| Status::internal(format!("encode AppendEntries response: {e}")))?;

        Ok(Response::new(AppendResponse { payload }))
    }

    async fn vote(
        &self,
        request: Request<VoteCallRequest>,
    ) -> Result<Response<VoteCallResponse>, Status> {
        let req: VoteRequest<MatchEngineTypeConfig> = decode(&request.into_inner().payload)
            .map_err(|e| Status::invalid_argument(format!("decode Vote: {e}")))?;

        let result = self.raft.vote(req).await;
        let payload =
            encode(&result).map_err(|e| Status::internal(format!("encode Vote response: {e}")))?;

        Ok(Response::new(VoteCallResponse { payload }))
    }

    async fn install_snapshot(
        &self,
        request: Request<Streaming<SnapshotChunk>>,
    ) -> Result<Response<crate::network::grpc::pb::SnapshotResponse>, Status> {
        let mut stream = request.into_inner();

        let mut vote: Option<VoteOf<MatchEngineTypeConfig>> = None;
        let mut meta: Option<SnapshotMetaOf<MatchEngineTypeConfig>> = None;
        let mut data = Vec::new();
        let mut received = 0u64;
        let mut complete = false;

        while let Some(chunk) = stream.message().await? {
            if !chunk.vote.is_empty() {
                vote =
                    Some(decode(&chunk.vote).map_err(|e| {
                        Status::invalid_argument(format!("decode snapshot vote: {e}"))
                    })?);
            }
            if !chunk.meta.is_empty() {
                meta =
                    Some(decode(&chunk.meta).map_err(|e| {
                        Status::invalid_argument(format!("decode snapshot meta: {e}"))
                    })?);
            }
            if chunk.offset != received {
                return Err(Status::invalid_argument(format!(
                    "snapshot chunk offset {}, expected {}",
                    chunk.offset, received
                )));
            }

            if !chunk.data.is_empty() {
                append_snapshot_data(&mut data, &mut received, &chunk.data)?;
            }

            if chunk.done {
                complete = true;
                break;
            }
        }

        if !complete {
            return Err(Status::cancelled(
                "snapshot stream ended before the terminal chunk; snapshot discarded",
            ));
        }

        let (vote, meta) = match (vote, meta) {
            (Some(vote), Some(meta)) => (vote, meta),
            _ => {
                return Err(Status::invalid_argument(
                    "snapshot stream is missing the first chunk's vote or meta",
                ));
            }
        };

        let snapshot = SnapshotOf::<MatchEngineTypeConfig, Cursor<Vec<u8>>> {
            meta,
            snapshot: Cursor::new(data),
        };

        let result: Result<
            SnapshotResponse<MatchEngineTypeConfig>,
            StreamingError<MatchEngineTypeConfig>,
        > = match self.raft.install_full_snapshot(vote, snapshot).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(StreamingError::Network(NetworkError::new(&e))),
        };

        let payload = encode(&result)
            .map_err(|e| Status::internal(format!("encode snapshot response: {e}")))?;

        Ok(Response::new(crate::network::grpc::pb::SnapshotResponse {
            payload,
        }))
    }

    /// One response per request, in order. `Raft::stream_append` pipelines
    /// requests into the core, so the follower flushes logs without waiting for
    /// a round trip per append.
    async fn append_stream(
        &self,
        request: Request<Streaming<AppendRequest>>,
    ) -> Result<Response<Self::AppendStreamStream>, Status> {
        tracing::debug!("gRPC AppendEntries stream opened");

        let raft = self.raft.clone();
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(STREAM_APPEND_CHANNEL_SIZE);
        let decode_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        tokio::spawn(async move {
            let error_slot = decode_error.clone();

            let requests = async_stream::stream! {
                loop {
                    match inbound.message().await {
                        Ok(Some(msg)) => match decode::<AppendEntriesRequest<MatchEngineTypeConfig>>(&msg.payload) {
                            Ok(req) => yield req,
                            Err(e) => {
                                *error_slot.lock().unwrap() =
                                    Some(format!("decode streamed AppendEntries: {e}"));
                                break;
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            *error_slot.lock().unwrap() = Some(format!("read AppendEntries stream: {e}"));
                            break;
                        }
                    }
                }
            };

            let mut results = Box::pin(raft.stream_append(requests));

            while let Some(item) = results.next().await {
                let out = match item {
                    Ok(result) => match encode(&result) {
                        Ok(payload) => Ok(AppendResponse { payload }),
                        Err(e) => Err(Status::internal(format!(
                            "encode streamed AppendEntries response: {e}"
                        ))),
                    },
                    Err(fatal) => Err(Status::internal(format!(
                        "raft stream append failed: {fatal}"
                    ))),
                };
                let fatal = out.is_err();

                if tx.send(out).await.is_err() || fatal {
                    return;
                }
            }

            let error = { decode_error.lock().unwrap().take() };
            if let Some(e) = error {
                let _ = tx.send(Err(Status::invalid_argument(e))).await;
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_data_is_capped_before_allocation() {
        let mut data = Vec::new();
        let mut received = 0;
        append_snapshot_data(&mut data, &mut received, b"1234").unwrap();
        assert_eq!(4, received);
        assert_eq!(b"1234", data.as_slice());

        let mut at_limit = Vec::new();
        let mut received = MAX_SNAPSHOT_BYTES as u64 - 1;
        append_snapshot_data(&mut at_limit, &mut received, b"x").unwrap();
        assert_eq!(MAX_SNAPSHOT_BYTES as u64, received);
        assert!(append_snapshot_data(&mut at_limit, &mut received, b"x").is_err());
        assert_eq!(1, at_limit.len());

        let mut overflow = Vec::new();
        let mut received = u64::MAX;
        assert!(append_snapshot_data(&mut overflow, &mut received, b"x").is_err());
        assert!(overflow.is_empty());
    }
}
