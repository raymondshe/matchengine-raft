//! Small subscriber-to-engine snapshot request used by Aeron stream 2010.
//!
//! The market-data streams are one-way publications; this is the only
//! subscriber-to-engine message. It is a fixed 24-byte little-endian frame so
//! the service and engine can decode it without the market-data wire version
//! changing.
//!
//! Layout:
//!
//! ```text
//! 0..4   "MSRQ"
//! 4      version (1)
//! 5      reserved (0)
//! 6..8   depth_levels u16 LE
//! 8..12  symbol_id u32 LE
//! 12..20 request_id u64 LE
//! 20..24 reserved (0)
//! ```

/// Stream offset from the configured market-data stream base: 2001 + 9 = 2010.
pub const STREAM_OFFSET: u16 = 9;
/// Fixed encoded length.
pub const ENCODED_LEN: usize = 24;
/// Magic prefix.
pub const MAGIC: &[u8; 4] = b"MSRQ";
/// Current request format version.
pub const VERSION: u8 = 1;
/// Largest depth request the engine accepts; deeper requests are clamped.
pub const MAX_DEPTH_LEVELS: u16 = 200;

/// A subscriber request for a fresh depth snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRequest {
    /// Instrument id expected by the engine.
    pub symbol_id: u32,
    /// Requested depth levels per side (clamped by the engine).
    pub depth_levels: u16,
    /// Caller-chosen id for logs and duplicate detection; not echoed.
    pub request_id: u64,
}

impl SnapshotRequest {
    /// Builds a request for `symbol_id` at `depth_levels`.
    pub fn new(symbol_id: u32, depth_levels: u16, request_id: u64) -> Self {
        Self {
            symbol_id,
            depth_levels: depth_levels.clamp(1, MAX_DEPTH_LEVELS),
            request_id,
        }
    }

    /// Encodes the fixed 24-byte request frame.
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let mut bytes = [0u8; ENCODED_LEN];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4] = VERSION;
        bytes[6..8].copy_from_slice(&self.depth_levels.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.symbol_id.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.request_id.to_le_bytes());
        bytes
    }

    /// Decodes one request frame, rejecting a wrong magic, version or length.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() != ENCODED_LEN {
            return Err("snapshot request must be exactly 24 bytes");
        }
        if &bytes[0..4] != MAGIC {
            return Err("snapshot request magic mismatch");
        }
        if bytes[4] != VERSION {
            return Err("unsupported snapshot request version");
        }
        let depth_levels = u16::from_le_bytes([bytes[6], bytes[7]]);
        let symbol_id = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let request_id = u64::from_le_bytes([
            bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
        ]);
        Ok(Self {
            symbol_id,
            depth_levels: depth_levels.clamp(1, MAX_DEPTH_LEVELS),
            request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_and_clamps_depth() {
        let request = SnapshotRequest::new(7, 1_000, 42);
        assert_eq!(MAX_DEPTH_LEVELS, request.depth_levels);
        let encoded = request.encode();
        assert_eq!(ENCODED_LEN, encoded.len());
        assert_eq!(Ok(request), SnapshotRequest::decode(&encoded));

        assert_eq!(
            Err("snapshot request must be exactly 24 bytes"),
            SnapshotRequest::decode(&encoded[..ENCODED_LEN - 1])
        );

        let mut bad = encoded;
        bad[0] = b'X';
        assert_eq!(
            Err("snapshot request magic mismatch"),
            SnapshotRequest::decode(&bad)
        );

        let mut bad = encoded;
        bad[4] = 2;
        assert_eq!(
            Err("unsupported snapshot request version"),
            SnapshotRequest::decode(&bad)
        );
    }
}
