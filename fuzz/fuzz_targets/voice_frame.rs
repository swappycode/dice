//! Fuzz the voice datagram frame parser (`dice-voice-core`) against arbitrary
//! bytes — every inbound voice QUIC datagram is decoded here. The header is a
//! fixed 11 bytes and the payload is sliced out without copying, so the decoder
//! must reject short/oversized buffers and never panic on any input.
#![no_main]

use bytes::Bytes;
use dice_voice_core::VoiceFrame;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = VoiceFrame::decode(Bytes::copy_from_slice(data));
});
