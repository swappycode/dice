//! Packet-loss-concealment bookkeeping.
//!
//! The [`jitter`](crate::jitter) buffer decides *when* a frame is missing; this
//! module decides *what to do about it over time* and *measures it*:
//!
//! - [`LossStats`] accumulates per-stream counters (received, duplicate, late,
//!   played, concealed, underrun) and a [`loss_rate`](LossStats::loss_rate) — the
//!   device-free, measurable half of the "graceful degradation at 5 % packet
//!   loss" milestone gate.
//! - [`ConcealmentLimiter`] caps how many *consecutive* frames are concealed
//!   before giving up and emitting silence: concealing one or two lost frames is
//!   imperceptible, but synthesizing a long run produces robotic artifacts, so
//!   past a threshold it's better to go quiet until real audio resumes.

use crate::jitter::{Playout, PushOutcome};

/// What to emit for a concealed slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conceal {
    /// Synthesize one frame of concealment audio (short gap — imperceptible).
    Frame,
    /// The loss run is too long — emit silence until real audio resumes.
    Silence,
}

/// Running per-stream loss/quality counters.
///
/// Feed it [`on_push`](LossStats::on_push) for every [`PushOutcome`] and
/// [`on_pop`](LossStats::on_pop) for every playout slot, and it tracks the
/// stream's health.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LossStats {
    /// Distinct frames accepted into the buffer.
    pub received: u64,
    /// Frames discarded as duplicates.
    pub duplicate: u64,
    /// Frames discarded for arriving after the playhead passed them.
    pub late: u64,
    /// Real frames handed to the decoder.
    pub played: u64,
    /// Slots filled by concealment because the frame was missing.
    pub concealed: u64,
    /// Playout slots where nothing was available at all (buffer empty).
    pub underrun: u64,
}

impl LossStats {
    /// Record the result of a [`JitterBuffer::push`](crate::jitter::JitterBuffer::push).
    pub fn on_push(&mut self, outcome: PushOutcome) {
        match outcome {
            PushOutcome::Queued => self.received += 1,
            PushOutcome::Duplicate => self.duplicate += 1,
            PushOutcome::Late => self.late += 1,
        }
    }

    /// Record the result of a [`JitterBuffer::pop`](crate::jitter::JitterBuffer::pop).
    /// Pass `None` for a slot the playout clock ticked through with nothing
    /// available (an underrun).
    pub fn on_pop(&mut self, playout: Option<&Playout>) {
        match playout {
            Some(Playout::Frame(_)) => self.played += 1,
            Some(Playout::Conceal) => self.concealed += 1,
            None => self.underrun += 1,
        }
    }

    /// Effective playout loss: the fraction of expected frames that had to be
    /// concealed. `0.0` when nothing has played yet.
    #[must_use]
    pub fn loss_rate(&self) -> f64 {
        let expected = self.played + self.concealed;
        if expected == 0 {
            0.0
        } else {
            self.concealed as f64 / expected as f64
        }
    }
}

/// Caps consecutive concealment so long outages degrade to silence rather than
/// robotic artifacts.
#[derive(Debug, Clone, Copy)]
pub struct ConcealmentLimiter {
    max_consecutive: u32,
    consecutive: u32,
}

impl Default for ConcealmentLimiter {
    /// Conceal up to 5 frames (~100 ms at 20 ms/frame) before falling silent.
    fn default() -> Self {
        Self::new(5)
    }
}

impl ConcealmentLimiter {
    /// Build a limiter that conceals at most `max_consecutive` frames in a row.
    #[must_use]
    pub fn new(max_consecutive: u32) -> Self {
        Self {
            max_consecutive,
            consecutive: 0,
        }
    }

    /// Call when a real frame plays — resets the run.
    pub fn on_frame(&mut self) {
        self.consecutive = 0;
    }

    /// Call for a concealed slot; returns whether to conceal or fall to silence.
    pub fn on_gap(&mut self) -> Conceal {
        self.consecutive += 1;
        if self.consecutive <= self.max_consecutive {
            Conceal::Frame
        } else {
            Conceal::Silence
        }
    }

    /// Current run of consecutive concealed slots.
    #[must_use]
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::framing::VoiceFrame;
    use bytes::Bytes;

    fn played() -> Playout {
        Playout::Frame(VoiceFrame {
            ssrc: 1,
            seq: 0,
            timestamp: 0,
            marker: false,
            payload: Bytes::new(),
        })
    }

    #[test]
    fn counts_push_outcomes() {
        let mut s = LossStats::default();
        s.on_push(PushOutcome::Queued);
        s.on_push(PushOutcome::Queued);
        s.on_push(PushOutcome::Duplicate);
        s.on_push(PushOutcome::Late);
        assert_eq!(s.received, 2);
        assert_eq!(s.duplicate, 1);
        assert_eq!(s.late, 1);
    }

    #[test]
    fn loss_rate_is_concealed_over_expected() {
        let mut s = LossStats::default();
        for _ in 0..95 {
            s.on_pop(Some(&played()));
        }
        for _ in 0..5 {
            s.on_pop(Some(&Playout::Conceal));
        }
        assert!((s.loss_rate() - 0.05).abs() < 1e-9);
    }

    #[test]
    fn loss_rate_zero_before_any_playout() {
        let s = LossStats::default();
        assert_eq!(s.loss_rate(), 0.0);
        // Underruns alone don't count toward expected frames.
        let mut s2 = LossStats::default();
        s2.on_pop(None);
        assert_eq!(s2.loss_rate(), 0.0);
        assert_eq!(s2.underrun, 1);
    }

    #[test]
    fn limiter_conceals_up_to_threshold_then_silences() {
        let mut l = ConcealmentLimiter::new(3);
        assert_eq!(l.on_gap(), Conceal::Frame);
        assert_eq!(l.on_gap(), Conceal::Frame);
        assert_eq!(l.on_gap(), Conceal::Frame);
        assert_eq!(l.on_gap(), Conceal::Silence);
        assert_eq!(l.on_gap(), Conceal::Silence);
        assert_eq!(l.consecutive(), 5);
    }

    #[test]
    fn limiter_resets_on_a_real_frame() {
        let mut l = ConcealmentLimiter::new(2);
        assert_eq!(l.on_gap(), Conceal::Frame);
        assert_eq!(l.on_gap(), Conceal::Frame);
        assert_eq!(l.on_gap(), Conceal::Silence);
        l.on_frame();
        assert_eq!(l.consecutive(), 0);
        assert_eq!(l.on_gap(), Conceal::Frame); // run restarts
    }
}
