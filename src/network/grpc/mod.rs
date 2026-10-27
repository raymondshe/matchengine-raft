//! gRPC (tonic) Raft transport.
//!
//! Envelopes are protobuf; payloads are `postcard`-encoded openraft types. gRPC
//! supplies HTTP/2 multiplexing, deadlines, and streaming; postcard keeps the
//! hot path binary.

pub mod client;
pub mod server;

pub mod pb {
    tonic::include_proto!("matchengine.raft.v1");
}

/// Upper bound for a single gRPC message in either direction.
///
/// openraft batches entries per append; this leaves ample room for the matching
/// engine's order payloads while bounding allocation on a hostile peer.
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Snapshot chunk size. Small enough to stay well below the receive limit and to
/// let a cancellation take effect promptly, large enough to amortize framing.
pub const SNAPSHOT_CHUNK_SIZE: usize = 256 * 1024;

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
