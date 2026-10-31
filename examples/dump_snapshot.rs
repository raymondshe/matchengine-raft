//! Dump a state-machine snapshot as JSONL, one record per line.
//!
//! Usage:
//! `cargo run --example dump_snapshot -- <node-dir|snapshot-dir|current.snap>`
//!
//! The first line is `snapshot_meta`, then one `state` line, followed by one
//! `order`, `stop`, `event` or `submission` line per retained record.

use std::path::Path;
use std::path::PathBuf;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::store::StateMachineData;
use matchengine_raft::store::state_machine::read_snapshot_file;
use serde_json::json;

type C = MatchEngineTypeConfig;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: dump_snapshot <node-dir|snapshot-dir|current.snap>")
    })?;
    let snapshot_dir = snapshot_dir(Path::new(&path));
    let (meta, data) = read_snapshot_file::<C>(&snapshot_dir)?.ok_or_else(|| {
        anyhow::anyhow!("{} does not contain a current.snap", snapshot_dir.display())
    })?;
    let content: StateMachineData = postcard::from_bytes(&data)?;

    println!(
        "{}",
        serde_json::to_string(&json!({
            "kind": "snapshot_meta",
            "path": snapshot_dir.join("current.snap").display().to_string(),
            "meta": meta,
            "orders": content.orders.len(),
            "stops": content.stops.len(),
            "events": content.events.len(),
            "submissions": content.submissions.len(),
        }))?
    );
    println!(
        "{}",
        serde_json::to_string(&json!({
            "kind": "state",
            "sequence": content.sequence,
            "last_trade_price": content.last_trade_price,
            "symbol_config": content.symbol_config,
            "symbol_locked": content.symbol_locked,
            "events_evicted": content.events_evicted,
            "submissions_evicted": content.submissions_evicted,
        }))?
    );

    for order in content.orders {
        println!(
            "{}",
            serde_json::to_string(&json!({"kind": "order", "order": order}))?
        );
    }
    for order in content.stops {
        println!(
            "{}",
            serde_json::to_string(&json!({"kind": "stop", "order": order}))?
        );
    }
    for event in content.events {
        println!(
            "{}",
            serde_json::to_string(&json!({"kind": "event", "event": event}))?
        );
    }
    for submission in content.submissions {
        println!(
            "{}",
            serde_json::to_string(&json!({"kind": "submission", "submission": submission}))?
        );
    }
    Ok(())
}

fn snapshot_dir(path: &Path) -> PathBuf {
    if path.is_file() {
        return path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
    }
    if path.join("snapshot").is_dir() {
        path.join("snapshot")
    } else {
        path.to_path_buf()
    }
}
