//! Per-symbol JSONL archiving for the market-data service.
//!
//! The archive uses the same shape as the engine's `--market-data-log` sink:
//! one `{"node_id": 0, ...flattened MarketMessage}` object per line. That keeps
//! `source::jsonl_source` as the replay path and lets operators inspect the
//! archive with the engine's existing tooling.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use futures_util::StreamExt;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::io::BufWriter;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use matchengine_raft::market_data::MarketMessage;

use crate::source::BoxedSource;
use crate::source::Item;

/// `data_dir/<symbol>.jsonl`.
pub fn archive_path(dir: &Path, symbol: &str) -> PathBuf {
    dir.join(format!("{symbol}.jsonl"))
}

/// Wraps `source` so every successful message is appended before it is yielded.
///
/// Archive I/O is on a dedicated task; the returned stream applies backpressure
/// through its bounded channel. A write failure is reported as one `Err` item
/// and stops the wrapper, because a configured archive must not silently drop
/// recorded history.
pub fn archive_source(source: BoxedSource, path: PathBuf) -> BoxedSource {
    let (tx, rx) = mpsc::channel(4_096);
    tokio::spawn(async move {
        run(source, path, tx).await;
    });
    Box::new(ReceiverStream::new(rx))
}

#[derive(Serialize)]
struct ArchiveRecord<'a> {
    node_id: u64,
    #[serde(flatten)]
    message: &'a MarketMessage,
}

async fn run(mut source: BoxedSource, path: PathBuf, tx: mpsc::Sender<Item>) {
    if let Some(parent) = path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        let _ = tx.send(Err(error)).await;
        return;
    }
    let file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            let _ = tx.send(Err(error)).await;
            return;
        }
    };
    let mut writer = BufWriter::new(file);

    while let Some(item) = source.next().await {
        match item {
            Ok(message) => {
                let line = match serde_json::to_vec(&ArchiveRecord {
                    node_id: 0,
                    message: &message,
                }) {
                    Ok(line) => line,
                    Err(error) => {
                        let _ = tx
                            .send(Err(io::Error::new(io::ErrorKind::InvalidData, error)))
                            .await;
                        return;
                    }
                };
                if let Err(error) = writer.write_all(&line).await {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                if let Err(error) = writer.write_all(b"\n").await {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                // Flush each message so a replay can follow the archive live and
                // a crash leaves a complete prefix, not a large unflushed tail.
                if let Err(error) = writer.flush().await {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                if tx.send(Ok(message)).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                if tx.send(Err(error)).await.is_err() {
                    return;
                }
            }
        }
    }
}
