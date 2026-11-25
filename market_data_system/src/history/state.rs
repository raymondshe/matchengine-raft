//! `bar_state`, `bar_gaps` and `bar_revisions` accessors.
//!
//! TDengine is the system of record. The newest `bar_state` row is the writer's
//! cursor and watermark; gaps and revisions are append-only audit tables.

use std::io;

use serde_json::Value;

use super::tdengine::HistoryStore;
use crate::config::history_table_name;

#[derive(Clone, Debug, Default)]
pub struct BarState {
    pub sealed_through_us: u64,
    pub first_ts_us: u64,
    pub revision: u64,
    pub repair_floor_seq: u64,
    pub last_log: u64,
    pub last_ord: u32,
    pub writer_epoch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarGap {
    pub start_us: u64,
    pub end_us: u64,
    pub open: bool,
    pub noticed_us: u64,
}

fn ident(symbol: &str) -> io::Result<String> {
    let name = history_table_name(symbol);
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(io::Error::other(format!(
            "invalid history symbol {symbol:?}"
        )));
    }
    Ok(name)
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn u64_at(row: &[Value], index: usize) -> u64 {
    row.get(index).and_then(Value::as_u64).unwrap_or(0)
}

fn i64_at(row: &[Value], index: usize) -> i64 {
    row.get(index).and_then(Value::as_i64).unwrap_or(0)
}

pub async fn read_state(store: &HistoryStore, symbol: &str) -> io::Result<Option<BarState>> {
    let name = ident(symbol)?;
    let rows = store
        .sql(&format!("SELECT ts, sealed_through, first_ts, revision, repair_floor_seq, last_log, last_ord, writer_epoch FROM bar_state_{name} ORDER BY ts DESC LIMIT 1"))
        .await?;
    if let Some(row) = rows.first() {
        return Ok(Some(BarState {
            sealed_through_us: u64_at(row, 1),
            first_ts_us: u64_at(row, 2),
            revision: u64_at(row, 3),
            repair_floor_seq: u64_at(row, 4),
            last_log: u64_at(row, 5),
            last_ord: u64_at(row, 6) as u32,
            writer_epoch: row
                .get(7)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }));
    }
    // Old deployments (P0) have no bar_state row. Seed the cursor from the
    // newest materialized bar so a replacement writer never rewinds.
    let rows = store
        .sql(&format!(
            "SELECT ts, last_log, last_ord, last_ts FROM bars_1s_{name} ORDER BY ts DESC LIMIT 1"
        ))
        .await?;
    Ok(rows.first().map(|row| BarState {
        sealed_through_us: u64_at(row, 3),
        first_ts_us: u64_at(row, 0),
        last_log: u64_at(row, 1),
        last_ord: u64_at(row, 2) as u32,
        ..BarState::default()
    }))
}

#[allow(clippy::too_many_arguments)]
pub async fn write_state(
    store: &HistoryStore,
    symbol: &str,
    sealed_through_us: u64,
    first_ts_us: u64,
    revision: u64,
    repair_floor_seq: u64,
    last_log: u64,
    last_ord: u32,
    writer_epoch: &str,
) -> io::Result<()> {
    let table = format!("bar_state_{}", ident(symbol)?);
    store
        .sql(&format!(
            "INSERT INTO {table} VALUES (NOW, {sealed_through_us}, {first_ts_us}, {revision}, \
             {repair_floor_seq}, {last_log}, {last_ord}, {})",
            quote(writer_epoch)
        ))
        .await
        .map(|_| ())
}

/// Appends a revision audit row if `(open_ts, rev)` is not already recorded.
#[allow(clippy::too_many_arguments)]
pub async fn append_revision(
    store: &HistoryStore,
    symbol: &str,
    interval: &str,
    open_ts_us: u64,
    rev: u32,
    old_json: &str,
    new_json: &str,
    source: &str,
) -> io::Result<()> {
    let table = format!("bar_revisions_{}", ident(symbol)?);
    let existing = store
        .sql(&format!(
            "SELECT COUNT(*) FROM {table} WHERE open_ts={open_ts_us} AND rev={rev} AND i={}",
            quote(interval)
        ))
        .await?;
    if existing
        .first()
        .and_then(|row| row.first())
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        return Ok(());
    }
    store
        .sql(&format!(
            "INSERT INTO {table} VALUES (NOW, {}, {open_ts_us}, {rev}, {}, {}, {})",
            quote(interval),
            quote(old_json),
            quote(new_json),
            quote(source)
        ))
        .await
        .map(|_| ())
}

pub async fn append_gap(
    store: &HistoryStore,
    symbol: &str,
    start_us: u64,
    end_us: u64,
    open: bool,
) -> io::Result<()> {
    let table = format!("bar_gaps_{}", ident(symbol)?);
    let open = i32::from(open);
    store
        .sql(&format!(
            "INSERT INTO {table} VALUES (NOW, {start_us}, {end_us}, {open})"
        ))
        .await
        .map(|_| ())
}

/// Reduces the append-only gap table: newest notice per `(start, end)` wins.
pub async fn load_gaps(store: &HistoryStore, symbol: &str) -> io::Result<Vec<BarGap>> {
    let table = format!("bar_gaps_{}", ident(symbol)?);
    let rows = store
        .sql(&format!(
            "SELECT ts, gap_start, gap_end, open FROM {table} ORDER BY ts DESC"
        ))
        .await?;
    let mut seen = std::collections::HashSet::new();
    let mut gaps = Vec::new();
    for row in rows {
        let start_us = u64_at(&row, 1);
        let end_us = u64_at(&row, 2);
        if !seen.insert((start_us, end_us)) {
            continue;
        }
        gaps.push(BarGap {
            start_us,
            end_us,
            open: i64_at(&row, 3) != 0,
            noticed_us: u64_at(&row, 0),
        });
    }
    Ok(gaps)
}
