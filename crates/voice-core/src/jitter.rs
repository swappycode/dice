//! A playout jitter buffer.
//!
//! Networks deliver voice frames out of order, duplicated, late, or not at all.
//! The jitter buffer absorbs that: callers [`push`](JitterBuffer::push) frames
//! as they arrive off the wire and [`pop`](JitterBuffer::pop) them on a steady
//! playout clock. Pop yields frames in sequence order; when a frame is missing
//! but later ones have arrived, it yields [`Playout::Conceal`] so the decoder
//! runs packet-loss concealment for that slot instead of glitching.
//!
//! Sequence numbers are 16-bit and wrap; the buffer tracks an internal
//! *extended* (un-wrapped) sequence so ordering stays correct across the wrap
//! boundary. The buffer is pure logic — it never sleeps or reads a clock; the
//! caller's playout cadence is the clock.

use std::collections::BTreeMap;

use crate::framing::VoiceFrame;

/// What a [`pop`](JitterBuffer::pop) yields for one playout slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    /// A real, in-order frame is ready to decode and play.
    Frame(VoiceFrame),
    /// The expected frame is missing but later frames have arrived — the
    /// decoder should synthesize one frame of concealment and advance.
    Conceal,
}

/// What happened to a frame handed to [`push`](JitterBuffer::push).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Buffered for playout.
    Queued,
    /// A frame with this sequence is already buffered — ignored.
    Duplicate,
    /// Older than the playout head — too late to play — ignored.
    Late,
}

/// Jitter buffer tuning. All depths are expressed in milliseconds and converted
/// to whole frames against [`frame_ms`](JitterConfig::frame_ms).
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// Frame duration in milliseconds (Opus default is 20 ms).
    pub frame_ms: u32,
    /// How much audio to buffer before playout begins. Larger absorbs more
    /// jitter at the cost of latency.
    pub target_ms: u32,
    /// Hard cap on buffered audio; the oldest frame is dropped past this so a
    /// burst can't grow the buffer without bound.
    pub max_ms: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            frame_ms: 20,
            target_ms: 60,
            max_ms: 400,
        }
    }
}

impl JitterConfig {
    fn target_frames(&self) -> u32 {
        (self.target_ms / self.frame_ms.max(1)).max(1)
    }

    fn max_frames(&self) -> usize {
        ((self.max_ms / self.frame_ms.max(1)).max(self.target_frames())) as usize
    }
}

/// Extended sequence numbers are anchored well away from 0 and `u64::MAX` so the
/// ±32 767 reorder window can never under- or over-flow in practice.
const SEQ_ANCHOR: u64 = 1 << 32;

/// Map a wrapped 16-bit sequence to an extended 64-bit one, assuming it lies
/// within ±32 767 of the highest sequence seen so far (standard RTP unwrap).
fn extend(highest: Option<u64>, seq: u16) -> u64 {
    match highest {
        None => SEQ_ANCHOR | u64::from(seq),
        Some(h) => {
            let h_low = (h & 0xFFFF) as u16;
            let delta = i64::from(seq.wrapping_sub(h_low) as i16);
            (h as i64 + delta) as u64
        }
    }
}

/// A reordering playout buffer for one audio stream (one ssrc).
#[derive(Debug)]
pub struct JitterBuffer {
    cfg: JitterConfig,
    target_frames: u32,
    max_frames: usize,
    /// Buffered frames keyed by extended sequence (so ordering survives wrap).
    buf: BTreeMap<u64, VoiceFrame>,
    /// Highest extended sequence seen, for unwrapping subsequent arrivals.
    highest: Option<u64>,
    /// Extended sequence of the last slot emitted by `pop`.
    playhead: Option<u64>,
    /// Playout begins only once the buffer first reaches `target_frames`.
    started: bool,
}

impl JitterBuffer {
    /// Create a buffer with the given tuning.
    #[must_use]
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            target_frames: cfg.target_frames(),
            max_frames: cfg.max_frames(),
            cfg,
            buf: BTreeMap::new(),
            highest: None,
            playhead: None,
            started: false,
        }
    }

    /// Insert a freshly-arrived frame.
    pub fn push(&mut self, frame: VoiceFrame) -> PushOutcome {
        let ext = extend(self.highest, frame.seq);
        // Already played past this slot → too late to matter.
        if let Some(ph) = self.playhead
            && ext <= ph
        {
            return PushOutcome::Late;
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        if self.buf.contains_key(&ext) {
            return PushOutcome::Duplicate;
        }
        self.buf.insert(ext, frame);
        // Enforce the depth cap by evicting the oldest frame(s).
        while self.buf.len() > self.max_frames {
            if let Some(&oldest) = self.buf.keys().next() {
                self.buf.remove(&oldest);
            } else {
                break;
            }
        }
        PushOutcome::Queued
    }

    /// Take the next playout slot, or `None` if there is nothing to play yet
    /// (buffer still filling to target, or a true underrun with an empty
    /// buffer). Once started, each call advances exactly one frame slot.
    pub fn pop(&mut self) -> Option<Playout> {
        if !self.started {
            if (self.buf.len() as u32) < self.target_frames {
                return None;
            }
            self.started = true;
            // Start the playhead one slot before the earliest buffered frame.
            let first = *self.buf.keys().next()?;
            self.playhead = Some(first - 1);
        }
        let want = self.playhead? + 1;
        if let Some(frame) = self.buf.remove(&want) {
            self.playhead = Some(want);
            return Some(Playout::Frame(frame));
        }
        if self.buf.is_empty() {
            // Underrun: nothing buffered. Hold the playhead so a delayed frame
            // for `want` can still arrive and play in order.
            return None;
        }
        // Later frames exist, so `want` is genuinely lost — conceal and advance.
        self.playhead = Some(want);
        Some(Playout::Conceal)
    }

    /// Number of frames currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Buffered audio depth in milliseconds.
    #[must_use]
    pub fn buffered_ms(&self) -> u32 {
        self.buf.len() as u32 * self.cfg.frame_ms
    }

    /// Whether playout has begun (the buffer reached target depth at least
    /// once).
    #[must_use]
    pub fn started(&self) -> bool {
        self.started
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(seq: u16) -> VoiceFrame {
        VoiceFrame {
            ssrc: 1,
            seq,
            timestamp: u32::from(seq) * 960,
            marker: false,
            payload: Bytes::from_static(b"x"),
        }
    }

    /// target_ms=0 (or < frame_ms) still yields a 1-frame target.
    fn immediate() -> JitterBuffer {
        JitterBuffer::new(JitterConfig {
            frame_ms: 20,
            target_ms: 20,
            max_ms: 200,
        })
    }

    #[test]
    fn waits_for_target_depth_before_playout() {
        let mut jb = JitterBuffer::new(JitterConfig {
            frame_ms: 20,
            target_ms: 60, // 3 frames
            max_ms: 200,
        });
        jb.push(frame(0));
        jb.push(frame(1));
        assert!(jb.pop().is_none(), "should still be filling");
        jb.push(frame(2));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
    }

    #[test]
    fn in_order_playout() {
        let mut jb = immediate();
        for s in 0..5 {
            jb.push(frame(s));
        }
        for s in 0..5 {
            assert_eq!(jb.pop(), Some(Playout::Frame(frame(s))));
        }
        assert!(jb.pop().is_none());
    }

    #[test]
    fn reorders_out_of_order_arrivals() {
        let mut jb = immediate();
        jb.push(frame(0));
        jb.push(frame(2));
        jb.push(frame(1));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(1))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(2))));
    }

    #[test]
    fn conceals_a_gap_when_later_frames_exist() {
        let mut jb = immediate();
        jb.push(frame(0));
        jb.push(frame(2)); // 1 is lost
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
        assert_eq!(jb.pop(), Some(Playout::Conceal));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(2))));
    }

    #[test]
    fn underrun_returns_none_and_holds_for_a_late_frame() {
        let mut jb = immediate();
        jb.push(frame(0));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
        // 1 hasn't arrived and nothing later has either → hold, don't conceal.
        assert!(jb.pop().is_none());
        // 1 finally arrives late → it still plays in order.
        assert_eq!(jb.push(frame(1)), PushOutcome::Queued);
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(1))));
    }

    #[test]
    fn drops_duplicates() {
        let mut jb = immediate();
        assert_eq!(jb.push(frame(0)), PushOutcome::Queued);
        assert_eq!(jb.push(frame(0)), PushOutcome::Duplicate);
    }

    #[test]
    fn drops_late_arrivals_after_playhead() {
        let mut jb = immediate();
        jb.push(frame(0));
        jb.push(frame(1));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(1))));
        // 0 arrives again now that the playhead is past it.
        assert_eq!(jb.push(frame(0)), PushOutcome::Late);
    }

    #[test]
    fn enforces_max_depth_by_dropping_oldest() {
        // max_ms=60 @ 20ms = 3 frames.
        let mut jb = JitterBuffer::new(JitterConfig {
            frame_ms: 20,
            target_ms: 20,
            max_ms: 60,
        });
        for s in 0..5 {
            jb.push(frame(s));
        }
        assert_eq!(jb.len(), 3);
        // Oldest two (0,1) were evicted; playout resumes at 2.
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(2))));
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut jb = immediate();
        jb.push(frame(u16::MAX - 1));
        jb.push(frame(u16::MAX));
        jb.push(frame(0)); // wrapped
        jb.push(frame(1));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(u16::MAX - 1))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(u16::MAX))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(1))));
    }

    #[test]
    fn reordering_across_the_wrap_boundary() {
        let mut jb = immediate();
        // 0 arrives before the pre-wrap frame it should follow.
        jb.push(frame(u16::MAX));
        jb.push(frame(0));
        jb.push(frame(u16::MAX - 1));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(u16::MAX - 1))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(u16::MAX))));
        assert_eq!(jb.pop(), Some(Playout::Frame(frame(0))));
    }

    #[test]
    fn buffered_ms_and_started_reporting() {
        let mut jb = JitterBuffer::new(JitterConfig {
            frame_ms: 20,
            target_ms: 40,
            max_ms: 200,
        });
        assert!(!jb.started());
        jb.push(frame(0));
        assert_eq!(jb.buffered_ms(), 20);
        jb.push(frame(1));
        assert!(jb.pop().is_some());
        assert!(jb.started());
    }
}
