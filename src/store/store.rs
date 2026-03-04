use tokio::fs::File;
use tokio::io::{AsyncReadExt,self, AsyncWriteExt};
use tokio::fs::OpenOptions;

use walkdir::WalkDir;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Cursor;

use openraft::storage::Snapshot;
use openraft::SnapshotMeta;
use openraft::StorageError;

use crate::ExampleTypeConfig;
use crate::store::ExampleStore;
use crate::ExampleNodeId;
use crate::store::ExampleStateMachine;

/// A snapshot of the Raft state machine (file-based operations).
///
/// This struct is used specifically for file-based snapshot operations
/// in the store module.
#[derive(Debug, Clone)]
pub struct ExampleSnapshot {
    /// Metadata about this snapshot, including the last log ID applied.
    pub meta: SnapshotMeta<ExampleNodeId>,

    /// The serialized data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/// File-based snapshot operations for the Raft storage.
///
/// This impl block provides methods for reading, writing, and managing
/// state machine snapshots stored as files on disk.
impl ExampleStore {

    /// Writes the current snapshot to disk.
    ///
    /// This method writes the in-memory snapshot to a file. To ensure
    /// atomicity, it first writes to a temporary file and then renames
    /// it to the final filename. This prevents partial or corrupted
    /// snapshots if the process crashes during writing.
    ///
    /// The snapshot filename format is: `{prefix}+{node_id}+{snapshot_id}.bin`
    ///
    /// # Returns
    ///
    /// `Ok(())` if the snapshot was written successfully, or an error
    /// if file operations failed.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn write_snapshot(&self) -> io::Result<()> {
        tracing::debug!("write_snapshot: start");

        let snapshot_opt = self.current_snapshot.read().await.clone();
        let snapshot = match &snapshot_opt {
            Some(s) => s,
            None => {
                tracing::debug!("write_snapshot: no snapshot to write");
                return Ok(());
            }
        };

        // Ensure snapshot directory exists
        tokio::fs::create_dir_all(&self.config.snapshot_path).await?;

        let file_name = format!(
            "{}/{}+{}+{}.bin",
            self.config.snapshot_path,
            self.config.instance_prefix,
            self.node_id,
            snapshot.meta.snapshot_id
        );
        tracing::debug!("write_snapshot: writing to {:?}", file_name);

        // Write to a temp file first, then rename for atomicity
        let temp_file_name = format!("{}.tmp", file_name);

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_file_name)
            .await?;

        file.write_all(snapshot.data.as_slice()).await?;
        file.flush().await?;

        // Atomic rename
        tokio::fs::rename(&temp_file_name, &file_name).await?;

        tracing::debug!("write_snapshot: completed successfully");
        Ok(())
    }

    /// Reads the latest snapshot file from disk.
    ///
    /// This method finds the most recent snapshot file and reads its
    /// entire contents into memory.
    ///
    /// # Returns
    ///
    /// The raw bytes of the snapshot file, or an error if no snapshot
    /// was found or the file could not be read.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn read_snapshot_file(&self) -> io::Result<Vec<u8>> {
        let latest_file = match self.latest_snapshot_file().await {
            Ok(file) => file,
            _ => return Err(Error::new(ErrorKind::NotFound,"No snapshot files")),
        };
        tracing::debug!("read_file: {}", latest_file);

        let file = File::open(&latest_file).await;
        let mut file = match file {
            Ok(file) => file,
            Err(e) => return Err(e),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data).await?;

        Ok(data)
    }

    /// Finds the most recent snapshot file on disk.
    ///
    /// This method scans the snapshot directory and finds the snapshot
    /// with the highest log index in its filename. The filename is
    /// parsed to extract the log index.
    ///
    /// Only files matching the pattern `{prefix}+{node_id}+*.bin` are
    /// considered.
    ///
    /// # Returns
    ///
    /// The full path to the latest snapshot file, or an error if no
    /// valid snapshot files were found.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn latest_snapshot_file(&self) -> Result<String, ()> {
        let mut max_index: u64 = 0;
        let mut latest_snapshot_file: String = String::new();

        let snapshot_dir = match std::path::Path::new(&self.config.snapshot_path).metadata() {
            Ok(meta) if meta.is_dir() => &self.config.snapshot_path,
            _ => {
                tracing::debug!("snapshot directory does not exist or is not a directory");
                return Err(());
            }
        };

        for entry in WalkDir::new(snapshot_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| !e.file_type().is_dir())
        {
            let f_name = String::from(entry.file_name().to_string_lossy());
            let mut s1 = f_name.split('.');
            let file = match s1.next() {
                Some(f) => f,
                None => continue,
            };
            let ext = match s1.next() {
                Some(e) => e,
                None => continue,
            };

            if ext != "bin" {
                continue;
            }

            tracing::debug!("file: {:?}", file);
            let mut s3 = file.split('+');
            let prefix = match s3.next() {
                Some(p) => p,
                None => continue,
            };

            if prefix != self.config.instance_prefix {
                continue;
            }

            let node_id_str = match s3.next() {
                Some(n) => n,
                None => continue,
            };

            if node_id_str != self.node_id.to_string() {
                continue;
            }

            let snapshot_id = match s3.next() {
                Some(s) => s,
                None => continue,
            };

            let mut s2 = snapshot_id.split('-');
            // Skip term_id and node_id parts, get to the index
            let _ = s2.next(); // term_id
            let _ = s2.next(); // node_id
            let index_str = match s2.next() {
                Some(i) => i,
                None => continue,
            };

            let index = match u64::from_str_radix(index_str, 10) {
                Ok(i) => i,
                Err(_) => continue,
            };

            if index > max_index {
                max_index = index;
                latest_snapshot_file = f_name;
            }
        }

        if !latest_snapshot_file.is_empty() {
            Ok(format!("{}/{}", self.config.snapshot_path, latest_snapshot_file))
        } else {
            Err(())
        }
    }

    /// Loads the latest snapshot, either from memory or from disk.
    ///
    /// This method first checks if there's a snapshot already loaded in
    /// memory. If not, it attempts to read the latest snapshot from disk.
    /// The snapshot data is deserialized and returned in a format ready
    /// for Raft to install.
    ///
    /// # Returns
    ///
    /// The latest snapshot wrapped in an `Option`, or `None` if no
    /// snapshot was available or it could not be loaded.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn load_latest_snapshot(
        &self,
    ) -> Result<Option<Snapshot<ExampleTypeConfig, Cursor<Vec<u8>>>>, StorageError<ExampleNodeId>> {
        tracing::debug!("load_latest_snapshot: start");

        match &*self.current_snapshot.read().await {
            Some(snapshot) => {
                let data = snapshot.data.clone();
                Ok(Some(Snapshot {
                    meta: snapshot.meta.clone(),
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
            None => {
                let data = match self.read_snapshot_file().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!("load_latest_snapshot: no snapshot found: {}", e);
                        return Ok(None);
                    }
                };

                let content: ExampleStateMachine = match serde_json::from_slice(&data) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("load_latest_snapshot: failed to deserialize snapshot: {}", e);
                        return Ok(None);
                    }
                };

                let last_applied_log = match content.last_applied_log {
                    Some(log) => log,
                    None => {
                        tracing::error!("load_latest_snapshot: snapshot missing last_applied_log");
                        return Ok(None);
                    }
                };

                tracing::debug!("load_latest_snapshot: last_applied_log = {:?}", last_applied_log);

                let snapshot_idx = {
                    let mut l = self.snapshot_idx.lock().unwrap();
                    *l += 1;
                    *l
                };

                let snapshot_id = format!(
                    "{}-{}-{}",
                    last_applied_log.leader_id, last_applied_log.index, snapshot_idx
                );

                let meta = SnapshotMeta {
                    last_log_id: last_applied_log,
                    snapshot_id,
                };

                tracing::debug!("load_latest_snapshot: meta {:?}", meta);

                Ok(Some(Snapshot {
                    meta,
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
        }
    }
}
