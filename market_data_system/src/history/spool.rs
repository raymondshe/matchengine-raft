//! Rolling local trade spool for the historical writer.
//!
//! Every consumed trade is appended as one JSON line to
//! `<spool_dir>/<symbol>/<day>.trades`, where `day` is the UTC day number. The
//! file is fsynced on a short cadence by the writer task. A same-pod restart
//! replays from the newest bar cursor; a replacement pod with an empty spool
//! falls back to the bar row's own cursor.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::HistoryTrade;
use crate::config::history_table_name;

const DAY_MS: u64 = 86_400_000;

type Writer = (u64, BufWriter<File>);

pub struct TradeSpool {
    dir: PathBuf,
    days: u64,
    writers: HashMap<String, Writer>,
    last_sync: Instant,
}

impl TradeSpool {
    pub fn open(dir: impl Into<PathBuf>, days: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            days: days.max(1),
            writers: HashMap::new(),
            last_sync: Instant::now(),
        })
    }

    fn day(ts_ms: u64) -> u64 {
        ts_ms / DAY_MS
    }

    fn path(&self, symbol: &str, day: u64) -> PathBuf {
        self.dir
            .join(history_table_name(symbol))
            .join(format!("{day}.trades"))
    }

    pub fn append(&mut self, trade: &HistoryTrade) -> std::io::Result<()> {
        let day = Self::day(trade.ts_ms);
        let symbol = history_table_name(&trade.symbol);
        let needs_new = self
            .writers
            .get(&symbol)
            .is_none_or(|(current, _)| *current != day);
        if needs_new {
            if let Some((_, mut old)) = self.writers.remove(&symbol) {
                old.flush()?;
            }
            let path = self.path(&trade.symbol, day);
            std::fs::create_dir_all(path.parent().expect("spool file parent"))?;
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.writers
                .insert(symbol.clone(), (day, BufWriter::new(file)));
        }
        let (_, writer) = self
            .writers
            .get_mut(&symbol)
            .expect("writer inserted above");
        serde_json::to_writer(&mut *writer, trade).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
        Ok(())
    }

    /// Flush and fsync all open spool files. Call on the writer's 1 s cadence.
    pub fn sync(&mut self) -> std::io::Result<()> {
        for (_, writer) in self.writers.values_mut() {
            writer.flush()?;
            writer.get_ref().sync_data()?;
        }
        self.last_sync = Instant::now();
        Ok(())
    }

    pub fn since_sync(&self) -> std::time::Duration {
        self.last_sync.elapsed()
    }

    /// Replay trades strictly after `(log_index, ordinal)` for one symbol.
    pub fn replay(
        &self,
        symbol: &str,
        after: Option<(u64, u32)>,
    ) -> std::io::Result<Vec<HistoryTrade>> {
        let dir = self.dir.join(history_table_name(symbol));
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) == Some("trades") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let mut trades = Vec::new();
        for path in files {
            let content = std::fs::read_to_string(path)?;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let trade: HistoryTrade =
                    serde_json::from_str(line).map_err(std::io::Error::other)?;
                let position = (trade.log_index, trade.ordinal);
                if after.is_none_or(|cursor| position > cursor) {
                    trades.push(trade);
                }
            }
        }
        Ok(trades)
    }

    /// Remove spool files older than the configured retention window.
    pub fn prune(&self, now_ms: u64) -> std::io::Result<()> {
        let cutoff = Self::day(now_ms).saturating_sub(self.days);
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&path) {
                Ok(files) => files,
                Err(_) => continue,
            };
            for file in files.flatten() {
                let name = file.file_name();
                let name = name.to_string_lossy();
                let Some(day) = name
                    .strip_suffix(".trades")
                    .and_then(|v| v.parse::<u64>().ok())
                else {
                    continue;
                };
                if day < cutoff {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        Ok(())
    }
}

/// Commands for the blocking spool actor. Sending these over a Tokio channel
/// keeps file writes, fsync and directory pruning off Tokio workers.
pub enum SpoolCommand {
    Append(HistoryTrade),
    Sync,
    Prune(u64),
}

/// Takes ownership of an opened spool and runs it on a blocking thread. When
/// the async writer drops the sender, the actor flushes and exits.
pub fn actor(mut spool: TradeSpool) -> (mpsc::Sender<SpoolCommand>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<SpoolCommand>(8_192);
    let handle = tokio::task::spawn_blocking(move || {
        while let Some(command) = rx.blocking_recv() {
            match command {
                SpoolCommand::Append(trade) => {
                    if let Err(error) = spool.append(&trade) {
                        tracing::warn!(%error, "trade spool append failed");
                    }
                }
                SpoolCommand::Sync => {
                    if let Err(error) = spool.sync() {
                        tracing::warn!(%error, "trade spool fsync failed");
                    }
                }
                SpoolCommand::Prune(now_ms) => {
                    if let Err(error) = spool.prune(now_ms) {
                        tracing::warn!(%error, "trade spool prune failed");
                    }
                }
            }
        }
        let _ = spool.sync();
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchengine_raft::order_book::Fixed;

    fn trade(symbol: &str, day: u64, log_index: u64, ordinal: u32) -> HistoryTrade {
        HistoryTrade {
            symbol: symbol.to_string(),
            symbol_id: 1,
            ts_ms: day * DAY_MS + 1_000,
            price: "1.085".parse::<Fixed>().unwrap(),
            volume: "1".parse::<Fixed>().unwrap(),
            quote_volume: "1.085".parse::<Fixed>().unwrap(),
            taker_buy: true,
            log_index,
            ordinal,
            market_seq: log_index,
        }
    }

    #[test]
    fn spool_appends_replays_and_prunes() {
        let dir = std::env::temp_dir().join(format!("md-spool-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut spool = TradeSpool::open(&dir, 2).unwrap();
        spool.append(&trade("eurusd", 100, 1, 0)).unwrap();
        spool.append(&trade("eurusd", 100, 2, 0)).unwrap();
        spool.append(&trade("eurusd", 103, 3, 0)).unwrap();
        spool.sync().unwrap();

        assert_eq!(spool.replay("eurusd", None).unwrap().len(), 3);
        assert_eq!(spool.replay("eurusd", Some((1, 0))).unwrap().len(), 2);
        spool.prune(105 * DAY_MS).unwrap();
        assert_eq!(spool.replay("eurusd", None).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
