//! Acoustic echo canceller: a constrained, partitioned-block frequency-domain
//! NLMS adaptive filter (overlap-save MDF). Pure logic, deterministic, no IO —
//! it cancels the buffered far-end (loudspeaker) signal out of the near-end
//! (mic) on a per-256-sample block, given the two already time-aligned by the
//! caller ([`super::EchoNoiseProcessor`] owns the far delay line).
//!
//! Why frequency-domain partitioned: a usable desktop echo tail is ~128 ms
//! (6144 taps); a per-sample time-domain NLMS that long is hot and numerically
//! twitchy on correlated speech, whereas K partitions of a 512-pt FFT reach the
//! same tail with per-bin power-normalised NLMS that decorrelates the far-end
//! and converges faster, and the partitions self-select the echo lag (a coarse
//! delay estimate for free). The gradient (window) constraint on every partition
//! each block is the structural guard against circular-convolution wrap — the
//! single most important divergence guard in block AEC.

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Block / hop length (samples). 256 @ 48 kHz = 5.33 ms.
pub(super) const B: usize = 256;
/// FFT length (overlap-save: `B` carried + `B` new).
const N: usize = 2 * B;

/// Far-end power smoothing (per-bin NLMS denominator tracker).
const LAMBDA: f32 = 0.9;
/// Per-block coefficient leakage: relaxes a stale filter toward zero, bounds
/// coefficient energy, and forgets a changed echo path within ~seconds.
const LEAK: f32 = 0.9999;
/// Absolute regularisation floor for the NLMS denominator.
const DELTA0: f32 = 1e-3;
/// Relative regularisation (tracks far level); small so it does not cap ERLE.
const DELTA_REL: f32 = 1e-3;
/// Far-end block RMS below this counts as silence -> freeze adaptation (no echo
/// can exist without a far-end, so there is nothing to learn; adapting here only
/// lets the normalised step random-walk the weights).
const FAR_SIL_RMS: f32 = 1.5e-3;
/// Geigel double-talk threshold: near-end peak above this fraction of the recent
/// far-end peak implies near-end speech on top of the echo (the echo path
/// attenuates, so a loud near sample can't be echo alone) -> freeze.
const T_GEIGEL: f32 = 0.5;
/// Decay of the tracked far-end peak (~0.95^block over the recent tail).
const FAR_PEAK_DECAY: f32 = 0.95;
/// Freeze adaptation for this many blocks after a double-talk trigger (~160 ms).
const DT_HANG: u32 = 30;
/// Divergence watchdog: residual louder than near for this many blocks -> halve
/// (~160 ms response — fast enough to catch a transient before it clips).
const DIVERGE_BLOCKS: u32 = 30;
/// Hard clamp on the effective step so a mistuned gate can never over-step.
const MU_MAX: f32 = 0.5;

/// Constrained partitioned-block frequency-domain NLMS echo canceller.
pub(super) struct Mdf {
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// Partition count (filter tail = `k * B` taps).
    k: usize,
    mu: f32,
    /// Overlap-save far input window `[prev B | cur B]`.
    far_win: Vec<f32>,
    /// Last `k` far-end spectra, newest at front; each length `N`.
    x_hist: VecDeque<Vec<Complex<f32>>>,
    /// `k` frequency-domain weight blocks, each length `N`.
    w: Vec<Vec<Complex<f32>>>,
    /// Per-bin smoothed far power (NLMS denominator), length `N`.
    p: Vec<f32>,
    /// Per-bin CURRENT (un-smoothed) far power; floors the denominator so a far
    /// onset after silence cannot under-normalise and over-step on its 1st block.
    cur_pk: Vec<f32>,
    /// Scratch (length `N`) reused for every transform to avoid per-block allocs
    /// on the filter/update path.
    scratch: Vec<Complex<f32>>,
    delta: f32,
    far_peak: f32,
    dt_hang: u32,
    diverge_cnt: u32,
    rho: f32,
    last_erle_db: f32,
}

impl Mdf {
    pub(super) fn new(tail_ms: u32, sample_rate: u32, mu: f32) -> Self {
        let tail = (tail_ms as usize) * (sample_rate as usize / 1000);
        let k = tail.div_ceil(B).max(1);
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(N);
        let ifft = planner.plan_fft_inverse(N);
        Self {
            fft,
            ifft,
            k,
            mu,
            far_win: vec![0.0; N],
            x_hist: VecDeque::with_capacity(k),
            w: vec![vec![Complex::ZERO; N]; k],
            p: vec![0.0; N],
            cur_pk: vec![0.0; N],
            scratch: vec![Complex::ZERO; N],
            delta: DELTA0,
            far_peak: 0.0,
            dt_hang: 0,
            diverge_cnt: 0,
            rho: 1.0,
            last_erle_db: 0.0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.far_win.iter_mut().for_each(|s| *s = 0.0);
        self.x_hist.clear();
        for wk in &mut self.w {
            wk.iter_mut().for_each(|c| *c = Complex::ZERO);
        }
        self.p.iter_mut().for_each(|p| *p = 0.0);
        self.cur_pk.iter_mut().for_each(|p| *p = 0.0);
        self.delta = DELTA0;
        self.far_peak = 0.0;
        self.dt_hang = 0;
        self.diverge_cnt = 0;
        self.rho = 1.0;
        self.last_erle_db = 0.0;
    }

    pub(super) fn last_erle_db(&self) -> f32 {
        self.last_erle_db
    }

    /// Forward DFT of a real length-`N` signal into a fresh spectrum.
    fn rfft(&self, time: &[f32]) -> Vec<Complex<f32>> {
        let mut buf: Vec<Complex<f32>> = time.iter().map(|&s| Complex::new(s, 0.0)).collect();
        self.fft.process(&mut buf);
        buf
    }

    /// Observe one far-end block: slide the overlap-save window, transform it,
    /// push it into the partition history, and update the per-bin far power +
    /// regularisation. Called for EVERY far block — including while the mic is
    /// muted and [`Self::run`] is skipped — so the far reference stays continuous
    /// and the echo estimate is correctly aligned the instant the mic re-opens.
    pub(super) fn observe_far(&mut self, far: &[f32; B]) {
        self.far_win.copy_within(B..N, 0);
        self.far_win[B..N].copy_from_slice(far);
        let x = self.rfft(&self.far_win);
        self.x_hist.push_front(x);
        self.x_hist.truncate(self.k);
        for bin in 0..N {
            let mut pk = 0.0f32;
            for xk in &self.x_hist {
                pk += xk[bin].norm_sqr();
            }
            self.cur_pk[bin] = pk;
            self.p[bin] = LAMBDA * self.p[bin] + (1.0 - LAMBDA) * pk;
        }
        let mean_p: f32 = self.p.iter().sum::<f32>() / N as f32;
        self.delta = DELTA0 + DELTA_REL * mean_p;
    }

    /// Cancel the echo from one near-end block, given its time-aligned far block.
    /// Returns the residual (near - echo estimate), length `B`.
    pub(super) fn run(&mut self, far: &[f32; B], near: &[f32; B]) -> [f32; B] {
        // 1) Track the far-end (window slide + spectrum + power).
        self.observe_far(far);

        // 2) Forward filter: Y = Σ_k W_k · X_k ; echo = last B of IFFT(Y).
        for s in &mut self.scratch {
            *s = Complex::ZERO;
        }
        for (wk, xk) in self.w.iter().zip(self.x_hist.iter()) {
            for bin in 0..N {
                self.scratch[bin] += wk[bin] * xk[bin];
            }
        }
        self.ifft.process(&mut self.scratch);
        let inv_n = 1.0 / N as f32;
        let mut e = [0.0f32; B];
        for (i, e_i) in e.iter_mut().enumerate() {
            *e_i = near[i] - self.scratch[B + i].re * inv_n;
        }

        // 3) Energies + adaptation gates.
        let pe: f32 = e.iter().map(|&v| v * v).sum();
        let pd: f32 = near.iter().map(|&v| v * v).sum::<f32>() + 1e-12;
        let far_rms = (far.iter().map(|&v| v * v).sum::<f32>() / B as f32).sqrt();
        let far_silent = far_rms < FAR_SIL_RMS;
        let near_peak = near.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        let far_blk_peak = far.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        self.far_peak = (self.far_peak * FAR_PEAK_DECAY).max(far_blk_peak);
        let double_talk = !far_silent && near_peak > T_GEIGEL * self.far_peak;
        if double_talk {
            self.dt_hang = DT_HANG;
        }
        let mut mu_eff = if far_silent { 0.0 } else { self.mu };
        if self.dt_hang > 0 {
            self.dt_hang -= 1;
            mu_eff = 0.0;
        }
        mu_eff = mu_eff.clamp(0.0, MU_MAX);
        // True iff adaptation is frozen this block (far-silent or double-talk).
        let frozen = mu_eff == 0.0;

        // 4) NLMS update with the per-partition gradient (window) constraint.
        if mu_eff > 0.0 {
            // E = FFT of [0(B) | e(B)] — the overlap-save gradient constraint.
            for s in self.scratch.iter_mut() {
                *s = Complex::ZERO;
            }
            for (i, &e_i) in e.iter().enumerate() {
                self.scratch[B + i] = Complex::new(e_i, 0.0);
            }
            self.fft.process(&mut self.scratch);
            // Clone E out of scratch (constrain() reuses scratch per partition).
            let grad = self.scratch.clone();
            for k in 0..self.x_hist.len() {
                {
                    let xk = &self.x_hist[k];
                    let wk = &mut self.w[k];
                    for bin in 0..N {
                        // Normalise by the larger of the smoothed and the CURRENT
                        // far power, so a sudden far onset after silence (smoothed
                        // power still lagging) cannot under-normalise the step.
                        let denom = self.p[bin].max(self.cur_pk[bin]) + self.delta;
                        wk[bin] += (mu_eff / denom) * grad[bin] * xk[bin].conj();
                    }
                }
                self.constrain(k);
            }
        }

        // 5) Leakage (always) — bounds energy, forgets stale paths.
        for wk in &mut self.w {
            for c in wk.iter_mut() {
                *c *= LEAK;
            }
        }

        // 6) Divergence watchdog: a residual sustained louder than the near-end
        //    means the filter is amplifying rather than cancelling -> halve it.
        //    While adaptation is frozen (double-talk / far-silence) a large
        //    residual is EXPECTED, so neither count it (it would wreck a converged
        //    filter) NOR reset the counter (so a real pre-freeze divergence is
        //    still caught once the freeze lifts).
        if !frozen {
            if pe > 1.5 * pd {
                self.diverge_cnt += 1;
                if self.diverge_cnt > DIVERGE_BLOCKS {
                    for wk in &mut self.w {
                        for c in wk.iter_mut() {
                            *c *= 0.5;
                        }
                    }
                    self.diverge_cnt = 0;
                }
            } else {
                self.diverge_cnt = 0;
            }
        }

        // 7) ERLE metric (smoothed), for tests/diagnostics.
        self.rho = 0.9 * self.rho + 0.1 * (pe / pd);
        self.last_erle_db = if self.rho > 1e-9 {
            (-10.0 * self.rho.log10()).max(0.0)
        } else {
            0.0
        };
        e
    }

    /// Gradient (window) constraint on partition `k`: project the weight block
    /// back to a `B`-tap filter (IFFT, zero the upper half, FFT). Without this
    /// the partitioned circular convolution wraps and the filter diverges.
    fn constrain(&mut self, k: usize) {
        self.scratch.copy_from_slice(&self.w[k]);
        self.ifft.process(&mut self.scratch);
        let inv_n = 1.0 / N as f32;
        for i in 0..B {
            self.scratch[i] *= inv_n;
        }
        for i in B..N {
            self.scratch[i] = Complex::ZERO;
        }
        self.fft.process(&mut self.scratch);
        self.w[k].copy_from_slice(&self.scratch);
    }
}
