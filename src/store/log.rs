use crate::store::ExampleStateMachine;
use crate::store::ExampleStore;
use crate::ExampleNodeId;
use crate::ExampleTypeConfig;
use openraft::AnyError;
use openraft::Entry;
use openraft::ErrorSubject;
use openraft::ErrorVerb;
use openraft::StorageIOError;
use std::io;

impl ExampleStore {
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn write_log(&self, entries: &[&Entry<ExampleTypeConfig>]) {
        tracing::debug!("write_log: start");
        let mut sled = self.sled.write().await;
        let sm = self.state_machine.read().await;

        match &*sled {
            None => {
                let db_path = format!(
                    "{}/{}-{}.binlog",
                    self.config.journal_path, self.config.instance_prefix,
                    self.node_id
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
                    db.insert(entry.log_id.index.to_be_bytes(), data.unwrap().as_slice());
                }
            }
            _ => (),
        }
    }
}
