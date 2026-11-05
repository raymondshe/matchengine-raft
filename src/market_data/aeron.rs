//! Aeron publication sink for the market-data feed.
//!
//! rusteron's Aeron handles are `!Send`, so a dedicated publisher thread owns
//! the driver, client and one exclusive publication per stream (2001–2007 by
//! default). The processor task hands encoded frames to that thread through a
//! bounded channel; the matching path is untouched.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusteron_client::Aeron;
use rusteron_client::AeronContext;
use rusteron_client::AeronExclusivePublication;
use rusteron_media_driver::testing::EmbeddedDriver;

use crate::market_data::MarketDataConfig;
use crate::market_data::MarketDataSink;
use crate::market_data::MarketMessage;
use crate::market_data::STREAM_TRADE;
use crate::market_data::wire;

/// Queue from the processor task to the publication thread.
const PUBLISH_QUEUE: usize = 8_192;

/// Publishes wire-v6 frames to Aeron streams through a dedicated thread.
pub struct AeronSink {
    tx: mpsc::SyncSender<(u16, Vec<u8>)>,
    stream_base: u16,
}

impl AeronSink {
    /// Starts the publisher thread, connects/embeds the media driver and waits
    /// until the Aeron client is ready.
    pub async fn start(config: &MarketDataConfig) -> io::Result<Self> {
        let channel = config.aeron_channel.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Aeron sink requires --market-data-aeron-channel",
            )
        })?;
        let stream_base = config.aeron_stream_base;
        let dir = config.aeron_dir.clone();
        let embedded = config.aeron_embedded_driver;

        let (tx, rx) = mpsc::sync_channel(PUBLISH_QUEUE);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        thread::Builder::new()
            .name("aeron-publisher".to_string())
            .spawn(move || publisher_thread(channel, stream_base, dir, embedded, rx, ready_tx))
            .map_err(|error| io::Error::other(format!("spawn Aeron publisher: {error}")))?;

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self { tx, stream_base }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(io::Error::other(
                "Aeron publisher thread stopped during setup",
            )),
        }
    }
}

impl MarketDataSink for AeronSink {
    fn name(&self) -> &'static str {
        "aeron"
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
            .ok_or_else(|| io::Error::other("Aeron stream id overflow"))?;
        let payload = wire::encode(message);
        match self.tx.try_send((stream, payload)) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Aeron publisher queue full",
            )),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Aeron publisher thread stopped",
            )),
        }
    }
}

fn publisher_thread(
    channel: String,
    _stream_base: u16,
    dir: Option<String>,
    embedded: bool,
    rx: mpsc::Receiver<(u16, Vec<u8>)>,
    ready: tokio::sync::oneshot::Sender<io::Result<()>>,
) {
    let driver = match embedded_driver(embedded, dir.as_deref()) {
        Ok(driver) => driver,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let channel = match CString::new(channel) {
        Ok(channel) => channel,
        Err(error) => {
            let _ = ready.send(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Aeron channel: {error}"),
            )));
            return;
        }
    };
    let context = match AeronContext::new() {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(io::Error::other(format!("Aeron context: {error}"))));
            return;
        }
    };
    let effective_dir = driver
        .as_ref()
        .map(|driver| driver.dir().to_string())
        .or(dir)
        .or_else(|| std::env::var("AERON_DIR").ok());
    if let Some(dir) = effective_dir {
        let dir = match CString::new(dir) {
            Ok(dir) => dir,
            Err(error) => {
                let _ = ready.send(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Aeron dir: {error}"),
                )));
                return;
            }
        };
        if let Err(error) = context.set_dir(dir.as_c_str()) {
            let _ = ready.send(Err(io::Error::other(format!("Aeron dir: {error}"))));
            return;
        }
    }
    let aeron = match Aeron::new(&context) {
        Ok(aeron) => aeron,
        Err(error) => {
            let _ = ready.send(Err(io::Error::other(format!("Aeron client: {error}"))));
            return;
        }
    };
    if let Err(error) = aeron.start() {
        let _ = ready.send(Err(io::Error::other(format!("Aeron start: {error}"))));
        return;
    }

    let mut publications: HashMap<u16, AeronExclusivePublication> = HashMap::new();
    let _ = ready.send(Ok(()));

    loop {
        let (stream, payload) = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(item) => item,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = publications.entry(stream) {
            let poller = match aeron
                .async_add_exclusive_publication(channel.as_c_str(), i32::from(stream))
            {
                Ok(poller) => poller,
                Err(error) => {
                    tracing::warn!(%stream, %error, "add Aeron publication failed");
                    continue;
                }
            };
            match poller.poll_blocking(Duration::from_secs(5)) {
                Ok(publication) => {
                    entry.insert(publication);
                }
                Err(error) => {
                    tracing::warn!(%stream, %error, "resolve Aeron publication failed");
                    continue;
                }
            }
        }
        let publication = publications.get(&stream).expect("inserted above");
        let mut offered = false;
        for _ in 0..1_000 {
            match publication.offer(&payload) {
                Ok(_) => {
                    offered = true;
                    break;
                }
                Err(error) if error.is_retryable() => std::hint::spin_loop(),
                Err(error) => {
                    tracing::warn!(%stream, %error, "Aeron offer failed");
                    break;
                }
            }
        }
        if !offered {
            tracing::warn!(%stream, "Aeron publication backpressured; frame dropped");
        }
    }
}

fn embedded_driver(embedded: bool, dir: Option<&str>) -> io::Result<Option<EmbeddedDriver>> {
    if !embedded {
        return Ok(None);
    }
    match dir {
        Some(dir) => {
            let dir = CString::new(dir).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("Aeron dir: {error}"))
            })?;
            EmbeddedDriver::launch_with(move |context| {
                context.set_dir(dir.as_c_str())?;
                Ok(())
            })
            .map(Some)
            .map_err(|error| io::Error::other(format!("start embedded Aeron driver: {error}")))
        }
        None => EmbeddedDriver::launch()
            .map(Some)
            .map_err(|error| io::Error::other(format!("start embedded Aeron driver: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusteron_client::Handlers;
    use rusteron_client::cformat;

    use crate::market_data::BboRecord;
    use crate::market_data::MessageBody;
    use crate::market_data::STREAM_BBO;

    #[tokio::test]
    async fn publishes_wire_frames_on_aeron() {
        let driver = EmbeddedDriver::launch().expect("embedded driver");
        let dir = driver.dir().to_string();
        let config = MarketDataConfig {
            aeron_channel: Some("aeron:ipc".to_string()),
            aeron_dir: Some(dir.clone()),
            ..MarketDataConfig::default()
        };
        let mut sink = AeronSink::start(&config).await.expect("sink");

        let context = AeronContext::new().expect("context");
        context.set_dir(&cformat!("{dir}")).expect("dir");
        let aeron = Aeron::new(&context).expect("client");
        aeron.start().expect("start");
        let subscription = aeron
            .async_add_subscription(
                c"aeron:ipc",
                i32::from(STREAM_BBO),
                Handlers::NONE,
                Handlers::NONE,
            )
            .and_then(|poller| poller.poll_blocking(Duration::from_secs(5)))
            .expect("subscription");

        let message = MarketMessage {
            stream: STREAM_BBO,
            flags: 0,
            market_seq: 7,
            sub_seq: 0,
            log_index: 8,
            ordinal: 0,
            epoch: 1,
            leader_id: 1,
            ts_ms: 1_000,
            symbol_id: 0,
            body: MessageBody::Bbo(BboRecord {
                last_trade: Some("1.085".parse().unwrap()),
                bid: "1.085".parse().unwrap(),
                bid_volume: "10".parse().unwrap(),
                ask: "1.086".parse().unwrap(),
                ask_volume: "5".parse().unwrap(),
                mid: Some("1.0855".parse().unwrap()),
                spread: Some("0.001".parse().unwrap()),
            }),
        };

        let mut received = None;
        for _ in 0..500 {
            let _ = sink.publish(&message);
            subscription
                .poll_fn(
                    |buffer, _header| {
                        if let Ok(decoded) = wire::decode(buffer) {
                            received = Some(decoded);
                        }
                    },
                    10,
                )
                .expect("poll");
            if received.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let received = received.expect("published frame");
        assert_eq!(STREAM_BBO, received.stream);
        assert_eq!(7, received.market_seq);
        assert_eq!(message.body, received.body);
    }
}
