use crate::store::ExampleStore;
use crate::ExampleNodeId;
use openraft::SnapshotMeta;
use tokio::fs::File;
use tokio::io::{AsyncReadExt,self, AsyncWriteExt};
use walkdir::WalkDir;
use std::io::Error;
use std::io::ErrorKind;


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
                    "{}/{}+{}+{}.bin",
                    self.config.snapshot_path,
                    self.config.instance_prefix,
                    self.node_id,
                    snapshot.meta.snapshot_id
                );
                tracing::debug!("write_file: [{:?}, +oo)", file_name);
                let mut file = File::create(file_name).await?;
                file.write_all(snapshot.data.as_slice()).await?;
            }
            None => (),
        }
        Ok(())
    }

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

    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn latest_snapshot_file(&self) -> Result<String, ()> {
        let mut max_index: u64 = 0;
        let mut latest_snapshot_file: String= String::from("");

        for entry in WalkDir::new(&self.config.snapshot_path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| !e.file_type().is_dir())
        {
            let f_name = String::from(entry.file_name().to_string_lossy());
            let mut s1 = f_name.split(".");
            let file = s1.next();
            let ext = s1.next();
            match ext.unwrap() {
                "bin" => {
                    tracing::debug!("file: {:?}", file);
                    let mut s3 = file.unwrap().split("+");
                    let prefix = s3.next();

                    match prefix {
                        Some(p) => if p != self.config.instance_prefix {
                                continue
                        }
                        None => continue
                    }
                    let node_id = s3.next().unwrap();
                    if node_id != self.node_id.to_string() {continue};
                    let snapshot_id = s3.next().unwrap();
 
                    let mut s2 = snapshot_id.split("-");
                    //TODO:
                    let term_id = s2.next();
                    let node_id = s2.next();
                    let index = s2.next();
                    let snapshot_id = s2.next();
                    
                    let index = u64::from_str_radix(index.unwrap(), 10).unwrap();
                    if index > max_index {
                        max_index = index;
                        latest_snapshot_file = f_name;
                    }
                }
                _ => (),
            }
        }
        if latest_snapshot_file.len() > 0 {
            Ok(format!("{}/{}",
                self.config.snapshot_path, latest_snapshot_file))   
        } else {
            Err(())
        }
    }
}
