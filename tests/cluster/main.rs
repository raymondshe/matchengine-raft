mod test_cluster;

use std::sync::Arc;
use openraft::StorageError;
use openraft::testing::Suite;
use example_raft_key_value::store::ExampleStore;
use example_raft_key_value::ExampleNodeId;

pub async fn new_async() -> Arc<ExampleStore> {
    let res = ExampleStore::open_create(0);

    Arc::new(res)
}

#[test]
pub fn test_mem_store() -> Result<(), StorageError<ExampleNodeId>> {
    Suite::test_all(new_async)?;
    Ok(())
}
