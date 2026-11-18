//! Direct NATS publication sink for the market-data feed (Profile N).
//!
//! The processor task hands encoded wire frames to a bounded Tokio channel; a
//! background task owns the NATS client and publishes each frame to
//! `<prefix>.<stream_id>`. A reconnect does not block the matching path, and a
//! full queue drops the frame exactly like the Aeron transport (the processor
//! turns the error into a `GAP` marker).
//!
//! Core NATS is at-most-once; the deterministic reset snapshot on leadership
//! gain and the periodic `snapshot_every_ms` refresh are what make a late or
//! reconnected subscriber whole again. JetStream retention can be layered on
//! later without changing this wire format.

use std::io;
use std::time::Duration;

use futures::StreamExt;

use crate::market_data::MarketDataConfig;
use crate::market_data::MarketDataSink;
use crate::market_data::MarketMessage;
use crate::market_data::STREAM_TRADE;
use crate::market_data::request::SnapshotRequest;
use crate::market_data::wire;

/// Queue from the processor task to the NATS publish task.
const PUBLISH_QUEUE: usize = 8_192;

/// Publishes wire frames to NATS subjects.
pub struct NatsSink {
    tx: tokio::sync::mpsc::Sender<(u16, Vec<u8>)>,
    stream_base: u16,
    snapshot_requests: Option<tokio::sync::mpsc::UnboundedReceiver<SnapshotRequest>>,
}

impl NatsSink {
    /// Starts the background publisher. The first connection attempt runs
    /// asynchronously, so an engine can start before NATS is reachable.
    pub async fn start(config: &MarketDataConfig) -> io::Result<Self> {
        let url = config.nats_url.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "NATS sink requires --market-data-nats-url",
            )
        })?;
        let prefix = config
            .nats_subject_prefix
            .trim()
            .trim_end_matches('.')
            .to_string();
        if prefix.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NATS sink requires --market-data-nats-subject-prefix",
            ));
        }
        let stream_base = config.nats_stream_base;
        let (tx, rx) = tokio::sync::mpsc::channel(PUBLISH_QUEUE);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run_publisher(url, prefix, rx, request_tx));
        Ok(Self {
            tx,
            stream_base,
            snapshot_requests: Some(request_rx),
        })
    }
}

impl MarketDataSink for NatsSink {
    fn name(&self) -> &'static str {
        "nats"
    }

    fn take_snapshot_requests(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<SnapshotRequest>> {
        self.snapshot_requests.take()
    }

    fn publish(&mut self, message: &MarketMessage) -> io::Result<()> {
        let offset = message.stream.checked_sub(STREAM_TRADE).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown market-data stream {}", message.stream),
            )
        })?;
        let stream = self
            .stream_base
            .checked_add(offset)
            .ok_or_else(|| io::Error::other("NATS stream id overflow"))?;
        let payload = wire::encode(message);
        match self.tx.try_send((stream, payload)) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "NATS publisher queue full",
            )),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "NATS publisher task stopped",
            )),
        }
    }
}

async fn run_publisher(
    url: String,
    prefix: String,
    mut frames: tokio::sync::mpsc::Receiver<(u16, Vec<u8>)>,
    requests: tokio::sync::mpsc::UnboundedSender<SnapshotRequest>,
) {
    let request_subject = format!("{prefix}.snapshot.request");
    let mut backoff = Duration::from_secs(1);
    loop {
        let client = match async_nats::connect(&url).await {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                client
            }
            Err(error) => {
                tracing::warn!(%url, %error, "NATS connect failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        tracing::info!(%url, %prefix, "NATS market-data publisher connected");

        let mut subscription = match client.subscribe(request_subject.clone()).await {
            Ok(subscription) => Some(subscription),
            Err(error) => {
                tracing::warn!(%request_subject, %error, "NATS snapshot-request subscribe failed");
                None
            }
        };
        let mut flush = tokio::time::interval(Duration::from_millis(100));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                frame = frames.recv() => {
                    let Some((stream, payload)) = frame else {
                        return;
                    };
                    let subject = format!("{prefix}.{stream}");
                    if let Err(error) = client.publish(subject, payload.into()).await {
                        tracing::warn!(stream, %error, "NATS publish failed");
                    }
                }
                message = async {
                    match subscription.as_mut() {
                        Some(subscription) => subscription.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(message) = message else {
                        break;
                    };
                    match SnapshotRequest::decode(&message.payload) {
                        Ok(request) => {
                            if requests.send(request).is_err() {
                                // The engine task is gone; keep publishing frames.
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "discarding malformed NATS snapshot request");
                        }
                    }
                }
                _ = flush.tick() => {
                    if let Err(error) = client.flush().await {
                        tracing::warn!(%error, "NATS flush failed");
                    }
                }
            }
        }
        tracing::warn!(%url, "NATS subscription ended; reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Stream id for a logical stream, used by tests and the market-data source.
pub fn stream_id(stream_base: u16, stream: u16) -> Option<u16> {
    stream_base.checked_add(stream.checked_sub(STREAM_TRADE)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_follow_the_configured_base() {
        assert_eq!(Some(2001), stream_id(2001, 2001));
        assert_eq!(Some(2007), stream_id(2001, 2007));
        assert_eq!(Some(2017), stream_id(2011, 2007));
        assert_eq!(None, stream_id(2001, 2000));
    }
}
