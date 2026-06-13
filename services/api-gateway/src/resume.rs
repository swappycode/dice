//! Sequencing + replay (docs/protocol.md §5): the per-session replay ring
//! buffer and the detached-session registry that brokers `Resume`.
//!
//! Registry shape: the spec sketches `DashMap<SessionId, Detached{...}>`
//! holding the buffer by value, plus a global reaper. Here the buffer stays
//! inside the (still-running) session task through the detached window — it
//! must keep draining the outbound queue into the ring while detached — so
//! the registry maps `gateway_session_id` → an offer channel into that task,
//! and each task expires itself after `resume_window_ms` (functionally the
//! reaper, but per-session and race-free). See `session::detached_wait`.

use std::collections::VecDeque;

use bytes::Bytes;
use dashmap::DashMap;
use dice_network_core::server::FramedTransport;
use dice_protocol::v1::Frame;
use prost::Message as _;
use tokio::sync::{mpsc, oneshot};

/// Replay ring bounds (protocol §5): 256 frames OR 256 KiB, whichever first.
pub(crate) const MAX_BUFFERED_FRAMES: usize = 256;
pub(crate) const MAX_BUFFERED_BYTES: usize = 256 * 1024;

/// `Ready.resume_token` length (protocol §3).
pub(crate) const RESUME_TOKEN_LEN: usize = 32;

struct Buffered {
    seq: u64,
    bytes: usize,
    frame: Frame,
}

/// Bounded ring of sequenced (class A) dispatch frames, with their assigned
/// per-session seq. Drop-from-front on overflow; a resume that needs dropped
/// frames fails (`covers` returns false).
pub(crate) struct ReplayBuffer {
    frames: VecDeque<Buffered>,
    total_bytes: usize,
    /// Highest seq no longer in the buffer (acked by the client or evicted).
    /// A resume from `last_seq < trimmed_to` cannot be healed.
    trimmed_to: u64,
}

impl ReplayBuffer {
    pub(crate) fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            total_bytes: 0,
            trimmed_to: 0,
        }
    }

    /// Insert a sequenced frame (seq already assigned, strictly increasing).
    pub(crate) fn push(&mut self, frame: Frame) {
        let bytes = frame.encoded_len();
        self.total_bytes += bytes;
        self.frames.push_back(Buffered {
            seq: frame.seq,
            bytes,
            frame,
        });
        // Enforce both bounds, but always keep the newest frame.
        while self.frames.len() > 1
            && (self.frames.len() > MAX_BUFFERED_FRAMES || self.total_bytes > MAX_BUFFERED_BYTES)
        {
            self.evict_front();
        }
    }

    fn evict_front(&mut self) {
        if let Some(dropped) = self.frames.pop_front() {
            self.total_bytes -= dropped.bytes;
            self.trimmed_to = self.trimmed_to.max(dropped.seq);
        }
    }

    /// Cumulative ack (Heartbeat.last_seq / Resume.last_seq): drop everything
    /// the client already has.
    pub(crate) fn ack(&mut self, last_seq: u64) {
        while self
            .frames
            .front()
            .is_some_and(|front| front.seq <= last_seq)
        {
            self.evict_front();
        }
        self.trimmed_to = self.trimmed_to.max(last_seq);
    }

    /// Can a client that has everything up to `last_seq` be fully healed from
    /// this buffer?
    pub(crate) fn covers(&self, last_seq: u64) -> bool {
        last_seq >= self.trimmed_to
    }

    /// Buffered frames in seq order (all have seq > the last acked).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().map(|b| &b.frame)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.frames.len()
    }
}

/// A new connection offering its transport to a detached session.
pub(crate) struct ResumeOffer {
    /// Client-presented `Resume.resume_token` (compared in constant time).
    pub(crate) token: Bytes,
    /// Client-presented cumulative ack.
    pub(crate) last_seq: u64,
    /// The new connection's transport; ownership moves to the detached
    /// session task on success.
    pub(crate) transport: Box<dyn FramedTransport>,
    pub(crate) reply: oneshot::Sender<ResumeReply>,
}

pub(crate) enum ResumeReply {
    /// Transport taken; the offering connection task is done.
    Accepted,
    /// Validation failed; transport handed back so the connection can send
    /// `Error{INVALID_SESSION}` and stay open for a fresh Identify.
    Rejected(Box<dyn FramedTransport>),
}

/// `gateway_session_id` → offer channel into the detached session task.
pub(crate) struct ResumeRegistry {
    map: DashMap<u64, mpsc::Sender<ResumeOffer>>,
}

impl ResumeRegistry {
    pub(crate) fn new() -> Self {
        Self {
            map: DashMap::new(),
        }
    }

    pub(crate) fn insert(&self, session_id: u64, tx: mpsc::Sender<ResumeOffer>) {
        self.map.insert(session_id, tx);
    }

    pub(crate) fn remove(&self, session_id: u64) {
        self.map.remove(&session_id);
    }

    /// Offer `transport` to the detached session, if any. Returns the
    /// transport back when the resume was rejected or the session is unknown
    /// (caller sends `Error{INVALID_SESSION}` and keeps the connection open);
    /// `None` when ownership transferred.
    pub(crate) async fn offer(
        &self,
        resume: dice_protocol::v1::Resume,
        transport: Box<dyn FramedTransport>,
    ) -> Option<Box<dyn FramedTransport>> {
        let Some(tx) = self
            .map
            .get(&resume.gateway_session_id)
            .map(|entry| entry.value().clone())
        else {
            return Some(transport);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let offer = ResumeOffer {
            token: resume.resume_token,
            last_seq: resume.last_seq,
            transport,
            reply: reply_tx,
        };
        match tx.send(offer).await {
            Ok(()) => match reply_rx.await {
                Ok(ResumeReply::Accepted) => None,
                Ok(ResumeReply::Rejected(transport)) => Some(transport),
                // Detached task died mid-offer (shutdown race); the transport
                // went with it — the client reconnects and re-identifies.
                Err(_) => None,
            },
            // Entry expired between lookup and send: token-invalid path.
            Err(mpsc::error::SendError(offer)) => Some(offer.transport),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use dice_protocol::v1::{self, frame::Payload};

    fn msg_frame(seq: u64, content: &str) -> Frame {
        Frame {
            seq,
            nonce: 0,
            payload: Some(Payload::MessageCreate(v1::MessageCreate {
                message: Some(v1::Message {
                    id: seq,
                    channel_id: 1,
                    author_id: 2,
                    content: content.to_owned(),
                    edited_at_ms: 0,
                    reply_to_id: 0,
                    reactions: Vec::new(),
                    attachments: Vec::new(),
                }),
                nonce: 0,
            })),
        }
    }

    #[test]
    fn push_ack_replay_round_trip() {
        let mut buf = ReplayBuffer::new();
        for seq in 1..=10 {
            buf.push(msg_frame(seq, "hi"));
        }
        assert!(buf.covers(0), "nothing trimmed yet");
        buf.ack(4);
        assert!(buf.covers(4));
        assert!(!buf.covers(3), "acked frames are gone");
        let seqs: Vec<u64> = buf.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn frame_count_bound_evicts_from_front() {
        let mut buf = ReplayBuffer::new();
        for seq in 1..=(MAX_BUFFERED_FRAMES as u64 + 20) {
            buf.push(msg_frame(seq, "x"));
        }
        assert_eq!(buf.len(), MAX_BUFFERED_FRAMES);
        assert!(!buf.covers(19), "evicted seqs cannot be resumed past");
        assert!(buf.covers(20));
        assert_eq!(buf.iter().next().unwrap().seq, 21);
    }

    #[test]
    fn byte_bound_evicts_from_front() {
        let mut buf = ReplayBuffer::new();
        let big = "y".repeat(100 * 1024); // ~100 KiB per frame
        for seq in 1..=4 {
            buf.push(msg_frame(seq, &big));
        }
        assert!(buf.len() < 4, "256 KiB cap must have evicted");
        assert!(buf.total_bytes <= MAX_BUFFERED_BYTES);
        assert!(!buf.covers(0));
    }

    #[test]
    fn oversized_single_frame_is_kept_alone() {
        let mut buf = ReplayBuffer::new();
        buf.push(msg_frame(1, &"z".repeat(MAX_BUFFERED_BYTES)));
        assert_eq!(buf.len(), 1, "newest frame always survives");
        buf.push(msg_frame(2, "small"));
        assert_eq!(buf.iter().next().unwrap().seq, 2);
    }

    #[test]
    fn ack_beyond_buffer_trims_everything() {
        let mut buf = ReplayBuffer::new();
        buf.push(msg_frame(1, "a"));
        buf.push(msg_frame(2, "b"));
        buf.ack(99);
        assert_eq!(buf.iter().count(), 0);
        assert!(buf.covers(99));
        assert!(!buf.covers(98));
    }
}
