//! gRPC (tonic) Raft transport.
//!
//! Envelopes are protobuf; payloads are `postcard`-encoded openraft types. gRPC
//! supplies HTTP/2 multiplexing, deadlines, and streaming; postcard keeps the
//! hot path binary.

use std::time::Duration;

pub mod client;
pub mod events;
pub mod metrics;
pub mod server;

pub mod pb {
    tonic::include_proto!("matchengine.raft.v1");
}

/// Upper bound for a single gRPC message in either direction.
///
/// openraft batches entries per append; this leaves ample room for the matching
/// engine's order payloads while bounding allocation on a hostile peer.
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// HTTP/2 per-stream flow-control window for Raft RPCs.
///
/// The default 64 KiB window forces frequent window updates for large append
/// batches and snapshot chunks. 1 MiB keeps connection memory bounded while
/// letting a single RPC make progress without a round trip per 64 KiB.
pub const HTTP2_STREAM_WINDOW: u32 = 1024 * 1024;

/// HTTP/2 per-connection flow-control window for Raft RPCs.
///
/// There are three Raft connections per peer (replication, heartbeat and
/// snapshot), so this is deliberately conservative for larger clusters.
pub const HTTP2_CONNECTION_WINDOW: u32 = 4 * 1024 * 1024;

/// HTTP/2 keepalive interval for Raft connections.
pub const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// HTTP/2 keepalive timeout for Raft connections.
pub const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Snapshot chunk size used when OpenRaft does not provide one.
///
/// Small enough to stay well below the receive limit and to let a cancellation
/// take effect promptly, large enough to amortize framing.
pub const SNAPSHOT_CHUNK_SIZE: usize = 256 * 1024;

/// Hard cap for a snapshot chunk taken from OpenRaft's config.
pub const SNAPSHOT_CHUNK_SIZE_MAX: usize = 8 * 1024 * 1024;

/// Buffer for the client -> server snapshot feed.
pub const SNAPSHOT_CHANNEL_SIZE: usize = 4;

/// In-flight AppendEntries requests for the bidirectional stream.
pub const STREAM_APPEND_CHANNEL_SIZE: usize = 64;

pub(crate) fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(value)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}
