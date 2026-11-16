#![no_main]

use libfuzzer_sys::fuzz_target;
use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::store::StateMachineData;
use matchengine_raft::store::state_machine::read_snapshot_bytes;

const SNAPSHOT_MAGIC: &[u8; 7] = b"MRFSNAP";
const SNAPSHOT_VERSION: u32 = 9;

/// Builds a valid snapshot header and CRC around an arbitrary body. This is
/// what lets the fuzzer reach the postcard decoder instead of stopping at the
/// checksum on every input.
fn framed(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SNAPSHOT_MAGIC.len() + 8 + body.len());
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&crc32fast::hash(body).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fuzz_target!(|data: &[u8]| {
    // Raw input exercises magic, version, header-length and CRC rejection.
    let _ = read_snapshot_bytes::<MatchEngineTypeConfig>(data);

    // A valid frame reaches the postcard `SnapshotPayload` decoder.
    let _ = read_snapshot_bytes::<MatchEngineTypeConfig>(&framed(data));

    // The state-machine body is decoded separately by `StateMachineStore::open`;
    // fuzz it directly so random payloads are not filtered by the meta layout.
    let _ = postcard::from_bytes::<StateMachineData>(data);
});
