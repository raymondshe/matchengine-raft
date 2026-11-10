//! Aeron multicast ingestion for the demo service.
//!
//! The engine publishes one wire-v6 stream per product (2001–2007). This source
//! creates one subscription per stream in a `spawn_blocking` task (rusteron
//! handles stay off the Tokio worker threads), decodes frames and feeds the
//! assembler through a bounded channel. A full instance can therefore run next to each
//! other and scale out without touching the engine.

use std::ffi::CString;
use std::io;
use std::time::Duration;

use rusteron_client::Aeron;
use rusteron_client::AeronContext;
use rusteron_client::AeronExclusivePublication;
use rusteron_client::AeronSubscription;
use rusteron_client::Handlers;
use rusteron_media_driver::testing::EmbeddedDriver;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;

use matchengine_raft::market_data::request;
use matchengine_raft::market_data::request::SnapshotRequest;
use matchengine_raft::market_data::wire;

use crate::config::AeronSpec;
use crate::source::BoxedSource;
use crate::source::Item;
use crate::source::SnapshotRequester;

/// Streams published by the engine: 2001 TRADE .. 2007 BAR.
const STREAM_COUNT: u16 = 7;

/// The Aero source plus the handle that asks the engine for snapshots.
pub struct AeronSource {
    /// Decoded feed frames.
    pub frames: BoxedSource,
    /// Snapshot-request sender for stream 2010.
    pub requester: SnapshotRequester,
}

/// Connects to (or embeds) a media driver and subscribes to all feed streams.
pub async fn aeron_source(aeron: &AeronSpec) -> io::Result<BoxedSource> {
    Ok(aeron_source_with_requester(aeron, 0).await?.frames)
}

/// Like [`aeron_source`], also returning the stream-2010 request handle.
pub async fn aeron_source_with_requester(
    aeron: &AeronSpec,
    symbol_id: u32,
) -> io::Result<AeronSource> {
    let channel = aeron.channel.clone();
    let stream_base = aeron.stream_base;
    let dir = aeron.dir.clone();
    let embedded = aeron.embedded;
    let (requester, request_rx) = SnapshotRequester::channel(symbol_id);

    let (tx, rx) = mpsc::channel::<Item>(8_192);
    let (ready_tx, ready_rx) = oneshot::channel();
    // rusteron handles are `!Send`, so the blocking source owns them inside a
    // `spawn_blocking` closure instead of a Tokio task.
    tokio::task::spawn_blocking(move || {
        run(
            channel,
            stream_base,
            dir,
            embedded,
            tx,
            request_rx,
            ready_tx,
        )
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(AeronSource {
            frames: Box::new(ReceiverStream::new(rx)),
            requester,
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::other("Aeron source task stopped during setup")),
    }
}

fn run(
    channel: String,
    stream_base: u16,
    dir: Option<String>,
    embedded: bool,
    tx: mpsc::Sender<Item>,
    request_rx: std::sync::mpsc::Receiver<SnapshotRequest>,
    ready: oneshot::Sender<io::Result<()>>,
) {
    match setup(channel, stream_base, dir.as_deref(), embedded, tx) {
        Ok((_driver, aeron, subscriptions, tx, channel, request_stream)) => {
            let _ = ready.send(Ok(()));
            poll_loop(
                subscriptions,
                tx,
                aeron,
                channel,
                request_stream,
                request_rx,
            );
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
}

type Setup = (
    Option<EmbeddedDriver>,
    Aeron,
    Vec<AeronSubscription>,
    mpsc::Sender<Item>,
    CString,
    u16,
);

fn setup(
    channel: String,
    stream_base: u16,
    dir: Option<&str>,
    embedded: bool,
    tx: mpsc::Sender<Item>,
) -> io::Result<Setup> {
    let driver = if embedded {
        Some(match dir {
            Some(dir) => {
                let dir = CString::new(dir).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("Aeron dir: {error}"))
                })?;
                EmbeddedDriver::launch_with(move |context| {
                    context.set_dir(dir.as_c_str())?;
                    Ok(())
                })
                .map_err(|error| io::Error::other(format!("start embedded driver: {error}")))?
            }
            None => EmbeddedDriver::launch()
                .map_err(|error| io::Error::other(format!("start embedded driver: {error}")))?,
        })
    } else {
        None
    };
    let effective_dir = driver
        .as_ref()
        .map(|driver| driver.dir().to_string())
        .or_else(|| dir.map(str::to_string))
        .or_else(|| std::env::var("AERON_DIR").ok());

    let context =
        AeronContext::new().map_err(|error| io::Error::other(format!("Aeron context: {error}")))?;
    if let Some(dir) = effective_dir {
        let dir = CString::new(dir).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("Aeron dir: {error}"))
        })?;
        context
            .set_dir(dir.as_c_str())
            .map_err(|error| io::Error::other(format!("Aeron dir: {error}")))?;
    }
    let aeron =
        Aeron::new(&context).map_err(|error| io::Error::other(format!("Aeron client: {error}")))?;
    aeron
        .start()
        .map_err(|error| io::Error::other(format!("Aeron start: {error}")))?;

    let channel = CString::new(channel).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Aeron channel: {error}"),
        )
    })?;
    let mut subscriptions = Vec::with_capacity(usize::from(STREAM_COUNT));
    for offset in 0..STREAM_COUNT {
        let stream = stream_base
            .checked_add(offset)
            .ok_or_else(|| io::Error::other("Aeron stream id overflow"))?;
        let subscription = aeron
            .async_add_subscription(
                channel.as_c_str(),
                i32::from(stream),
                Handlers::NONE,
                Handlers::NONE,
            )
            .and_then(|poller| poller.poll_blocking(Duration::from_secs(5)))
            .map_err(|error| io::Error::other(format!("subscribe to stream {stream}: {error}")))?;
        subscriptions.push(subscription);
    }
    let request_stream = stream_base
        .checked_add(request::STREAM_OFFSET)
        .ok_or_else(|| io::Error::other("Aeron request stream id overflow"))?;
    Ok((
        driver,
        aeron,
        subscriptions,
        tx,
        channel,
        request_stream,
    ))
}

fn poll_loop(
    subscriptions: Vec<AeronSubscription>,
    tx: mpsc::Sender<Item>,
    aeron: Aeron,
    channel: CString,
    request_stream: u16,
    request_rx: std::sync::mpsc::Receiver<SnapshotRequest>,
) {
    let mut request_publication: Option<AeronExclusivePublication> = None;
    loop {
        while let Ok(request) = request_rx.try_recv() {
            if request_publication.is_none() {
                let publication = aeron
                    .async_add_exclusive_publication(
                        channel.as_c_str(),
                        i32::from(request_stream),
                    )
                    .and_then(|poller| poller.poll_blocking(Duration::from_secs(5)));
                match publication {
                    Ok(publication) => request_publication = Some(publication),
                    Err(error) => {
                        tracing::warn!(%error, "create snapshot-request publication failed");
                        break;
                    }
                }
            }
            let Some(publication) = request_publication.as_ref() else {
                break;
            };
            let payload = request.encode();
            let mut offered = false;
            for _ in 0..1_000 {
                match publication.offer(&payload) {
                    Ok(_) => {
                        offered = true;
                        break;
                    }
                    Err(error) if error.is_retryable() => std::hint::spin_loop(),
                    Err(error) => {
                        tracing::warn!(%error, "snapshot request offer failed");
                        break;
                    }
                }
            }
            if !offered {
                tracing::warn!("snapshot request backpressured; request dropped");
            }
        }

        let mut received = 0usize;
        for subscription in &subscriptions {
            let result = subscription.poll_fn(
                |buffer, _header| {
                    let item = wire::decode(buffer).map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                    });
                    // The bounded channel is the backpressure point; a slow
                    // assembler slows this thread and Aeron flow control follows.
                    let _ = tx.blocking_send(item);
                },
                16,
            );
            match result {
                Ok(count) => received += usize::try_from(count).unwrap_or(0),
                Err(error) => {
                    let _ = tx.blocking_send(Err(io::Error::other(format!("Aeron poll: {error}"))));
                }
            }
        }
        if tx.is_closed() {
            return;
        }
        if received == 0 {
            // Blocking poll backoff: this function runs inside `spawn_blocking`,
            // so a Tokio timer cannot be awaited here.
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}
