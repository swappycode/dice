//! Generated wire types (`dice.v1`, `dice.internal.v1`) and the single framing
//! codec shared by the gateway, the desktop client, and the bus.
//!
//! The wire contract lives in `docs/protocol.md` (normative) — not in this code.

pub mod framing;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/dice.v1.rs"));
}
pub mod internal {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/dice.internal.v1.rs"));
    }
}

// Consumers take prost/bytes from here so versions can never skew.
pub use bytes;
pub use prost;

pub const PROTOCOL_VERSION: u32 = 1;
pub const ALPN_GATEWAY: &[u8] = b"dice/1";
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const HEARTBEAT_INTERVAL_MS: u32 = 30_000;
pub const RESUME_WINDOW_MS: u32 = 60_000;
/// WS close code / QUIC application close code = 4000 + ErrorCode.
pub const CLOSE_CODE_BASE: u32 = 4000;

/// `Identify.capabilities` bit 0: the client sends and accepts voice audio over
/// QUIC datagrams (the SFU path). Set only on a QUIC transport; over WSS the
/// gateway leaves it clear and voice audio is unavailable for that session.
pub const CAP_VOICE_DATAGRAMS: u64 = 1 << 0;

/// Delivery class of a frame. See docs/protocol.md §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Lifecycle/control + request/reply (10–69): reliable, never sequenced.
    Control,
    /// Dispatch events (100+ except typing): sequenced, replayed on resume.
    Sequenced,
    /// Typing (103): seq=0, never replayed, loss-tolerant.
    Unsequenced,
}

impl v1::Frame {
    /// Classify by payload. `None` payloads (unknown to this build) classify by
    /// their `seq` field: seq>0 means a dispatch we must still ack past.
    pub fn class(&self) -> FrameClass {
        use v1::frame::Payload;
        match &self.payload {
            Some(Payload::TypingStart(_)) => FrameClass::Unsequenced,
            Some(p) => {
                if payload_field_number(p) >= 100 {
                    FrameClass::Sequenced
                } else {
                    FrameClass::Control
                }
            }
            None => {
                if self.seq > 0 {
                    FrameClass::Sequenced
                } else {
                    FrameClass::Control
                }
            }
        }
    }

    /// A dispatch frame with seq unset (the gateway assigns it per session).
    pub fn dispatch(payload: v1::frame::Payload) -> Self {
        Self {
            seq: 0,
            nonce: 0,
            payload: Some(payload),
        }
    }

    /// A control frame (no seq, no nonce).
    pub fn control(payload: v1::frame::Payload) -> Self {
        Self {
            seq: 0,
            nonce: 0,
            payload: Some(payload),
        }
    }

    /// A request/reply frame correlated by nonce.
    pub fn with_nonce(nonce: u64, payload: v1::frame::Payload) -> Self {
        Self {
            seq: 0,
            nonce,
            payload: Some(payload),
        }
    }
}

fn payload_field_number(p: &v1::frame::Payload) -> u32 {
    use v1::frame::Payload::*;
    match p {
        Hello(_) => 10,
        Identify(_) => 11,
        Resume(_) => 12,
        Ready(_) => 13,
        Resumed(_) => 14,
        Heartbeat(_) => 15,
        HeartbeatAck(_) => 16,
        Close(_) => 17,
        Error(_) => 18,
        SendMessage(_) => 30,
        StartTyping(_) => 31,
        UpdatePresence(_) => 32,
        EditMessage(_) => 33,
        DeleteMessage(_) => 34,
        AddReaction(_) => 35,
        RemoveReaction(_) => 36,
        SendMessageAck(_) => 50,
        MessageCreate(_) => 100,
        MessageUpdate(_) => 101,
        MessageDelete(_) => 102,
        TypingStart(_) => 103,
        PresenceUpdate(_) => 104,
        GuildCreate(_) => 105,
        GuildUpdate(_) => 106,
        GuildDelete(_) => 107,
        ChannelCreate(_) => 108,
        ChannelUpdate(_) => 109,
        ChannelDelete(_) => 110,
        MemberAdd(_) => 111,
        MemberRemove(_) => 112,
        DmChannelCreate(_) => 113,
        ReactionUpdate(_) => 114,
        UserUpdate(_) => 115,
        ReadMarkerUpdate(_) => 116,
        FriendUpdate(_) => 117,
        VoiceJoin(_) => 118,
        VoiceLeave(_) => 119,
        VoiceState(_) => 120,
    }
}

impl v1::ErrorCode {
    /// The matching WS/QUIC close code (4000 + code).
    pub fn close_code(self) -> u32 {
        CLOSE_CODE_BASE + self as u32
    }
}
