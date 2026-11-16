//! Dump a node's WAL segments as JSONL, one record per line.
//!
//! Usage:
//! `cargo run --example dump_wal -- <node-dir|log-dir|segment.wal>`
//!
//! Each line has `file`, `offset`, `kind`, and either `entry` or `committed`.
//! Entries include their full openraft `log_id` and application payload.

use std::path::Path;
use std::path::PathBuf;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::store::log_store::WalRecord;
use matchengine_raft::store::log_store::read_wal_file;
use serde_json::json;

type C = MatchEngineTypeConfig;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: dump_wal <node-dir|log-dir|segment.wal>"))?;
    let files = wal_files(Path::new(&path))?;
    if files.is_empty() {
        anyhow::bail!("no .wal files under {path}");
    }

    for file in files {
        let file_name = file.display().to_string();
        for (offset, record) in read_wal_file::<C>(&file)? {
            let value = match record {
                WalRecord::Entry(entry) => json!({
                    "file": file_name,
                    "offset": offset,
                    "kind": "entry",
                    "entry": entry,
                }),
                WalRecord::Committed(committed) => json!({
                    "file": file_name,
                    "offset": offset,
                    "kind": "committed",
                    "committed": committed,
                }),
            };
            println!("{}", serde_json::to_string(&value)?);
        }
    }
    Ok(())
}

fn wal_files(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let dir = if path.join("log").is_dir() {
        path.join("log")
    } else {
        path.to_path_buf()
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "wal"))
        .collect();
    files.sort();
    Ok(files)
}
