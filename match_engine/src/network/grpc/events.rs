//! Internal gRPC match-event streaming.
//!
//! The stream is an alternative to long-polling `POST /events` for in-cluster
//! consumers. Pages use the same `EventsResponse` schema as `POST /events`, but
//! are encoded with `postcard` inside the protobuf `bytes` field, just like the
//! Raft RPC payloads. This keeps per-page CPU and bandwidth low on a hot stream.
//! `MatchEvent` uses `skip_serializing_if` for JSON, which cannot be decoded by
//! a positional format, so pages are encoded through the explicit
//! [`WireEventPage`] mirror below: no fields are omitted on the wire. `POST /events`
//! remains JSON for language-agnostic clients; only this internal Rust-to-Rust
//! stream uses the compact format.
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
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::MatchEngineRaft;
use crate::StateMachineStore;
use crate::match_result::EventId;
use crate::match_result::MatchEvent;
use crate::match_result::MatchEventKind;
use crate::network::api::DEFAULT_EVENTS_PAGE;
use crate::network::api::EventsResponse;
use crate::network::api::MAX_EVENTS_PAGE;
use crate::network::api::MAX_EVENTS_WAIT_MS;
use crate::network::api::read_events_page;
use crate::network::grpc::pb::StreamEventsPage;
use crate::network::grpc::pb::StreamEventsRequest;
use crate::network::grpc::pb::event_service_server::EventService;
use crate::network::read_policy::ReadPath;
use crate::network::read_policy::ReadPolicyRuntime;
use crate::order_book::FeeAsset;
use crate::order_book::Fixed;
use crate::order_book::MatchErrors;
use crate::order_book::OrderSide;
use crate::order_book::RejectReason;
use crate::order_book::VolumeUnit;

/// Buffer for pages queued toward one streaming client.
const EVENT_STREAM_CHANNEL_SIZE: usize = 8;

/// Positional wire mirror of [`MatchEvent`].
///
/// `MatchEvent` carries `skip_serializing_if` on two optional fields so JSONL
/// sinks stay compact. postcard is positional and cannot skip fields, so the
/// stream uses this explicit mirror where every field is always present.
#[derive(Serialize, Deserialize)]
struct WireMatchEvent {
    event_id: EventId,
    kind: MatchEventKind,
    order_sequence: u64,
    side: OrderSide,
    counterparty_sequence: Option<u64>,
    previous_sequence: Option<u64>,
    price: Fixed,
    volume: Fixed,
    quote_volume: Fixed,
    volume_unit: VolumeUnit,
    taker_fee_asset: Option<FeeAsset>,
    maker_fee_asset: Option<FeeAsset>,
    taker_fee: Fixed,
    maker_fee: Fixed,
    error: MatchErrors,
    reason: Option<RejectReason>,
}

impl From<MatchEvent> for WireMatchEvent {
    fn from(event: MatchEvent) -> Self {
        Self {
            event_id: event.event_id,
            kind: event.kind,
            order_sequence: event.order_sequence,
            side: event.side,
            counterparty_sequence: event.counterparty_sequence,
            previous_sequence: event.previous_sequence,
            price: event.price,
            volume: event.volume,
            quote_volume: event.quote_volume,
            volume_unit: event.volume_unit,
            taker_fee_asset: event.taker_fee_asset,
            maker_fee_asset: event.maker_fee_asset,
            taker_fee: event.taker_fee,
            maker_fee: event.maker_fee,
            error: event.error,
            reason: event.reason,
        }
    }
}

impl From<WireMatchEvent> for MatchEvent {
    fn from(event: WireMatchEvent) -> Self {
        Self {
            event_id: event.event_id,
            kind: event.kind,
            order_sequence: event.order_sequence,
            side: event.side,
            counterparty_sequence: event.counterparty_sequence,
            previous_sequence: event.previous_sequence,
            price: event.price,
            volume: event.volume,
            quote_volume: event.quote_volume,
            volume_unit: event.volume_unit,
            taker_fee_asset: event.taker_fee_asset,
            maker_fee_asset: event.maker_fee_asset,
            taker_fee: event.taker_fee,
            maker_fee: event.maker_fee,
            error: event.error,
            reason: event.reason,
        }
    }
}

/// Positional wire mirror of [`EventsResponse`] used by `StreamEventsPage`.
#[derive(Serialize, Deserialize)]
struct WireEventPage {
    events: Vec<WireMatchEvent>,
    next_after: Option<EventId>,
    oldest_event_id: Option<EventId>,
    last_event_id: Option<EventId>,
    has_more: bool,
}

impl From<EventsResponse> for WireEventPage {
    fn from(page: EventsResponse) -> Self {
        Self {
            events: page.events.into_iter().map(WireMatchEvent::from).collect(),
            next_after: page.next_after,
            oldest_event_id: page.oldest_event_id,
            last_event_id: page.last_event_id,
            has_more: page.has_more,
        }
    }
}

impl From<WireEventPage> for EventsResponse {
    fn from(page: WireEventPage) -> Self {
        Self {
            events: page.events.into_iter().map(MatchEvent::from).collect(),
            next_after: page.next_after,
            oldest_event_id: page.oldest_event_id,
            last_event_id: page.last_event_id,
            has_more: page.has_more,
        }
    }
}

/// Encodes one event page for the gRPC `StreamEvents` payload.
pub fn encode_event_page(page: EventsResponse) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(&WireEventPage::from(page))
}

/// Decodes one gRPC `StreamEvents` payload back into an [`EventsResponse`].
pub fn decode_event_page(bytes: &[u8]) -> Result<EventsResponse, postcard::Error> {
    postcard::from_bytes::<WireEventPage>(bytes).map(EventsResponse::from)
}

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

                let payload = match encode_event_page(page) {
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

#[cfg(test)]
mod tests {
    use super::decode_event_page;
    use super::encode_event_page;
    use crate::match_result::MatchEvent;
    use crate::network::api::EventsResponse;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::RejectReason;

    #[test]
    fn event_page_postcard_roundtrip_restores_omitted_optionals() {
        let order = Order::new(OrderSide::Buy, 100.0, 1.0, 7);
        let with_reason = MatchEvent::for_rejected(1, 0, &order, RejectReason::PostOnlyWouldCross);
        let without_reason = MatchEvent::for_rejected(2, 0, &order, RejectReason::OrderNotFound);
        let page = EventsResponse {
            events: vec![with_reason, without_reason],
            next_after: Some(with_reason.event_id),
            oldest_event_id: Some(with_reason.event_id),
            last_event_id: Some(without_reason.event_id),
            has_more: false,
        };

        let bytes = encode_event_page(page).expect("postcard encode");
        let decoded = decode_event_page(&bytes).expect("postcard decode");
        assert_eq!(decoded.events.len(), 2);
        assert_eq!(
            decoded.events[0].reason,
            Some(RejectReason::PostOnlyWouldCross)
        );
        assert_eq!(decoded.events[0].previous_sequence, None);
        assert_eq!(decoded.next_after, Some(with_reason.event_id));
        assert_eq!(decoded.oldest_event_id, Some(with_reason.event_id));
        assert_eq!(decoded.last_event_id, Some(without_reason.event_id));
        assert!(!decoded.has_more);
    }
}
