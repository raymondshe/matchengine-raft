//! Internal gRPC match-event streaming.
//!
//! The stream is an alternative to long-polling `POST /events` for in-cluster
//! consumers. Pages use the same `EventsResponse` **JSON schema**; the JSON
//! object travels as bytes inside a protobuf envelope. JSON (rather than the
//! postcard used by the Raft RPCs) is deliberate: `MatchEvent` omits absent
//! optional fields with `skip_serializing_if`, which positional formats cannot
//! deserialize reliably, and a language-agnostic page lets non-Rust consumers
//! share the documented schema.
//!
//! The stream is anchored with one leader-consistent read when it opens
//! (`local_lease` by default, falling back to the core lease path and
//! `ReadIndex`), so the serving node must
//! be the leader at that point. Every later page reads
//! locally applied events, which are committed by definition; this avoids a
//! quorum round-trip per page. If leadership moves, the stream keeps delivering
//! committed events from this node until the client reconnects and re-anchors on
//! the new leader.

use std::pin::Pin;

use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::MatchEngineRaft;
use crate::StateMachineStore;
use crate::match_result::EventId;
use crate::network::api::DEFAULT_EVENTS_PAGE;
use crate::network::api::MAX_EVENTS_PAGE;
use crate::network::api::MAX_EVENTS_WAIT_MS;
use crate::network::api::read_events_page;
use crate::network::grpc::pb::StreamEventsPage;
use crate::network::grpc::pb::StreamEventsRequest;
use crate::network::grpc::pb::event_service_server::EventService;
use crate::network::read_policy::ReadPath;
use crate::network::read_policy::ReadPolicyRuntime;

/// Buffer for pages queued toward one streaming client.
const EVENT_STREAM_CHANNEL_SIZE: usize = 8;

/// Serves `EventService` on the node's gRPC listener.
pub struct EventStreamService {
    raft: MatchEngineRaft,
    store: StateMachineStore,
    read_policy: ReadPolicyRuntime,
}

impl EventStreamService {
    pub fn new(
        raft: MatchEngineRaft,
        store: StateMachineStore,
        read_policy: ReadPolicyRuntime,
    ) -> Self {
        Self {
            raft,
            store,
            read_policy,
        }
    }
}

#[tonic::async_trait]
impl EventService for EventStreamService {
    type StreamEventsStream =
        Pin<Box<dyn Stream<Item = Result<StreamEventsPage, Status>> + Send + 'static>>;

    async fn stream_events(
        &self,
        request: Request<StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        let request = request.into_inner();

        let after = match (request.after_log_index, request.after_ordinal) {
            (None, None) => None,
            (Some(log_index), Some(ordinal)) => Some(EventId { log_index, ordinal }),
            _ => {
                return Err(Status::invalid_argument(
                    "after_log_index and after_ordinal must be provided together",
                ));
            }
        };

        let limit = request
            .limit
            .map_or(DEFAULT_EVENTS_PAGE, |value| value as usize);
        if !(1..=MAX_EVENTS_PAGE).contains(&limit) {
            return Err(Status::invalid_argument(format!(
                "events limit must be between 1 and {MAX_EVENTS_PAGE}"
            )));
        }

        let wait_ms = u64::from(request.wait_ms.unwrap_or(0));
        if wait_ms > MAX_EVENTS_WAIT_MS {
            return Err(Status::invalid_argument(format!(
                "events wait_ms must be at most {MAX_EVENTS_WAIT_MS}"
            )));
        }

        let max_pages = request.max_pages.unwrap_or(0);

        // Anchor the stream with one linearizable read. Later pages only read
        // committed applied events, so they need no extra quorum round-trip.
        let ret = self
            .read_policy
            .linearizer(&self.raft, ReadPath::StreamEvents)
            .await
            .map_err(|e| Status::internal(format!("event stream read setup failed: {e}")))?;
        match ret {
            Ok(linearizer) => {
                linearizer
                    .await_ready(&self.raft)
                    .await
                    .map_err(|e| Status::internal(format!("event stream read wait failed: {e}")))?;
            }
            Err(error) => {
                return Err(Status::failed_precondition(format!(
                    "event stream requires the leader: {error}"
                )));
            }
        }

        let store = self.store.clone();
        let (tx, rx) = mpsc::channel(EVENT_STREAM_CHANNEL_SIZE);
        tokio::spawn(async move {
            let mut cursor = after;
            let mut pages = 0u32;
            loop {
                let page = read_events_page(&store, cursor, limit, wait_ms).await;
                cursor = page.next_after;
                pages = pages.saturating_add(1);

                let payload = match serde_json::to_vec(&page) {
                    Ok(payload) => payload,
                    Err(error) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("encode event page: {error}"))))
                            .await;
                        break;
                    }
                };
                if tx.send(Ok(StreamEventsPage { payload })).await.is_err() {
                    break; // client went away
                }
                if max_pages != 0 && pages >= max_pages {
                    break;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
