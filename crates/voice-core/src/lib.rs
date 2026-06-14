//! Pure-Rust voice plumbing for Dice.
//!
//! This crate holds the parts of the voice path that are *device-independent*
//! and therefore unit-testable without a microphone, speaker, or network:
//!
//! - [`framing`] — an RTP-inspired wire format that wraps an opaque Opus
//!   payload with the metadata the receiver needs (ssrc, sequence, timestamp,
//!   talkspurt marker). `voice-core` never encodes or decodes audio itself; the
//!   payload is bytes it carries verbatim. Frames travel over QUIC datagrams
//!   ([`framing::VoiceFrame::encode`] / [`framing::VoiceFrame::decode`]).
//! - [`jitter`] — a playout jitter buffer: it reorders out-of-order frames,
//!   drops duplicates and late arrivals, and reports gaps so the decoder can
//!   conceal them. Handles 16-bit sequence wraparound.
//! - [`plc`] — packet-loss-concealment bookkeeping: loss statistics (so the
//!   "graceful at 5 % loss" gate is measurable) and a limiter that caps how many
//!   consecutive frames are concealed before falling back to silence.
//!
//! No async, no IO, no codec — every type here is pure logic. The actual Opus
//! encode/decode, capture, and playback live in the client (the on-hardware
//! voice phase); the SFU forwarding lives in `voice-service` and the gateway.

pub mod framing;
pub mod jitter;
pub mod plc;

pub use framing::{HEADER_LEN, MAX_FRAME_BYTES, VoiceFrame, VoiceFrameError};
pub use jitter::{JitterBuffer, JitterConfig, Playout, PushOutcome};
pub use plc::{Conceal, ConcealmentLimiter, LossStats};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod integration_tests {
    //! End-to-end: drive a lossy, reordered frame stream through the jitter
    //! buffer + PLC + loss stats and assert the playout stays continuous and the
    //! measured loss tracks the injected loss — the device-free half of the
    //! "graceful degradation at 5 % packet loss" milestone gate.

    use super::*;
    use bytes::Bytes;

    fn frame(seq: u16) -> VoiceFrame {
        VoiceFrame {
            ssrc: 7,
            seq,
            timestamp: u32::from(seq).wrapping_mul(960), // 20 ms @ 48 kHz
            marker: seq == 0,
            payload: Bytes::from_static(b"opus"),
        }
    }

    /// Feed 1000 frames, drop a deterministic 5 % in flight, jitter the arrival
    /// order of neighbours, and play out on a steady clock: one playout slot per
    /// tick, frames pushed as they "arrive". Asserts the playout stays in order
    /// and continuous and the measured loss tracks the injected 5 %.
    #[test]
    fn lossy_reordered_stream_plays_out_continuously() {
        use std::collections::BTreeMap;

        let cfg = JitterConfig::default(); // 20 ms frames, 60 ms target
        let mut jb = JitterBuffer::new(cfg);
        let mut stats = LossStats::default();
        let mut limiter = ConcealmentLimiter::default();

        let total: u16 = 1000;
        let present = |s: u16| s < total && s % 20 != 7; // drop every 20th (5 %)

        // Arrival tick of each surviving frame = its own seq, then swap the
        // arrival ticks of each (even, even+1) pair so the buffer must reorder.
        let mut tick_of: BTreeMap<u16, u32> = (0..total)
            .filter(|&s| present(s))
            .map(|s| (s, u32::from(s)))
            .collect();
        for s in (0..total).step_by(2) {
            if present(s) && present(s + 1) {
                let (a, b) = (tick_of[&s], tick_of[&(s + 1)]);
                tick_of.insert(s, b);
                tick_of.insert(s + 1, a);
            }
        }
        let mut schedule: BTreeMap<u32, Vec<u16>> = BTreeMap::new();
        for (seq, tick) in tick_of {
            schedule.entry(tick).or_default().push(seq);
        }

        let mut played_real = 0u64;
        let mut concealed = 0u64;
        let mut silence = 0u64;
        let mut last_played: Option<u16> = None;

        // Run the clock until every arrival has been scheduled and the buffer
        // has fully drained; trailing ticks past the last frame are the clock
        // outrunning the audio, not starvation, so they don't count.
        let last_tick = *schedule.keys().next_back().expect("non-empty schedule");
        let mut t = 0u32;
        loop {
            if let Some(seqs) = schedule.get(&t) {
                for &s in seqs {
                    stats.on_push(jb.push(frame(s)));
                }
            }
            match jb.pop() {
                Some(p) => {
                    stats.on_pop(Some(&p));
                    match p {
                        Playout::Frame(f) => {
                            played_real += 1;
                            if let Some(prev) = last_played {
                                assert!(f.seq > prev, "out of order: {} after {prev}", f.seq);
                            }
                            last_played = Some(f.seq);
                            limiter.on_frame();
                        }
                        Playout::Conceal => match limiter.on_gap() {
                            Conceal::Frame => concealed += 1,
                            Conceal::Silence => silence += 1,
                        },
                    }
                }
                None => {
                    if t > last_tick && jb.is_empty() {
                        break; // stream finished and drained
                    }
                    if jb.started() {
                        stats.on_pop(None); // a genuine mid-stream underrun
                    }
                }
            }
            t += 1;
        }

        // Each of the 50 dropped frames surfaced as exactly one concealed slot;
        // no run was long enough to trip the silence fallback at isolated 5 %
        // loss; and the cushion never underran.
        assert_eq!(silence, 0, "isolated single-frame losses must not silence");
        assert_eq!(stats.underrun, 0, "target cushion should absorb 5 % loss");
        assert_eq!(concealed, 50, "concealed={concealed}");
        assert_eq!(played_real, 950, "played_real={played_real}");
        // Measured loss tracks the injected 5 %.
        let rate = stats.loss_rate();
        assert!((0.045..=0.055).contains(&rate), "loss_rate={rate}");
    }
}
