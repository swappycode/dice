//! Fuzz the ONE realtime frame codec (docs/protocol.md §1) against arbitrary,
//! attacker-controlled bytes — every QUIC/WSS byte hits this before auth.
//!
//! Two entry points are exercised:
//!  - `decode_frame_bare` — one self-delimited WS message / QUIC datagram,
//!  - `FrameDecoder` — the length-prefixed QUIC control-stream decoder, fed in
//!    two arbitrary chunks so the partial-buffer + announced-length cap paths
//!    (which must reject before allocating) are covered.
//!
//! The contract under test: NO input may panic, hang, or allocate beyond the
//! frame cap. libFuzzer flags a crash on any panic.
#![no_main]

use dice_protocol::framing::{FrameDecoder, decode_frame_bare};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Bare decode (one datagram / WS binary message).
    let _ = decode_frame_bare(data);

    // Streaming length-prefixed decode, split at an arbitrary point so the
    // cross-chunk reassembly + cap check are fuzzed.
    let mut dec = FrameDecoder::new();
    let mid = data.len() / 2;
    if dec.extend(&data[..mid]).is_ok() {
        let _ = dec.try_next();
        if dec.extend(&data[mid..]).is_ok() {
            // Drain every complete frame; stop on the first error or when empty.
            while let Ok(Some(_)) = dec.try_next() {}
        }
    }
});
