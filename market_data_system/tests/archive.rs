//! Archive recording and replay coverage.
//!
//! A live source is wrapped once, every message is appended to a per-symbol
//! JSONL archive, and `jsonl_source` replays the same stream through the normal
//! assembler seam.

use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures_util::StreamExt;
use matchengine_marketdata::archive_path;
use matchengine_marketdata::archive_source;
use matchengine_marketdata::channel_source;
use matchengine_marketdata::jsonl_source;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_STATUS;
use matchengine_raft::market_data::StatusRecord;

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "matchengine-marketdata-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn status(seq: u64) -> MarketMessage {
    MarketMessage {
        stream: STREAM_STATUS,
        flags: 0,
        market_seq: seq,
        sub_seq: 0,
        log_index: seq + 1,
        ordinal: 0,
        epoch: 1,
        leader_id: 1,
        ts_ms: 1_000 + seq,
        symbol_id: 0,
        body: MessageBody::Status(StatusRecord {
            last_log_index: seq + 1,
            last_ordinal: 0,
            last_market_seq: seq,
            last_sub_seq: 0,
            applied_index: seq + 1,
            book_sequence: seq,
            pending_stops: 0,
        }),
    }
}

async fn next(stream: &mut matchengine_marketdata::BoxedSource) -> MarketMessage {
    tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("source timed out")
        .expect("source ended")
        .expect("source error")
}

#[tokio::test]
async fn archives_and_replays_every_message() {
    let dir = temp_dir("archive");
    let path = archive_path(&dir, "eurusd");
    let expected: Vec<MarketMessage> = (1..=3).map(status).collect();

    let (tx, source) = channel_source(8);
    let mut archived = archive_source(source, path.clone());
    for message in &expected {
        tx.send(Ok(message.clone())).await.unwrap();
    }
    for message in &expected {
        assert_eq!(*message, next(&mut archived).await);
    }
    drop(archived);
    drop(tx);

    let lines = std::fs::read_to_string(&path).unwrap();
    assert_eq!(3, lines.lines().count());

    let mut replay = jsonl_source(path);
    for message in &expected {
        assert_eq!(*message, next(&mut replay).await);
    }

    std::fs::remove_dir_all(dir).ok();
}
