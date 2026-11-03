#![no_main]

use libfuzzer_sys::fuzz_target;
use matchengine_raft::market_data::wire;

fuzz_target!(|data: &[u8]| {
    // The reference decoder must reject arbitrary bytes without panicking.
    let _ = wire::decode(data);

    // Also explore the body decoders: force a valid magic/version and repair the
    // body length so mutated inputs reach the per-type parsing.
    if data.len() >= wire::HEADER_LEN {
        let mut frame = data.to_vec();
        frame[..4].copy_from_slice(&wire::MAGIC);
        frame[4] = wire::VERSION;
        let body_len = (frame.len() - wire::HEADER_LEN) as u32;
        frame[28..32].copy_from_slice(&body_len.to_le_bytes());
        let _ = wire::decode(&frame);
    }
});
