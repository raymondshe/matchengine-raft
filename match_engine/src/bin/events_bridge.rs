//! Reference MQ bridge sidecar for the replicated match-event log.
//!
//! The engine never publishes to a message queue from `apply`. This sidecar is
//! an ordinary consumer: it long-polls `POST /events`, publishes every event,
//! and checkpoints its cursor only after the destination acknowledged the
//! page. That ordering makes delivery at-least-once; consumers dedupe on
//! `event_id` (`log_index` + `ordinal`).
//!
//! The shipped publisher appends one JSON object per event to a file. It is the
//! reference implementation of the loop: a Kafka/NATS publisher replaces the
//! append with a broker publish and the checkpoint with a broker-acknowledged
//! offset. Nothing else changes.
//!
//! ```text
//! cargo run --bin events_bridge -- \
//!   --engine 127.0.0.1:21001 --engine-id 1 \
//!   --out /tmp/events.jsonl --cursor /tmp/events.cursor
//! ```
//!
//! A cursor behind the retained window (`oldest_event_id > cursor`) stops the
//! bridge with exit code 2: events were evicted and the consumer must resync
//! out of band. It never silently skips a gap.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use serde::Serialize;

use tokio::io::AsyncWriteExt;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::match_result::EventId;
use matchengine_raft::network::api::DEFAULT_EVENTS_PAGE;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::network::api::MAX_EVENTS_PAGE;
use matchengine_raft::network::api::MAX_EVENTS_WAIT_MS;

type C = MatchEngineTypeConfig;

/// Exit code used when the cursor fell behind the retained event window.
const EXIT_RETENTION_GAP: i32 = 2;

#[derive(Parser, Debug)]
struct Opt {
    /// HTTP address of any node; the client forwards to the leader.
    #[clap(long)]
    engine: String,
    /// Raft id of that node.
    #[clap(long, default_value_t = 1)]
    engine_id: u64,
    /// Append-only JSONL destination (one event per line).
    #[clap(long)]
    out: PathBuf,
    /// Cursor checkpoint, written after each flushed page.
    #[clap(long)]
    cursor: PathBuf,
    /// Events per page, 1..=1000.
    #[clap(long, default_value_t = DEFAULT_EVENTS_PAGE)]
    limit: usize,
    /// Long-poll wait per page, 0..=1000 ms.
    #[clap(long, default_value_t = MAX_EVENTS_WAIT_MS)]
    wait_ms: u64,
    /// Stop after this many pages (0 = run until interrupted).
    #[clap(long, default_value_t = 0)]
    max_pages: u64,
}

/// Persisted cursor. Kept as JSON so operators can inspect and edit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    log_index: u64,
    ordinal: u32,
}

impl From<EventId> for Cursor {
    fn from(id: EventId) -> Self {
        Self {
            log_index: id.log_index,
            ordinal: id.ordinal,
        }
    }
}

impl From<Cursor> for EventId {
    fn from(cursor: Cursor) -> Self {
        Self {
            log_index: cursor.log_index,
            ordinal: cursor.ordinal,
        }
    }
}

/// Loads the checkpoint, or `None` to start at the oldest retained event.
async fn load_cursor(path: &Path) -> std::io::Result<Option<EventId>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt cursor file {}: {error}", path.display()),
                )
            })?;
            Ok(Some(cursor.into()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes the checkpoint via a temporary file + rename, so a crash never leaves
/// a torn cursor.
async fn save_cursor(path: &Path, cursor: Option<EventId>) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let bytes = match cursor {
        Some(id) => serde_json::to_vec(&Cursor::from(id)).map_err(std::io::Error::other)?,
        None => b"null".to_vec(),
    };
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(&bytes).await?;
    file.write_all(b"\n").await?;
    file.sync_data().await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Whether the checkpoint is older than the retained window.
fn is_retention_gap(cursor: Option<EventId>, oldest: Option<EventId>) -> bool {
    match (cursor, oldest) {
        (Some(cursor), Some(oldest)) => oldest > cursor,
        _ => false,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opt = Opt::parse();
    if !(1..=MAX_EVENTS_PAGE).contains(&opt.limit) {
        return Err(format!("--limit must be between 1 and {MAX_EVENTS_PAGE}").into());
    }
    if opt.wait_ms > MAX_EVENTS_WAIT_MS {
        return Err(format!("--wait-ms must be at most {MAX_EVENTS_WAIT_MS}").into());
    }

    if let Some(parent) = opt.out.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(parent) = opt.cursor.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }

    let out = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&opt.out)
        .await?;
    let mut publisher = tokio::io::BufWriter::new(out);

    let client = MatchEngineClient::<C>::new(opt.engine_id, opt.engine.clone());
    let mut cursor = load_cursor(&opt.cursor).await?;
    let mut pages = 0u64;

    loop {
        let request = EventsRequest {
            after: cursor,
            limit: Some(opt.limit),
            wait_ms: Some(opt.wait_ms),
        };
        let page = match client.events(&request).await {
            Ok(Ok(page)) => page,
            Ok(Err(error)) => {
                // The client forwards automatically; a leftover engine-side
                // error is usually a leadership transition or a no-leader gap.
                eprintln!("event read failed: {error}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(error) => {
                eprintln!("event request failed: {error}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if is_retention_gap(cursor, page.oldest_event_id) {
            eprintln!(
                "retention gap: cursor {:?} is older than oldest retained {:?}; \
                 resync out of band and restart from a newer cursor",
                cursor, page.oldest_event_id
            );
            std::process::exit(EXIT_RETENTION_GAP);
        }

        // Publish the page, then acknowledge it by moving the checkpoint. A
        // crash in between replays the page; consumers dedupe on event_id.
        for event in &page.events {
            let line = serde_json::to_vec(event)?;
            publisher.write_all(&line).await?;
            publisher.write_all(b"\n").await?;
        }
        publisher.flush().await?;
        publisher.get_ref().sync_data().await?;

        if let Some(next) = page.next_after {
            cursor = Some(next);
            save_cursor(&opt.cursor, cursor).await?;
        }

        pages = pages.saturating_add(1);
        if opt.max_pages != 0 && pages >= opt.max_pages {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "events-bridge-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn cursor_round_trips_through_the_checkpoint_file() {
        let path = temp_path("cursor");
        assert_eq!(None, load_cursor(&path).await.unwrap());
        let id = EventId {
            log_index: 42,
            ordinal: 3,
        };
        save_cursor(&path, Some(id)).await.unwrap();
        assert_eq!(Some(id), load_cursor(&path).await.unwrap());
        // A torn write leaves the previous checkpoint intact.
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_corrupt_cursor_fails_loudly() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"not-json").unwrap();
        assert!(load_cursor(&path).await.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retention_gap_is_only_reported_beyond_the_window() {
        let cursor = EventId {
            log_index: 10,
            ordinal: 0,
        };
        assert!(!is_retention_gap(Some(cursor), Some(cursor)));
        assert!(!is_retention_gap(
            Some(cursor),
            Some(EventId {
                log_index: 9,
                ordinal: 0
            })
        ));
        assert!(is_retention_gap(
            Some(cursor),
            Some(EventId {
                log_index: 11,
                ordinal: 0
            })
        ));
        assert!(!is_retention_gap(None, Some(cursor)));
        assert!(!is_retention_gap(Some(cursor), None));
    }
}
