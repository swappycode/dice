//! Noise suppressor: an STFT decision-directed (Ephraim-Malah) Wiener filter
//! with an MCRA-style minimum-statistics noise floor and a HARD spectral floor.
//! Pure logic, deterministic, no IO — consumes the AEC residual PCM and returns
//! denoised PCM, streaming and sample-conserving.
//!
//! Design choices that keep it from murdering speech or making musical noise:
//!
//! - decision-directed a-priori SNR (beta = 0.98) smears the gain across time,
//! - a hard floor (`floor`, default -20 dB) so NO bin is ever zeroed — residual
//!   noise stays a smooth comfort floor instead of isolated spectral spikes,
//! - the noise estimate only updates in speech-ABSENT bins (S/Smin gated), so
//!   sustained speech is never slowly learned as noise,
//! - 3-bin cross-frequency gain smoothing.
//!
//! Root-Hann analysis+synthesis at 50 % overlap is COLA-exact, so at unit gain
//! the STFT reconstructs the input within FFT round-off (unit-tested).

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// STFT frame length.
const NF: usize = 256;
/// Hop (50 % overlap).
const HOP: usize = NF / 2;
/// Per-frame power smoothing for the speech-presence statistic.
const ASM: f32 = 0.7;
/// Noise-estimate update rate (only applied in speech-absent bins).
const AD: f32 = 0.95;
/// Decision-directed a-priori-SNR smoothing.
const DD_BETA: f32 = 0.98;
/// Over-subtraction: subtract this from the a-posteriori SNR before flooring.
const OVERSUB: f32 = 1.0;
/// Speech-presence threshold on S/Smin: above this the bin is "speech present"
/// and its noise estimate is frozen.
const MIN_RATIO: f32 = 5.0;
/// Minimum-statistics two-bucket window (frames). At HOP=128/48 kHz (~375 fps)
/// this spans ~0.32-0.64 s of running minimum.
const MIN_WINDOW: u32 = 120;

/// STFT decision-directed Wiener noise suppressor.
pub(super) struct WienerNs {
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// Root-Hann window (analysis == synthesis), length `NF`.
    win: Vec<f32>,
    in_buf: VecDeque<f32>,
    out: VecDeque<f32>,
    /// Sliding overlap-add accumulator, length `NF`.
    ola: Vec<f32>,
    /// Per-bin state (length `NF`).
    s: Vec<f32>, // smoothed power
    smin: Vec<f32>,          // running min (current window)
    stmp: Vec<f32>,          // running min (next window, building)
    noise: Vec<f32>,         // noise power estimate
    g_prev: Vec<f32>,        // previous gain (decision-directed)
    gamma_prev: Vec<f32>,    // previous a-posteriori SNR
    gains: Vec<f32>,         // per-frame gains (smoothed before applying)
    spec: Vec<Complex<f32>>, // reusable spectrum scratch
    floor: f32,
    min_cnt: u32,
    primed: bool,
}

impl WienerNs {
    pub(super) fn new(floor: f32) -> Self {
        use std::f32::consts::PI;
        // Periodic Hann, then its square root: rh*rh = Hann, and Hann at 50 %
        // overlap sums to 1 (COLA) -> perfect reconstruction at unit gain.
        let win: Vec<f32> = (0..NF)
            .map(|n| {
                let hann = 0.5 - 0.5 * (2.0 * PI * n as f32 / NF as f32).cos();
                hann.max(0.0).sqrt()
            })
            .collect();
        let mut planner = FftPlanner::<f32>::new();
        Self {
            fft: planner.plan_fft_forward(NF),
            ifft: planner.plan_fft_inverse(NF),
            win,
            in_buf: VecDeque::new(),
            out: VecDeque::new(),
            ola: vec![0.0; NF],
            s: vec![0.0; NF],
            smin: vec![0.0; NF],
            stmp: vec![0.0; NF],
            noise: vec![0.0; NF],
            g_prev: vec![1.0; NF],
            gamma_prev: vec![1.0; NF],
            gains: vec![1.0; NF],
            spec: vec![Complex::ZERO; NF],
            floor,
            min_cnt: 0,
            primed: false,
        }
    }

    pub(super) fn reset(&mut self) {
        self.in_buf.clear();
        self.out.clear();
        self.ola.iter_mut().for_each(|s| *s = 0.0);
        self.s.iter_mut().for_each(|x| *x = 0.0);
        self.smin.iter_mut().for_each(|x| *x = 0.0);
        self.stmp.iter_mut().for_each(|x| *x = 0.0);
        self.noise.iter_mut().for_each(|x| *x = 0.0);
        self.g_prev.iter_mut().for_each(|x| *x = 1.0);
        self.gamma_prev.iter_mut().for_each(|x| *x = 1.0);
        self.min_cnt = 0;
        self.primed = false;
    }

    /// Denoise a block of samples; returns however many output samples are ready
    /// (streaming, sample-conserving in steady state, latency ~one frame).
    pub(super) fn run(&mut self, block: &[f32]) -> Vec<f32> {
        self.in_buf.extend(block.iter().copied());
        let mut produced = Vec::new();
        while self.in_buf.len() >= NF {
            self.process_frame();
            for _ in 0..HOP {
                if let Some(s) = self.out.pop_front() {
                    produced.push(s);
                }
            }
        }
        produced
    }

    fn process_frame(&mut self) {
        // 1) Windowed analysis FFT of the leading NF samples (overlap retained).
        for i in 0..NF {
            self.spec[i] = Complex::new(self.in_buf[i] * self.win[i], 0.0);
        }
        self.fft.process(&mut self.spec);

        // 2) Per-bin noise tracking + decision-directed Wiener gain.
        let advance_window = self.min_cnt >= MIN_WINDOW;
        for bin in 0..NF {
            let pw = self.spec[bin].norm_sqr();
            if !self.primed {
                self.s[bin] = pw;
                self.smin[bin] = pw;
                self.stmp[bin] = pw;
                // Seed the noise floor LOW (not at the first frame's power): if the
                // session opens mid-speech a high seed would over-suppress until the
                // running minimum converges. Starting low only briefly
                // under-suppresses, then the speech-absent gate adapts it upward.
                self.noise[bin] = pw * 0.1;
            } else {
                self.s[bin] = ASM * self.s[bin] + (1.0 - ASM) * pw;
            }
            // Two-bucket running minimum.
            if advance_window {
                self.smin[bin] = self.stmp[bin].min(self.s[bin]);
                self.stmp[bin] = self.s[bin];
            } else {
                self.smin[bin] = self.smin[bin].min(self.s[bin]);
                self.stmp[bin] = self.stmp[bin].min(self.s[bin]);
            }
            // Update the noise floor only where speech is absent.
            let speech_absent = self.s[bin] < MIN_RATIO * self.smin[bin].max(1e-12);
            if speech_absent {
                self.noise[bin] = AD * self.noise[bin] + (1.0 - AD) * pw;
            }
            // Decision-directed a-priori SNR -> Wiener gain -> hard floor. Clamp
            // the a-posteriori SNR so a collapsed noise floor after long silence
            // can never produce a non-finite gain.
            let gamma = (pw / (self.noise[bin] + 1e-12)).min(1e9);
            let xi = DD_BETA * self.g_prev[bin] * self.g_prev[bin] * self.gamma_prev[bin]
                + (1.0 - DD_BETA) * (gamma - OVERSUB).max(0.0);
            let g = (xi / (xi + 1.0)).max(self.floor);
            self.gains[bin] = g;
            self.g_prev[bin] = g;
            self.gamma_prev[bin] = gamma;
        }
        if advance_window {
            self.min_cnt = 0;
        } else {
            self.min_cnt += 1;
        }
        self.primed = true;

        // 3) 3-bin cross-frequency gain smoothing, then apply (clone the gains so
        //    the smoothing reads pre-smoothing neighbours).
        let raw = self.gains.clone();
        for bin in 0..NF {
            let lo = raw[bin.saturating_sub(1)];
            let hi = raw[(bin + 1).min(NF - 1)];
            let g = 0.25 * lo + 0.5 * raw[bin] + 0.25 * hi;
            self.spec[bin] *= g;
        }

        // 4) Inverse FFT, windowed synthesis, overlap-add.
        self.ifft.process(&mut self.spec);
        let inv_n = 1.0 / NF as f32;
        for i in 0..NF {
            self.ola[i] += self.spec[i].re * inv_n * self.win[i];
        }
        // Emit the HOP samples that are now complete; slide the accumulator.
        for i in 0..HOP {
            self.out.push_back(self.ola[i]);
        }
        self.ola.copy_within(HOP..NF, 0);
        for i in (NF - HOP)..NF {
            self.ola[i] = 0.0;
        }
        // Consume HOP input samples (the rest overlaps into the next frame).
        self.in_buf.drain(..HOP);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Root-Hann analysis*synthesis at 50 % overlap is COLA-exact: the squared
    /// window summed over the two overlapping frames is 1 at every phase, so unit
    /// gain reconstructs the input (the musical-noise-free reconstruction basis).
    #[test]
    fn root_hann_is_cola_exact_at_50pct() {
        let ns = WienerNs::new(0.1);
        for n in 0..HOP {
            let a = ns.win[n] * ns.win[n];
            let b = ns.win[n + HOP] * ns.win[n + HOP];
            assert!((a + b - 1.0).abs() < 1e-5, "COLA broken at {n}: {}", a + b);
        }
    }

    /// End-to-end STFT plumbing: with the floor pinned to 1.0 every bin gain is
    /// exactly 1.0 (`xi/(xi+1) < 1` always), so analysis -> overlap-add synthesis
    /// must reconstruct the input (delayed by the fixed frame latency) within FFT
    /// round-off — proving the framing / hop / overlap-add conserves samples.
    #[test]
    fn unit_gain_reconstructs_the_input() {
        let mut ns = WienerNs::new(1.0);
        let mut rng = 0x1234_5678u64;
        let x: Vec<f32> = (0..4000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                ((rng >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0) * 0.5
            })
            .collect();
        let y = ns.run(&x);
        // Steady-state reconstruction error at the best lag (the STFT latency).
        let mut best_err = f32::INFINITY;
        for lag in 0..2 * NF {
            let mut e = 0.0f32;
            let mut cnt = 0u32;
            for i in 2000..3000 {
                if i + lag < y.len() {
                    e += (y[i + lag] - x[i]).powi(2);
                    cnt += 1;
                }
            }
            if cnt > 0 {
                best_err = best_err.min(e / cnt as f32);
            }
        }
        assert!(
            best_err.sqrt() < 1e-3,
            "unit-gain reconstruction rms error {}",
            best_err.sqrt()
        );
    }
}
