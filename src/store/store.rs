use openraft::SnapshotMeta;
use crate::ExampleNodeId;
use tokio::io::{self, AsyncWriteExt};
use tokio::fs::File;
use crate::store::ExampleStore;

#[derive(Debug)]
pub struct ExampleSnapshot {
    pub meta: SnapshotMeta<ExampleNodeId>,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

impl ExampleStore {

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn write_file(&self) -> io::Result<()> {
        tracing::debug!("write_file: start");
        match &*self.current_snapshot.read().await {
            Some(snapshot) => {
                let file_name = format!(
                    "{}/{}-{}-{}-{}.bin",
                    self.config.snapshot_path, self.config.instance_prefix,
                    snapshot.meta.last_log_id.leader_id, snapshot.meta.last_log_id.index, snapshot.meta.snapshot_id
                );
                tracing::debug!("write_file: [{:?}, +oo)", file_name);
                let mut file = File::create(file_name).await?;
                file.write_all(snapshot.data.as_slice()).await?;   
            }
            None => (),
        }
        Ok(())
    }
}

