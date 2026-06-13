//! The ONE codec (docs/protocol.md section 1). No other framing implementations are
//! permitted anywhere in the workspace.
//!
//! - QUIC control stream: `u32 big-endian length || Frame bytes`, decoded with
//!   [`FrameDecoder`] (cap checked BEFORE buffering the payload).
//! - WSS / QUIC datagrams: one binary message = one bare Frame
//!   ([`encode_frame_bare`] / [`decode_frame_bare`]).

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message as _;

use crate::{MAX_FRAME_BYTES, v1::Frame};

pub const LEN_PREFIX_BYTES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})")]
    TooLarge(usize),
    #[error("protobuf decode: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Encode with the u32-BE length prefix (QUIC control stream).
pub fn encode_frame(frame: &Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
    let len = frame.encoded_len();
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    dst.reserve(LEN_PREFIX_BYTES + len);
    dst.put_u32(len as u32);
    frame
        .encode(dst)
        .expect("BytesMut reserve guarantees capacity");
    Ok(())
}

/// Encode without a prefix (WS binary messages, QUIC datagrams self-delimit).
pub fn encode_frame_bare(frame: &Frame) -> Result<Bytes, FrameError> {
    let len = frame.encoded_len();
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = BytesMut::with_capacity(len);
    frame
        .encode(&mut buf)
        .expect("BytesMut reserve guarantees capacity");
    Ok(buf.freeze())
}

/// Decode a self-delimited frame (one WS message / one datagram).
pub fn decode_frame_bare(buf: &[u8]) -> Result<Frame, FrameError> {
    if buf.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(buf.len()));
    }
    Ok(Frame::decode(buf)?)
}

/// Incremental decoder for the length-prefixed QUIC control stream.
///
/// Bounded by construction: the announced length is validated against
/// `MAX_FRAME_BYTES` as soon as the 4-byte prefix arrives, before any payload
/// accumulation -- an attacker cannot make us allocate more than one frame.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: BytesMut,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw bytes from the transport. Fails fast on an oversized
    /// announced length so callers can close with PAYLOAD_TOO_LARGE.
    pub fn extend(&mut self, chunk: &[u8]) -> Result<(), FrameError> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() >= LEN_PREFIX_BYTES {
            let announced =
                u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if announced > MAX_FRAME_BYTES {
                return Err(FrameError::TooLarge(announced));
            }
        }
        Ok(())
    }

    /// Pop the next complete frame, if any.
    pub fn try_next(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buf.len() < LEN_PREFIX_BYTES {
            return Ok(None);
        }
        let announced =
            u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if announced > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge(announced));
        }
        if self.buf.len() < LEN_PREFIX_BYTES + announced {
            return Ok(None);
        }
        self.buf.advance(LEN_PREFIX_BYTES);
        let payload = self.buf.split_to(announced);
        Ok(Some(Frame::decode(payload.freeze())?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::FrameClass;
    use crate::v1::{self, frame::Payload};

    fn hello() -> Frame {
        Frame::control(Payload::Hello(v1::Hello {
            heartbeat_interval_ms: 30_000,
            resume_window_ms: 60_000,
            max_frame_bytes: MAX_FRAME_BYTES as u32,
        }))
    }

    #[test]
    fn round_trip_prefixed() {
        let mut buf = BytesMut::new();
        encode_frame(&hello(), &mut buf).unwrap();
        let mut dec = FrameDecoder::new();
        dec.extend(&buf).unwrap();
        let got = dec.try_next().unwrap().unwrap();
        assert_eq!(got, hello());
        assert!(dec.try_next().unwrap().is_none());
    }

    #[test]
    fn round_trip_bare() {
        let bytes = encode_frame_bare(&hello()).unwrap();
        assert_eq!(decode_frame_bare(&bytes).unwrap(), hello());
    }

    #[test]
    fn split_delivery_across_chunks() {
        let mut buf = BytesMut::new();
        encode_frame(&hello(), &mut buf).unwrap();
        encode_frame(&hello(), &mut buf).unwrap();
        let mut dec = FrameDecoder::new();
        let mid = buf.len() / 2;
        dec.extend(&buf[..3]).unwrap(); // not even a full prefix
        assert!(dec.try_next().unwrap().is_none());
        dec.extend(&buf[3..mid]).unwrap();
        dec.extend(&buf[mid..]).unwrap();
        assert!(dec.try_next().unwrap().is_some());
        assert!(dec.try_next().unwrap().is_some());
        assert!(dec.try_next().unwrap().is_none());
    }

    #[test]
    fn oversize_announced_length_rejected_before_payload() {
        let mut dec = FrameDecoder::new();
        let huge = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert!(matches!(dec.extend(&huge), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn oversize_encode_rejected() {
        let frame = Frame::with_nonce(
            7,
            Payload::SendMessage(v1::SendMessageRequest {
                channel_id: 1,
                content: "x".repeat(MAX_FRAME_BYTES + 1),
                reply_to_id: 0,
                attachment_ids: Vec::new(),
            }),
        );
        let mut buf = BytesMut::new();
        assert!(matches!(
            encode_frame(&frame, &mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    /// Forward-compat: a frame whose payload tag this build doesn't know decodes
    /// as payload=None, with seq/nonce intact (docs/protocol.md section 2 receiver policy).
    #[test]
    fn unknown_payload_decodes_as_none() {
        use prost::Message;
        let mut raw = Vec::new();
        // seq = 42 (field 1, varint)
        raw.extend_from_slice(&[0x08, 42]);
        // unknown field 1999, wire type 2 (length-delimited), 3 bytes
        let tag = (1999u32 << 3) | 2;
        let mut tagbuf = Vec::new();
        prost::encoding::encode_varint(tag as u64, &mut tagbuf);
        raw.extend_from_slice(&tagbuf);
        raw.extend_from_slice(&[3, 1, 2, 3]);
        let frame = Frame::decode(raw.as_slice()).unwrap();
        assert_eq!(frame.seq, 42);
        assert!(frame.payload.is_none());
        assert_eq!(frame.class(), FrameClass::Sequenced); // seq>0 => still ack it
    }

    #[test]
    fn classes() {
        assert_eq!(hello().class(), FrameClass::Control);
        let typing = Frame::dispatch(Payload::TypingStart(v1::TypingStart {
            channel_id: 1,
            user_id: 2,
        }));
        assert_eq!(typing.class(), FrameClass::Unsequenced);
        let msg = Frame::dispatch(Payload::MessageCreate(v1::MessageCreate::default()));
        assert_eq!(msg.class(), FrameClass::Sequenced);
    }
}
