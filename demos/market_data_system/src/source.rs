//! Feed ingestion.
//!
//! The production design consumes Aeron multicast. This reference implementation
//! uses the engine's existing `--market-data-log` JSONL file as its demo
//! transport: it replays the file from the start and follows new lines. The
//! `FrameSource` stream is the seam where an Aeron subscriber plugs in without
//! touching the assembler, hub or WebSocket layers.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::Stream;
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use matchengine_raft::market_data::MarketMessage;

/// One decoded feed item or a transport error.
pub type Item = io::Result<MarketMessage>;

/// A stream of decoded market-data messages.
///
/// The production adapter (Aeron) and the demo adapters both implement this
/// stream; everything downstream is transport-agnostic.
pub trait FrameSource: Stream<Item = Item> + Send + Unpin + 'static {}

impl<T> FrameSource for T where T: Stream<Item = Item> + Send + Unpin + 'static {}

/// Boxed source handed to [`crate::start`].
pub type BoxedSource = Box<dyn FrameSource>;

/// One JSONL line: the node id plus the flattened [`MarketMessage`].
#[derive(Debug, Deserialize)]
struct JsonlRecord {
    #[allow(dead_code)]
    node_id: u64,
    #[serde(flatten)]
    message: MarketMessage,
}

/// A driverless source for tests.
pub fn channel_source(capacity: usize) -> (mpsc::Sender<Item>, BoxedSource) {
    let (tx, rx) = mpsc::channel(capacity);
    (tx, Box::new(ReceiverStream::new(rx)))
}

/// Replays `path` from the start and then follows appended lines.
///
/// The engine's JSONL sink is append-only; a restart appends replayed entries
/// again, which the assembler dedupes by `(stream, market_seq, sub_seq)`. A
/// truncated file is reopened from the beginning (a fresh deployment).
pub fn jsonl_source(path: PathBuf) -> BoxedSource {
    let (tx, rx) = mpsc::channel(4_096);
    tokio::spawn(async move {
        run_jsonl(path, tx).await;
    });
    Box::new(ReceiverStream::new(rx))
}

async fn run_jsonl(path: PathBuf, tx: mpsc::Sender<Item>) {
    let mut position = 0u64;
    loop {
        match open_and_read(&path, &mut position, &tx).await {
            ReadOutcome::Closed => return,
            ReadOutcome::Reopen => continue,
        }
    }
}

enum ReadOutcome {
    Closed,
    Reopen,
}

async fn open_and_read(path: &PathBuf, position: &mut u64, tx: &mpsc::Sender<Item>) -> ReadOutcome {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return ReadOutcome::Reopen;
        }
        Err(error) => {
            if tx.send(Err(error)).await.is_err() {
                return ReadOutcome::Closed;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            return ReadOutcome::Reopen;
        }
    };

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF: wait for the writer to append, unless the file was
                // truncated or replaced underneath us.
                tokio::time::sleep(Duration::from_millis(50)).await;
                match reader.get_ref().metadata().await {
                    Ok(metadata) if metadata.len() < *position => {
                        *position = 0;
                        return ReadOutcome::Reopen;
                    }
                    Ok(_) => continue,
                    Err(_) => return ReadOutcome::Reopen,
                }
            }
            Ok(bytes) => {
                *position = position.saturating_add(bytes as u64);
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                let item = serde_json::from_str::<JsonlRecord>(trimmed)
                    .map(|record| record.message)
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid market-data JSONL line: {error}"),
                        )
                    });
                if tx.send(item).await.is_err() {
                    return ReadOutcome::Closed;
                }
            }
            Err(error) => {
                if tx.send(Err(error)).await.is_err() {
                    return ReadOutcome::Closed;
                }
                return ReadOutcome::Reopen;
            }
        }
    }
}
