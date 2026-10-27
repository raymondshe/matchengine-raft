use std::path::PathBuf;

use clap::Parser;
use env_logger::Env;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::start_match_engine_node_with_storage;

#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long)]
    pub id: u64,

    #[clap(long)]
    pub http_addr: String,

    /// Transport for peer-to-peer Raft RPCs.
    ///
    /// The HTTP API and both server endpoints stay available; this only selects
    /// what this node dials when it talks to peers.
    #[clap(long, value_enum, default_value_t = Transport::Grpc)]
    pub transport: Transport,

    /// Root directory for durable state. Each node stores its log and snapshot
    /// under `<data-dir>/<instance-prefix>-<id>/`.
    #[clap(long, env = "RAFT_DATA_DIR", default_value = "/tmp/matchengine-raft")]
    pub data_dir: PathBuf,

    /// Prefix of the per-node directory, so independent clusters can share one data dir.
    #[clap(long, env = "RAFT_INSTANCE_PREFIX", default_value = "matchengine")]
    pub instance_prefix: String,

    /// Append every applied match event (fills, rests, cancels) to this JSONL file.
    ///
    /// Debugging aid: the file is node-local, not replicated, and replayed entries are
    /// written again after a restart. Omit to discard match results.
    #[clap(long, env = "MATCH_EVENT_LOG")]
    pub match_event_log: Option<PathBuf>,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Setup the logger
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    let mut storage =
        StorageConfig::new(options.data_dir).with_instance_prefix(options.instance_prefix);
    if let Some(path) = options.match_event_log {
        storage = storage.with_match_event_log(path);
    }

    start_match_engine_node_with_storage(
        options.id,
        options.http_addr,
        options.transport,
        matchengine_raft::default_raft_config(),
        storage,
    )
    .await
}
