#![no_main]

use libfuzzer_sys::fuzz_target;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::store::log_store::read_wal_bytes;

const SEGMENT_MAGIC: &[u8; 4] = b"MRFT";
const SEGMENT_VERSION: u32 = 1;

/// Builds one valid segment around arbitrary record payloads. A valid CRC is
/// required to reach the postcard record decoder; otherwise every input would
/// stop at the frame check.
fn segment(parts: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SEGMENT_MAGIC);
    bytes.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
    for payload in parts {
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        bytes.extend_from_slice(payload);
    }
    bytes
}

fuzz_target!(|data: &[u8]| {
    // Raw input exercises the header, frame length and CRC rejection paths.
    let _ = read_wal_bytes::<MatchEngineTypeConfig>(data);

    // One valid frame reaches the postcard `WalRecord` decoder.
    let _ = read_wal_bytes::<MatchEngineTypeConfig>(&segment(&[data]));

    // Two frames exercise the multi-record loop and the torn-tail boundary.
    let mid = data.len() / 2;
    let _ = read_wal_bytes::<MatchEngineTypeConfig>(&segment(&[&data[..mid], &data[mid..]]));
});
