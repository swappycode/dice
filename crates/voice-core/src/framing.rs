//! The voice frame wire format.
//!
//! A [`VoiceFrame`] is an RTP-inspired envelope around an opaque Opus payload.
//! It travels as a **bare** QUIC datagram (no length prefix — datagrams
//! self-delimit, per the protocol spec). The header is a fixed 11 bytes:
//!
//! ```text
//! byte  0      : flags   (bit 0 = marker / start-of-talkspurt)
//! bytes 1..=4  : ssrc      u32 big-endian
//! bytes 5..=6  : seq       u16 big-endian
//! bytes 7..=10 : timestamp u32 big-endian
//! bytes 11..   : opus payload (verbatim, never inspected here)
//! ```
//!
//! Big-endian matches the rest of the Dice wire (`u32`-BE control-stream
//! framing). The encoder/decoder are allocation-frugal: [`VoiceFrame::decode`]
//! slices the payload out of the input `Bytes` without copying.

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Fixed size of the voice frame header, in bytes.
pub const HEADER_LEN: usize = 11;

/// Upper bound on an encoded voice frame. A 20 ms Opus frame is well under
/// 1 KiB even at the top 64 kbps tier, so this is a sanity guard (roughly one
/// network MTU) rather than a tight bound — it rejects obviously-corrupt
/// datagrams before any further work.
pub const MAX_FRAME_BYTES: usize = 1500;

/// One voice packet: metadata plus an opaque Opus payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceFrame {
    /// Synchronization source — identifies the speaker's stream within a voice
    /// channel. Assigned per join and stable for its lifetime, so a receiver can
    /// keep a separate jitter buffer per ssrc.
    pub ssrc: u32,
    /// Per-stream sequence number; increments by one per frame and wraps at
    /// [`u16::MAX`]. The jitter buffer uses it to reorder and detect loss.
    pub seq: u16,
    /// Sample-clock timestamp (48 kHz) of the first sample in the payload; wraps
    /// at [`u32::MAX`]. Carried for the playout clock; the jitter buffer keys off
    /// `seq`, not this.
    pub timestamp: u32,
    /// Start-of-talkspurt marker — set on the first frame after silence so the
    /// receiver can reset its jitter estimate and playout delay.
    pub marker: bool,
    /// Opaque Opus payload. `voice-core` carries it verbatim and never decodes
    /// it.
    pub payload: Bytes,
}

/// Why a buffer could not be parsed as a [`VoiceFrame`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VoiceFrameError {
    /// Fewer than [`HEADER_LEN`] bytes — not even a complete header.
    #[error("voice frame too short (need at least an 11-byte header)")]
    TooShort,
    /// Larger than [`MAX_FRAME_BYTES`]; rejected before further processing.
    #[error("voice frame too large: {0} bytes")]
    TooLarge(usize),
}

const FLAG_MARKER: u8 = 0x01;

impl VoiceFrame {
    /// Total encoded length: header plus payload.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    /// Encode into a freshly allocated [`Bytes`].
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut dst = BytesMut::with_capacity(self.encoded_len());
        self.encode_into(&mut dst);
        dst.freeze()
    }

    /// Encode by appending to an existing buffer (reuses the caller's
    /// allocation across many frames).
    pub fn encode_into(&self, dst: &mut BytesMut) {
        dst.reserve(self.encoded_len());
        dst.put_u8(if self.marker { FLAG_MARKER } else { 0 });
        dst.put_u32(self.ssrc);
        dst.put_u16(self.seq);
        dst.put_u32(self.timestamp);
        dst.extend_from_slice(&self.payload);
    }

    /// Decode a bare voice frame. The payload is sliced out of `buf` with no
    /// copy. An empty payload is permitted (a comfort-noise / DTX frame).
    pub fn decode(mut buf: Bytes) -> Result<Self, VoiceFrameError> {
        if buf.len() < HEADER_LEN {
            return Err(VoiceFrameError::TooShort);
        }
        if buf.len() > MAX_FRAME_BYTES {
            return Err(VoiceFrameError::TooLarge(buf.len()));
        }
        let flags = buf.get_u8();
        let ssrc = buf.get_u32();
        let seq = buf.get_u16();
        let timestamp = buf.get_u32();
        // `buf` now holds exactly the payload (a zero-copy slice of the input).
        Ok(Self {
            ssrc,
            seq,
            timestamp,
            marker: flags & FLAG_MARKER != 0,
            payload: buf,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> VoiceFrame {
        VoiceFrame {
            ssrc: 0xDEAD_BEEF,
            seq: 0x1234,
            timestamp: 0x00AB_CDEF,
            marker: true,
            payload: Bytes::from_static(&[1, 2, 3, 4, 5]),
        }
    }

    #[test]
    fn round_trips() {
        let f = sample();
        let bytes = f.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        assert_eq!(VoiceFrame::decode(bytes).unwrap(), f);
    }

    #[test]
    fn marker_bit_round_trips_both_ways() {
        for marker in [true, false] {
            let mut f = sample();
            f.marker = marker;
            assert_eq!(VoiceFrame::decode(f.encode()).unwrap().marker, marker);
        }
    }

    #[test]
    fn empty_payload_is_valid() {
        let mut f = sample();
        f.payload = Bytes::new();
        let bytes = f.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        let decoded = VoiceFrame::decode(bytes).unwrap();
        assert!(decoded.payload.is_empty());
        assert_eq!(decoded.ssrc, f.ssrc);
    }

    #[test]
    fn header_field_order_is_big_endian() {
        let bytes = sample().encode();
        assert_eq!(bytes[0], FLAG_MARKER);
        assert_eq!(&bytes[1..5], &0xDEAD_BEEFu32.to_be_bytes());
        assert_eq!(&bytes[5..7], &0x1234u16.to_be_bytes());
        assert_eq!(&bytes[7..11], &0x00AB_CDEFu32.to_be_bytes());
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(
            VoiceFrame::decode(Bytes::from_static(&[0; HEADER_LEN - 1])),
            Err(VoiceFrameError::TooShort)
        );
    }

    #[test]
    fn rejects_oversize_buffer() {
        let big = Bytes::from(vec![0u8; MAX_FRAME_BYTES + 1]);
        assert_eq!(
            VoiceFrame::decode(big),
            Err(VoiceFrameError::TooLarge(MAX_FRAME_BYTES + 1))
        );
    }

    #[test]
    fn payload_is_a_zero_copy_slice() {
        // Decoding shares the backing allocation with the input buffer.
        let f = sample();
        let encoded = f.encode();
        let decoded = VoiceFrame::decode(encoded).unwrap();
        assert_eq!(&decoded.payload[..], &[1, 2, 3, 4, 5]);
    }
}
