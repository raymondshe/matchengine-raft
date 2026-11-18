//! Direct NATS frame source (Profile N).
//!
//! One Tokio task subscribes to `<prefix>.*` and forwards decoded wire frames
//! to the transport-neutral feed. The NATS client reconnects automatically;
//! this task also rebuilds the subscription if the stream ends and keeps the
//! engine snapshot-request path (`<prefix>.snapshot.request`) alive.
//!
//! The subject carries the engine's stream id (`base..base+6`), not a logical
//! stream name, so the source can reject unrelated suffixes before decoding.

use std::io;
use std::time::Duration;

use futures_util::StreamExt;
use matchengine_raft::market_data::request::SnapshotRequest;
use matchengine_raft::market_data::wire;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::NatsSpec;
use crate::source::BoxedSource;
use crate::source::Item;
use crate::source::SnapshotRequester;

/// A running NATS source for one symbol.
pub struct NatsSource {
    pub frames: BoxedSource,
    pub requester: SnapshotRequester,
}

/// Starts a NATS source for one symbol.
///
/// The task starts in the background, so an engine or NATS outage during
/// startup is retried rather than failing the whole service.
pub fn nats_source(spec: &NatsSpec, symbol_id: u32) -> NatsSource {
    let (tx, rx) = mpsc::channel(4_096);
    let (requester, requests) = SnapshotRequester::tokio_channel(symbol_id);
    let spec = spec.clone();
    tokio::spawn(run(spec, tx, requests));
    NatsSource {
        frames: Box::new(ReceiverStream::new(rx)),
        requester,
    }
}

async fn run(
    spec: NatsSpec,
    tx: mpsc::Sender<Item>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<SnapshotRequest>,
) {
    let prefix = spec.subject_prefix.trim().trim_end_matches('.').to_string();
    let subject = format!("{prefix}.*");
    let request_subject = format!("{prefix}.snapshot.request");
    let first_stream = spec.stream_base;
    let last_stream = spec.stream_base.saturating_add(7);
    let mut backoff = Duration::from_secs(1);

    loop {
        let client = match async_nats::connect(&spec.url).await {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                client
            }
            Err(error) => {
                tracing::warn!(url = %spec.url, %error, "NATS source connect failed; retrying");
                if send_error(&tx, format!("NATS connect failed: {error}")).await {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };

        let mut subscription = match client.subscribe(subject.clone()).await {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::warn!(%subject, %error, "NATS source subscribe failed; retrying");
                if send_error(&tx, format!("NATS subscribe failed: {error}")).await {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        tracing::info!(
            url = %spec.url,
            %subject,
            %request_subject,
            stream_base = spec.stream_base,
            "NATS market-data source connected"
        );

        loop {
            tokio::select! {
                message = subscription.next() => {
                    let Some(message) = message else {
                        break;
                    };
                    let Some(stream) = message
                        .subject
                        .rsplit('.')
                        .next()
                        .and_then(|token| token.parse::<u16>().ok())
                    else {
                        continue;
                    };
                    if stream < first_stream || stream >= last_stream {
                        continue;
                    }
                    match wire::decode(&message.payload) {
                        Ok(message) => {
                            if tx.send(Ok(message)).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%stream, %error, "discarding malformed NATS market-data frame");
                        }
                    }
                }
                Some(request) = requests.recv() => {
                    if let Err(error) = client
                        .publish(request_subject.clone(), request.encode().to_vec().into())
                        .await
                    {
                        tracing::warn!(%error, "NATS snapshot request failed");
                    }
                }
            }
        }
        tracing::warn!(%subject, "NATS source subscription ended; reconnecting");
        if send_error(&tx, "NATS subscription ended".to_string()).await {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Returns true when the feed has stopped and the task should exit.
async fn send_error(tx: &mpsc::Sender<Item>, message: String) -> bool {
    tx.send(Err(io::Error::other(message))).await.is_err()
}
