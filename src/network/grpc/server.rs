//! gRPC server side of the Raft transport.
//!
//! The service is deliberately thin: it decodes one envelope, calls the same
//! `Raft` API the HTTP handlers call, and encodes the result. Consensus
//! semantics stay in openraft.

use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures::Stream;
use futures::StreamExt;
use openraft::alias::SnapshotMetaOf;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::errors::NetworkError;
use openraft::errors::StreamingError;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::SnapshotResponse;
use openraft::raft::TransferLeaderRequest;
use openraft::raft::VoteRequest;
use tokio::io::AsyncWriteExt;
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
use crate::network::grpc::metrics::TransportMetrics;
use crate::network::grpc::pb::AppendRequest;
use crate::network::grpc::pb::AppendResponse;
use crate::network::grpc::pb::SnapshotChunk;
use crate::network::grpc::pb::TransferLeaderCall;
use crate::network::grpc::pb::TransferLeaderResponse as PbTransferLeaderResponse;
use crate::network::grpc::pb::VoteCallRequest;
use crate::network::grpc::pb::VoteCallResponse;
use crate::network::grpc::pb::raft_service_server::RaftService;
use crate::store::state_machine::STAGING_FILE_PREFIX;
use crate::store::state_machine::SnapshotData;
use crate::store::state_machine::StagedSnapshot;

pub struct RaftGrpcService {
    raft: MatchEngineRaft,
    /// Directory for snapshot bodies received from the leader.
    snapshot_staging_dir: PathBuf,
    transport: Arc<TransportMetrics>,
}

impl RaftGrpcService {
    pub fn new(
        raft: MatchEngineRaft,
        snapshot_staging_dir: impl Into<PathBuf>,
        transport: Arc<TransportMetrics>,
    ) -> Self {
        Self {
            raft,
            snapshot_staging_dir: snapshot_staging_dir.into(),
            transport,
        }
    }
}

/// Checks the next received snapshot size against the transport cap.
fn next_snapshot_len(received: u64, chunk_len: usize) -> Result<u64, Status> {
    let next = received
        .checked_add(chunk_len as u64)
        .ok_or_else(|| Status::resource_exhausted("snapshot size overflow"))?;
    if next > MAX_SNAPSHOT_BYTES as u64 {
        return Err(Status::resource_exhausted(format!(
            "snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    Ok(next)
}

static NEXT_STAGING_FILE: AtomicU64 = AtomicU64::new(0);

/// Streams received snapshot chunks into a staging file.
///
/// The file is created with `create_new` under the state machine's snapshot
/// directory and removed on drop unless [`SnapshotStaging::finish`] hands it to
/// a [`StagedSnapshot`] for installation.
struct SnapshotStaging {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    len: u64,
    hasher: crc32fast::Hasher,
    keep: bool,
}

impl SnapshotStaging {
    async fn create(dir: &Path) -> Result<Self, Status> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| Status::internal(format!("create snapshot staging directory: {e}")))?;
        let sequence = NEXT_STAGING_FILE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = dir.join(format!(
            "{STAGING_FILE_PREFIX}{}-{sequence}-{nanos}.tmp",
            std::process::id()
        ));
        let file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
            .map_err(|e| Status::internal(format!("create snapshot staging file: {e}")))?;
        Ok(Self {
            path,
            file: Some(file),
            len: 0,
            hasher: crc32fast::Hasher::new(),
            keep: false,
        })
    }

    async fn push(&mut self, chunk: &[u8]) -> Result<(), Status> {
        let next = next_snapshot_len(self.len, chunk.len())?;
        let file = self
            .file
            .as_mut()
            .expect("snapshot staging file is open until finish");
        file.write_all(chunk)
            .await
            .map_err(|e| Status::internal(format!("write staged snapshot: {e}")))?;
        self.hasher.update(chunk);
        self.len = next;
        Ok(())
    }

    async fn finish(mut self) -> Result<StagedSnapshot, Status> {
        let mut file = self
            .file
            .take()
            .expect("snapshot staging file is open until finish");
        file.flush()
            .await
            .map_err(|e| Status::internal(format!("flush staged snapshot: {e}")))?;
        drop(file);
        self.keep = true;
        Ok(StagedSnapshot::new(
            self.path.clone(),
            self.len,
            self.hasher.clone().finalize(),
        ))
    }
}

impl Drop for SnapshotStaging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Test-only pause after a snapshot chunk is received.
///
/// Debug builds only. `MATCHENGINE_TEST_SNAPSHOT_INSTALL_DELAY_MS` slows a
/// learner's install so a test can kill it mid-stream; the marker file lets the
/// test wait until the first chunk has arrived. Release builds compile this out.
#[cfg(debug_assertions)]
async fn maybe_delay_snapshot_install() {
    let Some(millis) = std::env::var("MATCHENGINE_TEST_SNAPSHOT_INSTALL_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    if let Ok(marker) = std::env::var("MATCHENGINE_TEST_SNAPSHOT_INSTALL_MARKER") {
        let _ = tokio::fs::write(marker, b"installing").await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
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

    async fn pre_vote(
        &self,
        request: Request<VoteCallRequest>,
    ) -> Result<Response<VoteCallResponse>, Status> {
        let req: VoteRequest<MatchEngineTypeConfig> = decode(&request.into_inner().payload)
            .map_err(|e| Status::invalid_argument(format!("decode PreVote: {e}")))?;

        let result = self.raft.pre_vote(req).await;
        let payload = encode(&result)
            .map_err(|e| Status::internal(format!("encode PreVote response: {e}")))?;

        Ok(Response::new(VoteCallResponse { payload }))
    }

    async fn transfer_leader(
        &self,
        request: Request<TransferLeaderCall>,
    ) -> Result<Response<PbTransferLeaderResponse>, Status> {
        let req: TransferLeaderRequest<MatchEngineTypeConfig> =
            decode(&request.into_inner().payload)
                .map_err(|e| Status::invalid_argument(format!("decode TransferLeader: {e}")))?;

        let result = self.raft.handle_transfer_leader(req).await;
        let payload = encode(&result)
            .map_err(|e| Status::internal(format!("encode TransferLeader response: {e}")))?;

        Ok(Response::new(PbTransferLeaderResponse { payload }))
    }

    async fn install_snapshot(
        &self,
        request: Request<Streaming<SnapshotChunk>>,
    ) -> Result<Response<crate::network::grpc::pb::SnapshotResponse>, Status> {
        let started = Instant::now();
        let mut stream = request.into_inner();

        let mut staging = SnapshotStaging::create(&self.snapshot_staging_dir).await?;
        let mut vote: Option<VoteOf<MatchEngineTypeConfig>> = None;
        let mut meta: Option<SnapshotMetaOf<MatchEngineTypeConfig>> = None;
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
            if chunk.offset != staging.len {
                return Err(Status::invalid_argument(format!(
                    "snapshot chunk offset {}, expected {}",
                    chunk.offset, staging.len
                )));
            }

            if !chunk.data.is_empty() {
                staging.push(&chunk.data).await?;
            }

            #[cfg(debug_assertions)]
            maybe_delay_snapshot_install().await;

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

        let staged = staging.finish().await?;
        let snapshot_bytes = staged.len() as usize;
        let snapshot = SnapshotOf::<MatchEngineTypeConfig, SnapshotData> {
            meta,
            snapshot: SnapshotData::Staged(Arc::new(staged)),
        };

        let result: Result<
            SnapshotResponse<MatchEngineTypeConfig>,
            StreamingError<MatchEngineTypeConfig>,
        > = match self.raft.install_full_snapshot(vote, snapshot).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(StreamingError::Network(NetworkError::new(&e))),
        };
        self.transport
            .record_snapshot_received(started.elapsed(), snapshot_bytes, result.is_ok());

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
    fn snapshot_size_is_capped_before_writing() {
        assert_eq!(4, next_snapshot_len(0, 4).unwrap());
        assert_eq!(
            MAX_SNAPSHOT_BYTES as u64,
            next_snapshot_len(MAX_SNAPSHOT_BYTES as u64 - 1, 1).unwrap()
        );
        assert!(next_snapshot_len(MAX_SNAPSHOT_BYTES as u64, 1).is_err());
        assert!(next_snapshot_len(u64::MAX, 1).is_err());
    }

    #[tokio::test]
    async fn snapshot_chunks_are_streamed_to_a_verified_staging_file() {
        let dir = std::env::temp_dir().join(format!(
            "matchengine-snapshot-staging-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let mut staging = SnapshotStaging::create(&dir).await.unwrap();
        staging.push(b"hello ").await.unwrap();
        staging.push(b"world").await.unwrap();
        let staged = staging.finish().await.unwrap();

        assert_eq!(11, staged.len());
        assert_eq!(b"hello world".as_slice(), staged.read_verified().unwrap());
        assert_eq!(
            crc32fast::hash(b"hello world"),
            staged.crc32(),
            "the staged body carries its checksum"
        );

        let path = staged.path().to_path_buf();
        drop(staged);
        assert!(!path.exists(), "the staging file is removed on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_partial_receive_removes_its_staging_file() {
        let dir = std::env::temp_dir().join(format!(
            "matchengine-snapshot-staging-partial-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let path = {
            let mut staging = SnapshotStaging::create(&dir).await.unwrap();
            staging.push(b"partial").await.unwrap();
            let path = staging.path.clone();
            drop(staging);
            path
        };
        assert!(!path.exists(), "an incomplete receive leaves no file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
