use crate::store::ExampleStore;
use crate::ExampleNodeId;
use crate::ExampleTypeConfig;
use openraft::Entry;
use openraft::LogId;
use openraft::StorageError;

impl ExampleStore {
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn write_log(&self, entries: &[&Entry<ExampleTypeConfig>]) {
        tracing::debug!("write_log: start");
        let mut sled = self.sled.write().await;

        match &*sled {
            None => {
                let db_path = format!(
                    "{}/{}-{}.binlog",
                    self.config.journal_path, self.config.instance_prefix, self.node_id
                );
                let db = sled::open(db_path).unwrap();
                *sled = Some(db);
            }
            _ => (),
        }

        match &*sled {
            Some(db) => {
                for entry in entries {
                    let data = serde_json::to_vec(&entry);
                    //.map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)));
                    //.map_err(|e| e);
                    db.insert(entry.log_id.index.to_be_bytes(), data.unwrap().as_slice()).unwrap();
                }
            }
            _ => (),
        }
    }

    pub async fn purge_logfile_upto(
        &self,
        log_id: LogId<ExampleNodeId>,
    ) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("purge_logfile_upto: [{:?}, +oo)", log_id);
        self.write_file().await.unwrap();
        let sled = self.sled.write().await;
        match &*sled {
            Some(db) => {
                for kv in db.range(..=log_id.index.to_be_bytes()) {
                    db.remove(kv.unwrap().0).unwrap();
                }
            }
            _ => (),
        }
        Ok(())
    }
}
