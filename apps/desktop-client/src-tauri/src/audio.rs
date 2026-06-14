//! Voice audio pipeline (M3 phase 3).
//!
//! The canonical voice format is **48 kHz mono, 20 ms frames** (960 samples) —
//! Opus-native, low-latency, and the shape the gateway SFU forwards. This
//! module owns the codec (behind [`VoiceCodec`] so a passthrough/other codec
//! can drop in) and, in a later step, the `cpal` capture/playback engine that
//! wires mic → encode → [`GatewayHandle::send_voice`] and `VoiceData` →
//! jitter buffer → decode → speaker.
//!
//! Codec correctness is unit-tested here (encode→decode round-trip) without any
//! audio hardware; the device I/O is verified on-hardware.

/// Voice sample rate (Hz). Opus-native; the whole pipeline runs at this rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Frame duration (ms). 20 ms is the Opus/voice default.
pub const FRAME_MS: u32 = 20;
/// Samples in one mono frame (960 @ 48 kHz / 20 ms).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;

/// Encode/decode one 20 ms mono frame. Behind a trait so the rest of the
/// pipeline never names a concrete codec.
pub trait VoiceCodec: Send {
    /// Encode `pcm` (exactly [`FRAME_SAMPLES`] i16 mono samples) to a packet.
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError>;
    /// Decode `packet` into `out` ([`FRAME_SAMPLES`] capacity), returning the
    /// sample count. `packet = None` signals packet loss → the codec runs PLC
    /// and still produces a frame.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize, CodecError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus: {0}")]
    Opus(String),
}

/// Opus codec (libopus via `audiopus`), VoIP-tuned, 48 kHz mono.
pub struct OpusCodec {
    encoder: audiopus::coder::Encoder,
    decoder: audiopus::coder::Decoder,
    /// Reusable encode scratch (Opus packets are < 1 KiB at voice bitrates).
    scratch: Vec<u8>,
}

impl OpusCodec {
    pub fn new() -> Result<Self, CodecError> {
        use audiopus::{Application, Channels, SampleRate};
        let encoder =
            audiopus::coder::Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
                .map_err(opus_err)?;
        let decoder =
            audiopus::coder::Decoder::new(SampleRate::Hz48000, Channels::Mono).map_err(opus_err)?;
        Ok(Self {
            encoder,
            decoder,
            scratch: vec![0u8; 4000],
        })
    }
}

fn opus_err(e: audiopus::Error) -> CodecError {
    CodecError::Opus(e.to_string())
}

impl VoiceCodec for OpusCodec {
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError> {
        let n = self.encoder.encode(pcm, &mut self.scratch).map_err(opus_err)?;
        Ok(self.scratch[..n].to_vec())
    }

    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize, CodecError> {
        self.decoder.decode(packet, out, false).map_err(opus_err)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Opus actually links and round-trips a 20 ms frame — no audio hardware.
    #[test]
    fn opus_round_trips_a_frame() {
        let mut codec = OpusCodec::new().unwrap();
        // A quiet 440 Hz-ish tone over one 20 ms frame.
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 6000.0) as i16
            })
            .collect();

        let packet = codec.encode(&pcm).unwrap();
        assert!(
            !packet.is_empty() && packet.len() < 400,
            "voice packet should be small, got {}",
            packet.len()
        );

        let mut out = vec![0i16; FRAME_SAMPLES];
        let decoded = codec.decode(Some(&packet), &mut out).unwrap();
        assert_eq!(decoded, FRAME_SAMPLES, "one frame decodes to one frame");

        // Packet loss: PLC still yields a full frame.
        let concealed = codec.decode(None, &mut out).unwrap();
        assert_eq!(concealed, FRAME_SAMPLES, "PLC produces a frame on loss");
    }
}
