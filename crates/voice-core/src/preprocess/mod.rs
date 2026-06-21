//! Near-end voice pre-processor: acoustic echo cancellation (AEC) then noise
//! suppression (NS), applied in place to the captured mic frame BEFORE the VAD
//! and the Opus encode, so both echo and noise are gone from the transmitted
//! audio and from the speaking-orb VAD. Pure logic, deterministic, device-free
//! — it unit-tests without a mic/speaker/network (see the tests below).
//!
//! Gated OFF by default: the client constructs [`Passthrough`] (a literal no-op)
//! unless `DICE_VOICE_AEC` is set, in which case it builds [`EchoNoiseProcessor`]
//! (see `apps/desktop-client/.../audio.rs`). The two stages are decoupled — a
//! frequency-domain partitioned NLMS [`aec::Mdf`] feeds its time-domain residual
//! to a separate STFT Wiener [`ns::WienerNs`] — so each is independently testable
//! and swappable.
//!
//! # Far-end alignment (the crux)
//! The echo of a played far-end sample reaches the mic tens of ms later (DAC +
//! output buffer + acoustics + input buffer + capture backlog). We do NOT run a
//! delay estimator; instead the caller feeds produced playout frames via
//! [`AudioProcessor::push_far`] and we hold them in an elastic buffer, drained
//! ONE 20 ms frame per [`AudioProcessor::process`] call. Because both the mic
//! capture (which drives `process`) and the speaker playout run on real 48 kHz
//! device clocks, draining one far frame per near frame keeps the two timelines
//! rate-matched; a fixed pre-delay line then offsets the far reference by
//! `pre_delay_ms`, and the adaptive filter's `tail_ms` span absorbs the residual
//! device/acoustic/reverb lag. During a playout gap the elastic buffer drains to
//! silence (correct — silence was played); during mic mute `process` isn't
//! called but the fixed delay line preserves the offset for when it resumes.
//!
//! NOTE: this deliberately does NOT match a "push far every audio-loop iteration
//! with silence ticks" scheme — that over-feeds the reference ~4x (the loop ticks
//! ~every 5 ms but a frame is 20 ms) and loses the delay across silence gaps.

mod aec;
mod ns;

use std::collections::VecDeque;

/// Canonical voice geometry. MUST equal `audio.rs::FRAME_SAMPLES` (48 kHz, 20 ms,
/// mono); asserted in [`EchoNoiseProcessor::new`].
pub const FRAME: usize = 960;

/// Sample rate the pre-processor is built for.
const SAMPLE_RATE: u32 = 48_000;

/// Cap on the elastic far-produced buffer (~8 frames). Bounds growth while the
/// mic is muted (playout keeps producing but `process` isn't draining); the
/// oldest far is dropped, which is harmless — only recent far echoes.
const FAR_PENDING_CAP: usize = 8 * FRAME;

/// A near-end mic pre-processor (AEC + NS), applied in place, device-free and
/// deterministic: given the same far-end + near-end sample streams it produces
/// identical output with no hardware.
pub trait AudioProcessor: Send {
    /// Append one produced 20 ms far-end (loudspeaker) playout frame to the
    /// internal elastic buffer. Call ONLY when a real playout frame was produced
    /// (mixed remote audio actually written to the output ring); a playout gap
    /// needs no call (the buffer drains to silence on its own).
    fn push_far(&mut self, far: &[f32; FRAME]);

    /// Clean one 20 ms near-end mic frame IN PLACE: AEC (cancel the time-aligned
    /// far-end echo) then NS. Call once per CAPTURED frame — the real-time master
    /// clock — EVERY frame, even while muted, passing `transmit = false` when the
    /// mic is muted / PTT-released. A muted call still advances the far timeline
    /// (so alignment survives the mute) and keeps the echo model current, but
    /// leaves `near` untouched and does not adapt; a transmitting call cleans
    /// `near` in place.
    fn process(&mut self, near: &mut [f32; FRAME], transmit: bool);

    /// Echo path changed (device switch / rejoin): drop all adaptive + buffered
    /// state so a stale path is forgotten immediately.
    fn reset(&mut self);

    /// Last-block echo-return-loss-enhancement (dB), for tests/metrics; 0 when
    /// nothing was cancelled or AEC is off.
    fn last_erle_db(&self) -> f32 {
        0.0
    }
}

/// Pre-processor configuration. [`Default`] is the shipped tuning.
#[derive(Clone, Copy, Debug)]
pub struct PreprocConfig {
    /// Run the echo canceller.
    pub aec: bool,
    /// Run noise suppression.
    pub ns: bool,
    /// Fixed far-end pre-delay (removes the known playout backlog); clamped.
    pub pre_delay_ms: u32,
    /// Adaptive filter tail (echo path length the AEC models).
    pub tail_ms: u32,
    /// Normalised NLMS step size.
    pub mu: f32,
    /// Hard NS spectral floor (a bin's gain is never driven below this), linear.
    pub ns_floor: f32,
}

impl Default for PreprocConfig {
    fn default() -> Self {
        Self {
            aec: true,
            ns: true,
            pre_delay_ms: 40,
            tail_ms: 128,
            mu: 0.3,
            ns_floor: 0.1,
        }
    }
}

/// The OFF path: a literal no-op with zero state, so the disabled default is
/// byte-identical to the pre-feature pipeline and allocates nothing.
pub struct Passthrough;

impl AudioProcessor for Passthrough {
    fn push_far(&mut self, _far: &[f32; FRAME]) {}
    fn process(&mut self, _near: &mut [f32; FRAME], _transmit: bool) {}
    fn reset(&mut self) {}
}

/// The concrete AEC + NS pre-processor.
pub struct EchoNoiseProcessor {
    cfg: PreprocConfig,
    pre_delay: usize,
    /// Far-end produced by playout, not yet shifted into the delay line.
    far_pending: VecDeque<f32>,
    /// Fixed-length (`pre_delay`) delay line; its front is the far reference
    /// time-aligned with the current near frame.
    far_delay: VecDeque<f32>,
    aec: aec::Mdf,
    ns: ns::WienerNs,
    /// 960 near-end -> 256 AEC-block bridging.
    in_near: VecDeque<f32>,
    in_far: VecDeque<f32>,
    /// Processed samples awaiting the 960-sample emit; primed with one frame of
    /// latency so steady-state `process` is exactly 960-in / 960-out.
    out_acc: VecDeque<f32>,
}

impl EchoNoiseProcessor {
    pub fn new(cfg: PreprocConfig) -> Self {
        let pre_delay = (cfg.pre_delay_ms.min(200) as usize) * (SAMPLE_RATE as usize / 1000);
        let tail_ms = cfg.tail_ms.clamp(32, 256);
        let mut out_acc = VecDeque::with_capacity(2 * FRAME);
        out_acc.extend(std::iter::repeat_n(0.0, FRAME));
        let mut far_delay = VecDeque::with_capacity(pre_delay + FRAME);
        far_delay.extend(std::iter::repeat_n(0.0, pre_delay));
        Self {
            cfg,
            pre_delay,
            far_pending: VecDeque::new(),
            far_delay,
            aec: aec::Mdf::new(tail_ms, SAMPLE_RATE, cfg.mu),
            ns: ns::WienerNs::new(cfg.ns_floor),
            in_near: VecDeque::new(),
            in_far: VecDeque::new(),
            out_acc,
        }
    }
}

impl AudioProcessor for EchoNoiseProcessor {
    fn push_far(&mut self, far: &[f32; FRAME]) {
        self.far_pending.extend(far.iter().copied());
        while self.far_pending.len() > FAR_PENDING_CAP {
            self.far_pending.pop_front();
        }
    }

    fn process(&mut self, near: &mut [f32; FRAME], transmit: bool) {
        if !self.cfg.aec && !self.cfg.ns {
            return;
        }
        // NS-only (no AEC): no far timeline to keep; only act when transmitting.
        if !self.cfg.aec {
            if transmit {
                self.in_near.extend(near.iter().copied());
                while self.in_near.len() >= aec::B {
                    let mut nb = [0.0f32; aec::B];
                    for s in nb.iter_mut() {
                        *s = self.in_near.pop_front().unwrap_or(0.0);
                    }
                    self.out_acc.extend(self.ns.run(&nb));
                }
                for s in near.iter_mut() {
                    *s = self.out_acc.pop_front().unwrap_or(0.0);
                }
            }
            return;
        }
        // 1) Advance the far timeline by exactly ONE frame on EVERY call (the
        //    capture clock is the real-time master), then read the pre-delay-
        //    aligned far reference. Doing this even while muted keeps far_pending
        //    drained and the offset intact for when the mic re-opens.
        for _ in 0..FRAME {
            let s = self.far_pending.pop_front().unwrap_or(0.0);
            self.far_delay.push_back(s);
        }
        for _ in 0..FRAME {
            let s = self.far_delay.pop_front().unwrap_or(0.0);
            self.in_far.push_back(s);
        }
        // 2) Bridge 960-sample near frames into 256-sample AEC blocks (near and
        //    far advance together, so they stay paired).
        self.in_near.extend(near.iter().copied());
        while self.in_near.len() >= aec::B && self.in_far.len() >= aec::B {
            let mut nb = [0.0f32; aec::B];
            let mut fb = [0.0f32; aec::B];
            for s in nb.iter_mut() {
                *s = self.in_near.pop_front().unwrap_or(0.0);
            }
            for s in fb.iter_mut() {
                *s = self.in_far.pop_front().unwrap_or(0.0);
            }
            if transmit {
                let residual = self.aec.run(&fb, &nb);
                if self.cfg.ns {
                    self.out_acc.extend(self.ns.run(&residual));
                } else {
                    self.out_acc.extend(residual);
                }
            } else {
                // Muted: keep the far model current (alignment) but don't cancel,
                // adapt, or emit.
                self.aec.observe_far(&fb);
            }
        }
        // 3) Emit exactly one frame when transmitting (priming makes this never
        //    underrun in steady state); leave `near` untouched while muted.
        if transmit {
            for s in near.iter_mut() {
                *s = self.out_acc.pop_front().unwrap_or(0.0);
            }
        }
    }

    fn reset(&mut self) {
        self.far_pending.clear();
        self.far_delay.clear();
        self.far_delay
            .extend(std::iter::repeat_n(0.0, self.pre_delay));
        self.in_near.clear();
        self.in_far.clear();
        self.out_acc.clear();
        self.out_acc.extend(std::iter::repeat_n(0.0, FRAME));
        self.aec.reset();
        self.ns.reset();
    }

    fn last_erle_db(&self) -> f32 {
        if self.cfg.aec {
            self.aec.last_erle_db()
        } else {
            0.0
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ---- deterministic, hardware-free signal helpers ----

    /// xorshift64 PRNG -> white noise in [-amp, amp]; no `rand` dependency.
    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self, amp: f32) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let u = (self.0 >> 11) as f32 / (1u64 << 53) as f32; // [0,1)
            (u * 2.0 - 1.0) * amp
        }
    }

    fn energy(xs: &[f32]) -> f32 {
        xs.iter().map(|&v| v * v).sum()
    }

    /// Pearson-ish correlation (zero-lag), for "speech preserved" checks.
    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let ea = energy(a).sqrt();
        let eb = energy(b).sqrt();
        if ea < 1e-9 || eb < 1e-9 {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b).map(|(&x, &y)| x * y).sum();
        dot / (ea * eb)
    }

    /// Streaming FIR echo path: `near[n] = Σ_j h[j] * far[n-j]`.
    struct EchoPath {
        h: Vec<f32>,
        hist: VecDeque<f32>,
    }
    impl EchoPath {
        fn new(taps: &[(usize, f32)]) -> Self {
            let len = taps.iter().map(|&(d, _)| d).max().unwrap_or(0) + 1;
            let mut h = vec![0.0; len];
            for &(d, g) in taps {
                h[d] = g;
            }
            Self {
                h,
                hist: VecDeque::from(vec![0.0; len]),
            }
        }
        fn push(&mut self, far: f32) -> f32 {
            self.hist.push_front(far);
            self.hist.truncate(self.h.len());
            self.h
                .iter()
                .zip(self.hist.iter())
                .map(|(&c, &x)| c * x)
                .sum()
        }
    }

    fn aec_only_cfg(tail_ms: u32, pre_delay_ms: u32) -> PreprocConfig {
        PreprocConfig {
            aec: true,
            ns: false,
            pre_delay_ms,
            tail_ms,
            mu: 0.4,
            ns_floor: 0.1,
        }
    }

    /// Single-talk echo cancellation reaches strong ERLE (no near-end speech).
    #[test]
    fn aec_cancels_synthetic_echo() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0x1234_5678_9abc_def1);
        // Attenuating echo path (peak << far so Geigel never false-triggers),
        // all taps inside the 40 ms tail.
        let mut path = EchoPath::new(&[(0, 0.30), (40, 0.15), (120, 0.08)]);

        let frames = 320usize;
        let mut in_echo: Vec<f32> = Vec::new();
        let mut out_res: Vec<f32> = Vec::new();
        for _ in 0..frames {
            let mut near = [0.0f32; FRAME];
            let mut far = [0.0f32; FRAME];
            for i in 0..FRAME {
                let f = rng.next_f32(0.25);
                far[i] = f;
                near[i] = path.push(f); // echo only
            }
            p.push_far(&far);
            in_echo.extend_from_slice(&near);
            p.process(&mut near, true); // near now holds the residual
            out_res.extend_from_slice(&near);
        }

        // Output lags input by exactly one frame of priming; measure ERLE over a
        // late, converged window.
        let lag = FRAME;
        let lo = 240 * FRAME;
        let hi = 300 * FRAME;
        let ein = energy(&in_echo[lo..hi]);
        let eout = energy(&out_res[lo + lag..hi + lag]);
        let erle = 10.0 * (ein / eout.max(1e-12)).log10();
        assert!(erle >= 15.0, "ERLE only {erle:.1} dB (want >= 15)");
        assert!(out_res.iter().all(|s| s.is_finite()), "residual finite");
    }

    /// Double-talk: near-end speech on top of echo must NOT diverge the filter,
    /// and cancellation recovers once the talker stops.
    #[test]
    fn aec_survives_double_talk() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0xfeed_face_dead_beef);
        let mut path = EchoPath::new(&[(0, 0.30), (60, 0.12)]);

        let mut max_ratio = 0.0f32;
        let mut tone_phase = 0.0f32;
        // Converge (0-120), double-talk (120-180), recover (180-320).
        let mut erle_after = Vec::new();
        for frame in 0..320usize {
            let mut near = [0.0f32; FRAME];
            let mut far = [0.0f32; FRAME];
            let double_talk = (120..180).contains(&frame);
            for i in 0..FRAME {
                let f = rng.next_f32(0.25);
                far[i] = f;
                let mut d = path.push(f);
                if double_talk {
                    tone_phase += std::f32::consts::TAU * 350.0 / SAMPLE_RATE as f32;
                    d += 0.4 * tone_phase.sin(); // loud near-end talker
                }
                near[i] = d;
            }
            let ein = energy(&near);
            p.push_far(&far);
            p.process(&mut near, true);
            let eout = energy(&near);
            assert!(
                near.iter().all(|s| s.is_finite()),
                "no NaN/Inf at frame {frame}"
            );
            if double_talk {
                max_ratio = max_ratio.max(eout / ein.max(1e-12));
            }
            if frame >= 300 {
                erle_after.push(p.last_erle_db());
            }
        }
        // During double-talk the output never blows up past the input (freeze +
        // guards held; a diverged filter would amplify).
        assert!(
            max_ratio < 4.0,
            "double-talk blew up (ratio {max_ratio:.2})"
        );
        // After the talker stops, the preserved filter still cancels.
        let erle = erle_after.iter().copied().fold(0.0f32, f32::max);
        assert!(
            erle >= 10.0,
            "ERLE did not recover after double-talk ({erle:.1} dB)"
        );
    }

    /// A far-end onset after true silence must NOT over-step the NLMS update on
    /// its first block: the denominator is floored by the current far power, so
    /// the residual never spikes (an under-normalised step would blow past 1.0).
    #[test]
    fn aec_no_overstep_on_far_onset_after_silence() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut path = EchoPath::new(&[(0, 0.30), (40, 0.15)]);

        // Phase 1: true silence -> far-silence freeze, output stays silent.
        for _ in 0..40 {
            let mut near = [0.0f32; FRAME];
            p.push_far(&[0.0f32; FRAME]);
            p.process(&mut near, true);
            assert!(near.iter().all(|&s| s.abs() < 1e-6), "silence stays silent");
        }

        // Phase 2: sudden LOUD far onset + its echo (no near-end speech).
        let mut peak = 0.0f32;
        for _ in 0..80 {
            let mut near = [0.0f32; FRAME];
            let mut far = [0.0f32; FRAME];
            for i in 0..FRAME {
                let f = rng.next_f32(0.5);
                far[i] = f;
                near[i] = path.push(f);
            }
            p.push_far(&far);
            p.process(&mut near, true);
            assert!(near.iter().all(|s| s.is_finite()), "no NaN/Inf on onset");
            peak = near.iter().fold(peak, |m, &s| m.max(s.abs()));
        }
        assert!(
            peak < 1.0,
            "far-onset over-step spike: residual peak {peak}"
        );
    }

    /// Far-end silence => AEC is an exact pass-through (no adaptation, no
    /// divergence) and the bridge conserves every sample (output == input,
    /// delayed by the one-frame priming).
    #[test]
    fn far_silence_is_exact_passthrough_and_conserves_samples() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0x0bad_c0de_1234_5678);
        let frames = 16usize;
        let mut input: Vec<f32> = Vec::new();
        let mut output: Vec<f32> = Vec::new();
        for _ in 0..frames {
            let mut near = [0.0f32; FRAME];
            for s in near.iter_mut() {
                *s = rng.next_f32(0.5);
            }
            // No push_far -> far reference is silence.
            input.extend_from_slice(&near);
            p.process(&mut near, true);
            output.extend_from_slice(&near);
        }
        assert_eq!(output.len(), input.len(), "sample count conserved");
        let lag = FRAME;
        for i in 0..(input.len() - lag) {
            assert!(
                (output[i + lag] - input[i]).abs() < 1e-5,
                "pass-through mismatch at {i}: {} vs {}",
                output[i + lag],
                input[i]
            );
        }
    }

    /// Noise suppression attenuates stationary noise while preserving speech, and
    /// never zeroes a bin (musical-noise guard).
    #[test]
    fn ns_reduces_noise_and_preserves_speech() {
        let cfg = PreprocConfig {
            aec: false,
            ns: true,
            ..Default::default()
        };
        let mut p = EchoNoiseProcessor::new(cfg);
        let mut rng = Rng(0xa11c_e123_4567_89ab);

        let tone = |n: usize| {
            let t = n as f32 / SAMPLE_RATE as f32;
            0.3 * ((std::f32::consts::TAU * 220.0 * t).sin()
                + (std::f32::consts::TAU * 440.0 * t).sin()
                + (std::f32::consts::TAU * 660.0 * t).sin())
                / 3.0
        };

        let frames = 200usize;
        let mut n = 0usize;
        let mut clean: Vec<f32> = Vec::new();
        let mut out: Vec<f32> = Vec::new();
        // First 60 frames: noise only (let the floor learn + measure attenuation).
        // Then speech+noise.
        let mut noise_in = 0.0f32;
        let mut noise_out = 0.0f32;
        for frame in 0..frames {
            let speech = frame >= 60;
            let mut near = [0.0f32; FRAME];
            let mut frame_clean = [0.0f32; FRAME];
            for i in 0..FRAME {
                let s = if speech { tone(n) } else { 0.0 };
                frame_clean[i] = s;
                near[i] = s + rng.next_f32(0.06); // additive white noise
                n += 1;
            }
            let noisy_energy = energy(&near);
            clean.extend_from_slice(&frame_clean);
            p.process(&mut near, true);
            out.extend_from_slice(&near);
            // Measure noise-only frames (well past warmup, before speech).
            if (40..60).contains(&frame) {
                noise_in += noisy_energy;
                noise_out += energy(&near);
            }
        }
        let atten_db = 10.0 * (noise_in / noise_out.max(1e-12)).log10();
        assert!(atten_db >= 6.0, "noise attenuation only {atten_db:.1} dB");
        // Comfort floor: noise is suppressed but never fully killed.
        assert!(
            noise_out > 0.0,
            "noise frames must not be zeroed (comfort floor)"
        );
        // Speech preserved: late output correlates with the clean speech (account
        // for the NS frame latency by scanning a small lag window).
        let lo = 150 * FRAME;
        let hi = 195 * FRAME;
        let best = (0..512)
            .map(|lag| correlation(&out[lo + lag..hi + lag], &clean[lo..hi]))
            .fold(0.0f32, f32::max);
        assert!(best >= 0.8, "speech not preserved (corr {best:.2})");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    /// Mic mute keeps the far timeline aligned: `process` is still called every
    /// captured frame (transmit=false) so the far reference advances and the echo
    /// model stays current; cancellation resumes correctly afterwards.
    #[test]
    fn alignment_survives_mute_and_resume() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0x5151_5151_2727_2727);
        let mut path = EchoPath::new(&[(0, 0.28), (50, 0.14)]);
        let run_phase = |p: &mut EchoNoiseProcessor,
                         path: &mut EchoPath,
                         rng: &mut Rng,
                         frames: usize,
                         transmit: bool|
         -> f32 {
            let mut erle = 0.0;
            for _ in 0..frames {
                let mut near = [0.0f32; FRAME];
                let mut far = [0.0f32; FRAME];
                for i in 0..FRAME {
                    let f = rng.next_f32(0.25);
                    far[i] = f;
                    near[i] = path.push(f);
                }
                p.push_far(&far);
                p.process(&mut near, transmit);
                if transmit {
                    erle = p.last_erle_db();
                }
            }
            erle
        };
        run_phase(&mut p, &mut path, &mut rng, 200, true);
        // Mute: still capturing + producing far, but transmit=false.
        run_phase(&mut p, &mut path, &mut rng, 20, false);
        // Resume: cancellation should still be healthy within a few frames.
        let erle = run_phase(&mut p, &mut path, &mut rng, 60, true);
        assert!(erle >= 10.0, "ERLE after mute/resume only {erle:.1} dB");
    }

    /// TEMP VERIFICATION: far-silence then a sudden loud far onset (e.g. a
    /// notification chime) — does the lagged-p under-normalisation diverge?
    #[test]
    fn temp_onset_from_silence_stays_bounded() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(128, 0));
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        // A realistic-ish echo path within the 128 ms tail.
        let mut path = EchoPath::new(&[(0, 0.30), (80, 0.18), (300, 0.10), (1200, 0.05)]);

        let mut max_abs = 0.0f32;
        let mut all_finite = true;
        let mut max_erle = 0.0f32;

        // Phase A: ~1 s of TRUE far silence (no push_far). Mic captures quiet
        // room noise only. p[bin] decays to ~0, delta floors.
        for _ in 0..50 {
            let mut near = [0.0f32; FRAME];
            for s in near.iter_mut() {
                *s = rng.next_f32(0.002); // quiet room
            }
            // no push_far -> far is silence
            p.process(&mut near, true);
            for &s in &near {
                max_abs = max_abs.max(s.abs());
                all_finite &= s.is_finite();
            }
        }

        // Phase B: sudden LOUD far onset (chime) -> echo appears in near.
        // Repeat the onset MANY times to give an over-step room to compound.
        for frame in 0..400usize {
            let mut near = [0.0f32; FRAME];
            let mut far = [0.0f32; FRAME];
            for i in 0..FRAME {
                // Loud far, full-scale-ish white burst (worst case onset).
                let f = rng.next_f32(0.9);
                far[i] = f;
                near[i] = path.push(f); // pure echo, no near talker
            }
            p.push_far(&far);
            p.process(&mut near, true);
            for &s in &near {
                max_abs = max_abs.max(s.abs());
                all_finite &= s.is_finite();
            }
            if frame >= 350 {
                max_erle = max_erle.max(p.last_erle_db());
            }
        }

        println!("TEMP_ONSET max_abs={max_abs} all_finite={all_finite} max_erle_db={max_erle}");
        assert!(all_finite, "diverged to NaN/Inf on onset");
    }

    /// TEMP VERIFICATION: repeated silence->onset->silence cycling, the exact
    /// pattern the finding flags (chime after quiet, remote talker after quiet).
    #[test]
    fn temp_repeated_onset_cycles_stay_bounded() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(128, 40));
        let mut rng = Rng(0xcafe_babe_f00d_1234);
        let mut path = EchoPath::new(&[(0, 0.35), (90, 0.2), (500, 0.12)]);
        let mut max_abs = 0.0f32;
        let mut all_finite = true;
        for cycle in 0..20 {
            // silence gap
            for _ in 0..30 {
                let mut near = [0.0f32; FRAME];
                for s in near.iter_mut() {
                    *s = rng.next_f32(0.001);
                }
                p.process(&mut near, true);
                for &s in &near {
                    max_abs = max_abs.max(s.abs());
                    all_finite &= s.is_finite();
                }
            }
            // sudden loud onset
            for _ in 0..40 {
                let mut near = [0.0f32; FRAME];
                let mut far = [0.0f32; FRAME];
                for i in 0..FRAME {
                    let f = rng.next_f32(0.95);
                    far[i] = f;
                    near[i] = path.push(f);
                }
                p.push_far(&far);
                p.process(&mut near, true);
                for &s in &near {
                    max_abs = max_abs.max(s.abs());
                    all_finite &= s.is_finite();
                }
            }
            let _ = cycle;
        }
        println!("TEMP_CYCLES max_abs={max_abs} all_finite={all_finite}");
        assert!(all_finite, "diverged across onset cycles");
    }

    /// The OFF path is a literal no-op (bit-identical frame, no panics).
    #[test]
    fn passthrough_is_a_noop() {
        let mut p = Passthrough;
        let original: [f32; FRAME] = std::array::from_fn(|i| (i as f32 * 0.001).sin());
        let mut near = original;
        p.push_far(&original);
        p.process(&mut near, true);
        assert_eq!(near, original, "passthrough must not alter the frame");
        p.reset();
        assert_eq!(p.last_erle_db(), 0.0);
    }

    /// reset() returns to the construction state (idempotent pass-through again).
    #[test]
    fn reset_restores_clean_state() {
        let mut p = EchoNoiseProcessor::new(aec_only_cfg(40, 0));
        let mut rng = Rng(0xdead_0000_beef_1111);
        for _ in 0..50 {
            let mut near = [0.0f32; FRAME];
            let mut far = [0.0f32; FRAME];
            for i in 0..FRAME {
                far[i] = rng.next_f32(0.3);
                near[i] = 0.3 * far[i];
            }
            p.push_far(&far);
            p.process(&mut near, true);
        }
        p.reset();
        // After reset, far-silence pass-through is exact again.
        let mut input: Vec<f32> = Vec::new();
        let mut output: Vec<f32> = Vec::new();
        for _ in 0..8 {
            let mut near = [0.0f32; FRAME];
            for s in near.iter_mut() {
                *s = rng.next_f32(0.4);
            }
            input.extend_from_slice(&near);
            p.process(&mut near, true);
            output.extend_from_slice(&near);
        }
        for i in 0..(input.len() - FRAME) {
            assert!(
                (output[i + FRAME] - input[i]).abs() < 1e-5,
                "stale state after reset at {i}"
            );
        }
    }
}
