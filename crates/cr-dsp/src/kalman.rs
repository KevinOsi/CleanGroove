//! DeCrackle: a fixed-lag Kalman smoother over the last 64 samples, driven by the order-10 AR
//! model of the prediction-error stage.
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 9.

use crate::burg::reflection_to_predictor;
use crate::params::round_sym;

/// Samples the smoother holds (its lag): a sample is written out this many steps after it
/// entered.
pub const LAG: usize = 64;
const RING: usize = LAG - 1;
/// AR model order.
const ORDER: usize = 10;
/// Steps a smoothing window stays open after the last crackle sample.
const WINDOW_STEPS: i32 = 11;

/// Smoother state for one band.
#[derive(Debug, Clone)]
pub struct KalmanBandState {
    /// Predictor coefficients: `ar[n - 1]` multiplies the sample `n` steps back.
    pub ar: [f64; ORDER],
    /// The last `LAG` samples, being smoothed (a ring indexed by frame).
    pub x: [f64; LAG],
    /// Error covariance of `x`, `cov[row + col * LAG]`.
    pub cov: Vec<f64>,
    /// The covariance projected through the predictor (one value per ring slot).
    pub cov_ar: [f64; LAG],
    /// Kalman gain per ring slot.
    pub gain: [f64; LAG],
    /// Steps left in the current smoothing window; 0 when no window is open.
    pub steps_left: i32,
    /// Steps since the window opened (how much of the ring the window covers).
    pub filled: i32,
    /// +10 per crackle sample, -1 per other sample, reset when new coefficients load. A
    /// positive score when a window closes means the excitation there must be recomputed.
    pub crackle_score: i32,
    /// Running level of the prediction error: `lambda * level + |error|`.
    pub error_level: f64,
}

fn lambda(is_wavelet: bool) -> f64 {
    if is_wavelet {
        0.996
    } else {
        0.998
    }
}

impl KalmanBandState {
    pub fn new(is_wavelet: bool) -> Self {
        Self {
            ar: [0.0; ORDER],
            x: [0.0; LAG],
            cov: vec![0.0; LAG * LAG],
            cov_ar: [0.0; LAG],
            gain: [0.0; LAG],
            steps_left: 0,
            filled: 0,
            crackle_score: 0,
            error_level: 10.0 / (1.0 - lambda(is_wavelet)),
        }
    }

    pub fn reset(&mut self, is_wavelet: bool) {
        *self = Self::new(is_wavelet);
    }

    /// Loads the AR model for the next 256 samples from a block's reflection coefficients.
    pub fn load_model(&mut self, refl: &[f64]) {
        reflection_to_predictor(ORDER, refl, &mut self.ar);
        self.crackle_score = -10;
    }

    /// One smoother step for the sample in ring slot `slot`. A `missing` sample (crackle) is
    /// replaced by its prediction and opens or extends the smoothing window; otherwise the
    /// observation updates the window while one is open.
    pub fn step(&mut self, slot: usize, missing: bool) {
        let steps_left = if missing { WINDOW_STEPS } else { self.steps_left - 1 };
        // Ring slots the window covers, as offsets from `slot`: first..=LAG (LAG is `slot`
        // itself, wrapped round).
        let first = (LAG as i32 - self.filled).max(1) as usize;
        let at = |offset: usize| (slot + offset) & RING;

        if steps_left > 0 {
            // Prediction error of this sample from the ten before it.
            let mut innovation = self.x[slot];
            for lag in (1..=ORDER).rev() {
                innovation -= self.ar[lag - 1] * self.x[(slot + LAG - lag) & RING];
            }
            self.x[slot] -= innovation;

            // Covariance through the predictor, over the model order the window has so far.
            let order = (self.filled as usize).min(ORDER);
            for k in first..=LAG {
                let col = at(k) * LAG;
                let mut sum = 0.0;
                for lag in (1..=order).rev() {
                    sum += self.ar[lag - 1] * self.cov[((slot + LAG - lag) & RING) + col];
                }
                self.cov_ar[k & RING] = sum;
            }

            // Innovation variance, then the gain.
            let mut variance = 1.0;
            for i in 0..order {
                variance += self.ar[i] * self.cov_ar[RING - i];
            }
            self.cov_ar[0] = variance;
            let scale = 1.0 / variance;
            for k in first..=LAG {
                self.gain[k & RING] = scale * self.cov_ar[k & RING];
            }

            if missing {
                // No observation: the sample's row and column take the projected covariance.
                for k in first..=LAG {
                    let p = at(k);
                    let v = self.cov_ar[k & RING];
                    self.cov[slot + p * LAG] = v;
                    self.cov[p + slot * LAG] = v;
                }
            } else {
                // Observation: reduce the covariance, clear the sample's row and column, and
                // correct the window by the gain.
                for k1 in first..LAG {
                    let row = at(k1);
                    for k2 in first..LAG {
                        self.cov[row + at(k2) * LAG] -= self.cov_ar[k1] * self.gain[k2];
                    }
                }
                for k in first..=LAG {
                    let p = at(k);
                    self.cov[slot + p * LAG] = 0.0;
                    self.cov[p + slot * LAG] = 0.0;
                    self.x[p] += innovation * self.gain[k & RING];
                }
            }
        } else {
            // The window closes: clear the covariance it covered.
            for k1 in first..=LAG {
                let row = at(k1);
                for k2 in first..=LAG {
                    self.cov[row + at(k2) * LAG] = 0.0;
                }
            }
        }

        self.steps_left = steps_left;
        self.filled = if steps_left == 0 { 0 } else { self.filled + 1 };
    }

    /// Takes in the newest sample and its prediction error, after writing out the sample
    /// `LAG` steps old (into `samples[commit]` when it changed). Returns whether it changed.
    fn advance(&mut self, slot: usize, sample: i32, error: f64, lambda: f64, commit: Option<&mut i32>) -> bool {
        let mut changed = false;
        if let Some(out) = commit {
            let smoothed = round_sym(self.x[slot]);
            if smoothed != *out {
                *out = smoothed;
                changed = true;
            }
        }
        self.x[slot] = sample as f64;
        self.error_level = lambda * self.error_level + error;
        changed
    }

    /// Whether the sample at `i` is crackle: its local error energy against the running level.
    /// Updates the crackle score.
    fn is_crackle(&mut self, excit: &[f64], i: usize, mask: usize, factor: f64, past_edge: bool) -> bool {
        let prev = excit[i.wrapping_sub(1) & mask];
        let next = excit[(i + 1) & mask];
        let energy = prev.powi(2) + next.powi(2) + 2.0 * excit[i].powi(2);
        if energy > factor * self.error_level * self.error_level && past_edge {
            self.crackle_score += 10;
            true
        } else {
            self.crackle_score -= 1;
            false
        }
    }
}

/// The reflection coefficients stored for the 256-sample block containing `frame`.
pub fn block_refl(block_refl: &[f64], frame: isize) -> &[f64] {
    let blocks = (block_refl.len() / ORDER) as isize;
    let slot = ORDER * (frame / 256).rem_euclid(blocks) as usize;
    &block_refl[slot..slot + ORDER]
}

/// One frame of DeCrackle for a single-band (Simple) channel. Returns (the sample written
/// out, if it changed, as (index, value); whether the excitation must be recomputed).
pub fn process_kalman_frame(
    kalman: &mut KalmanBandState,
    frame: isize,
    frame_limit: isize,
    edge: isize,
    mask: usize,
    samples: &mut [i32],
    fwd_excit: &[f64],
    block_refls: &[f64],
    flags: &mut [u8],
    lambda: f64,
    factor: f64,
    enabled: bool,
) -> (Option<(usize, i32)>, bool) {
    if frame < 0 || frame >= frame_limit {
        return (None, false);
    }
    if (frame & 0xFF) == 0 {
        kalman.load_model(block_refl(block_refls, frame));
    }

    let slot = (frame as usize) & RING;
    let i = (frame as usize) & mask;
    let commit = (frame >= LAG as isize).then(|| ((frame - LAG as isize) as usize) & mask);
    let changed = match commit {
        Some(c) => {
            let mut v = samples[c];
            let changed = kalman.advance(slot, samples[i], fwd_excit[i], lambda, Some(&mut v));
            if changed {
                samples[c] = v;
                flags[c] |= crate::flag::DECRACKLED;
            }
            changed.then_some((c, v))
        }
        None => {
            kalman.advance(slot, samples[i], fwd_excit[i], lambda, None);
            None
        }
    };

    let crackle = enabled && kalman.is_crackle(fwd_excit, i, mask, factor, frame > edge);

    let mut recompute = false;
    if crackle {
        kalman.step(slot, true);
    } else if kalman.steps_left > 0 {
        kalman.step(slot, false);
        recompute = kalman.steps_left == 0 && kalman.crackle_score > 0;
    }
    (changed, recompute)
}

/// One frame of DeCrackle for both bands of a Wavelet channel. Crackle in either band opens
/// the window in both. Returns whether each band's excitation must be recomputed.
pub fn process_kalman_wavelet_frame(
    lo: &mut KalmanBandState,
    hi: &mut KalmanBandState,
    frame: isize,
    frame_limit: isize,
    edge: isize,
    mask: usize,
    lo_samples: &mut [i32],
    hi_samples: &mut [i32],
    lo_fwd_excit: &[f64],
    hi_fwd_excit: &[f64],
    lo_block_refls: &[f64],
    hi_block_refls: &[f64],
    flags: &mut [u8],
    lambda: f64,
    factor: f64,
    enabled: bool,
) -> (bool, bool) {
    if frame < 0 || frame >= frame_limit {
        return (false, false);
    }
    if (frame & 0xFF) == 0 {
        lo.load_model(block_refl(lo_block_refls, frame));
        hi.load_model(block_refl(hi_block_refls, frame));
    }

    let slot = (frame as usize) & RING;
    let i = (frame as usize) & mask;
    if frame >= LAG as isize {
        let c = ((frame - LAG as isize) as usize) & mask;
        let (mut l, mut h) = (lo_samples[c], hi_samples[c]);
        let changed_lo = lo.advance(slot, lo_samples[i], lo_fwd_excit[i], lambda, Some(&mut l));
        let changed_hi = hi.advance(slot, hi_samples[i], hi_fwd_excit[i], lambda, Some(&mut h));
        lo_samples[c] = l;
        hi_samples[c] = h;
        if changed_lo || changed_hi {
            flags[c] |= crate::flag::DECRACKLED;
        }
    } else {
        lo.advance(slot, lo_samples[i], lo_fwd_excit[i], lambda, None);
        hi.advance(slot, hi_samples[i], hi_fwd_excit[i], lambda, None);
    }

    let (mut crackle_lo, mut crackle_hi) = (false, false);
    if enabled {
        crackle_lo = lo.is_crackle(lo_fwd_excit, i, mask, factor, frame > edge);
        crackle_hi = hi.is_crackle(hi_fwd_excit, i, mask, factor, frame > edge);
    }

    let (mut recompute_lo, mut recompute_hi) = (false, false);
    if crackle_lo || crackle_hi {
        lo.step(slot, true);
        hi.step(slot, true);
    } else {
        if lo.steps_left > 0 {
            lo.step(slot, false);
            recompute_lo = lo.steps_left == 0 && lo.crackle_score > 0;
        }
        if hi.steps_left > 0 {
            hi.step(slot, false);
            recompute_hi = hi.steps_left == 0 && hi.crackle_score > 0;
        }
    }
    (recompute_lo, recompute_hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_sample_is_predicted_and_the_window_closes() {
        let mut k = KalmanBandState::new(false);
        k.ar = [0.8, -0.2, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        for i in 0..LAG {
            k.x[i] = i as f64 * 100.0;
        }
        k.x[10] = 50_000.0;
        k.step(10, true);
        assert_eq!((k.steps_left, k.filled), (WINDOW_STEPS, 1));
        assert!(k.x[10] < 50_000.0, "the missing sample is replaced by its prediction");

        for s in 1..=WINDOW_STEPS as usize {
            k.step((10 + s) & RING, false);
        }
        assert_eq!((k.steps_left, k.filled), (0, 0));
    }
}
