use std::ops::RangeBounds;
use std::ops::Bound;
use std::fmt::Debug;


use crate::store::ExampleStore;
use crate::ExampleNodeId;
use crate::ExampleTypeConfig;
use openraft::Entry;
use openraft::LogId;
use openraft::StorageError;
use openraft::StorageIOError;
use openraft::ErrorSubject;
use openraft::storage::LogState;
use openraft::LeaderId;

use std::convert::TryInto;
use std::ops::Bound::Excluded;
use std::ops::Bound::Included;

fn to_array<T, const N: usize>(v: Vec<T>) -> [T; N] {
    v.try_into()
        .unwrap_or_else(|v: Vec<T>| panic!("Expected a Vec of length {} but it was {}", N, v.len()))
}

impl ExampleStore {

    #[tracing::instrument(level = "debug", skip(self))]
    async fn get_log_state(&mut self) -> Result<LogState<ExampleTypeConfig>, StorageError<ExampleNodeId>> {
        // TODO: Seems not right. Need to handle LogId
        tracing::debug!("get_log_state: start");

        let sled = self.sled.read().await;
        let last = match &*sled {
            Some (db) => {
                let last_entry = db.iter().rev().next().unwrap();

                let last_entry : Entry<ExampleTypeConfig> = 
                    serde_json::from_slice(last_entry.unwrap().1.as_ref()).unwrap();
                
                Some(last_entry.log_id)
            },
            None => None
        };

        let last_purged = match &*sled {
            Some (db) => {
                let last_entry = db.iter().next().unwrap();

                let last_entry : Entry<ExampleTypeConfig> = 
                    serde_json::from_slice(last_entry.unwrap().1.as_ref()).unwrap();
                
                Some(last_entry.log_id)
            },
            None => None
        };

        let log_state = LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        };

        tracing::debug!("get_log_state: {:?}", log_state);
        Ok(log_state)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<ExampleTypeConfig>>, StorageError<ExampleNodeId>> {
        tracing::debug!("get_log_state: start");

        let sled = self.sled.read().await;
        match &*sled {
            Some (db) => {
                let response = match (range.start_bound(), range.end_bound()) {
                    (Unbounded, Excluded(x)) => db.range(..x.to_be_bytes()),
                    (Unbounded, Included(x)) => db.range(..=x.to_be_bytes()),
                    (Included(x), Included(y)) => db.range(x.to_be_bytes()..=y.to_be_bytes()),
                    (Excluded(x), Included(y)) => db.range(x.to_be_bytes()..=y.to_be_bytes()),
                    (Excluded(x), Excluded(y)) => db.range(x.to_be_bytes()..y.to_be_bytes()),
                    (Excluded(x), Unbounded) => db.range(x.to_be_bytes()..),
                    (Included(x), Excluded(y)) => db.range(x.to_be_bytes()..y.to_be_bytes()),
                    (Included(x), Unbounded) => db.range(x.to_be_bytes()..),
                    (Unbounded, _) => {let i: u64 = 0; db.range(i.to_be_bytes()..)},
                };
             
                let response = response.map(|entry| { 
                        let last_entry : Entry<ExampleTypeConfig> =
                            serde_json::from_slice(entry.unwrap().1.as_ref()).unwrap();
                        last_entry
                }) .collect::<Vec<_>>();
                Ok(response)
            },
            _ => Ok(vec![])
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn append_to_log_file(&self, entries: &[&Entry<ExampleTypeConfig>]) {
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

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn delete_conflict_logs_since_file(
        &self,
        log_id: LogId<ExampleNodeId>,
    ) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("delete_conflict_log_since: [{:?}, +oo)", log_id);

        let sled = self.sled.write().await;
        match &*sled {
            Some(db) => {
                for kv in db.range(log_id.index.to_be_bytes()..) {
                    db.remove(kv.unwrap().0).unwrap();
                }
            }
            _ => (),
        }

        Ok(())
    }

    pub async fn purge_logfile_upto_file(
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
