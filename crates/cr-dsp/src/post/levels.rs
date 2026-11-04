//! Output level check: sample peak, true peak (4x oversampled) and integrated loudness
//! (ITU-R BS.1770 / EBU R128, K-weighted and gated).

use std::collections::VecDeque;
use std::f64::consts::PI;

use super::filters::Biquad;
use super::FULL_SCALE;

/// Level measurements of a finished output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelReport {
    pub sample_peak_dbfs: f64,
    /// Inter-sample peak estimate from 4x oversampling.
    pub true_peak_dbtp: f64,
    /// Integrated loudness, LUFS (-inf for silence).
    pub loudness_lufs: f64,
    /// Samples beyond full scale (would clip when written as integers).
    pub over_full_scale: usize,
}

impl LevelReport {
    /// ReplayGain 2.0 album gain (reference -18 LUFS).
    pub fn replaygain_db(&self) -> f64 {
        -18.0 - self.loudness_lufs
    }

    /// The same report after a gain change.
    pub fn with_gain(&self, gain_db: f64, over_full_scale: usize) -> Self {
        Self {
            sample_peak_dbfs: self.sample_peak_dbfs + gain_db,
            true_peak_dbtp: self.true_peak_dbtp + gain_db,
            loudness_lufs: self.loudness_lufs + gain_db,
            over_full_scale,
        }
    }
}

/// Half length of the 4x interpolation filter (taps per phase = 2 * TP_HALF).
const TP_HALF: usize = 12;

/// K-weighting for any sample rate (BS.1770 pre-filter shelf and RLB high-pass, derived from
/// their analogue prototypes as in libebur128).
fn k_weighting(sample_rate: f64) -> [Biquad; 2] {
    let f0 = 1681.974450955533;
    let g = 3.999843853973347;
    let q = 0.7071752369554196;
    let k = (PI * f0 / sample_rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new(vh + vb * k / q + k * k, 2.0 * (k * k - vh), vh - vb * k / q + k * k, a0, 2.0 * (k * k - 1.0), 1.0 - k / q + k * k);

    let f0 = 38.13547087602444;
    let q = 0.5003270373238773;
    let k = (PI * f0 / sample_rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let rlb = Biquad::new(1.0, -2.0, 1.0, a0, 2.0 * (k * k - 1.0), 1.0 - k / q + k * k);
    [shelf, rlb]
}

/// Streaming biquad state (the chain's `Biquad::run` works on whole slices).
#[derive(Clone, Copy, Default)]
struct State(f64, f64);

fn step(bq: &Biquad, s: &mut State, x: f64) -> f64 {
    let c = bq.coefficients();
    let y = c.0 * x + s.0;
    s.0 = c.1 * x - c.3 * y + s.1;
    s.1 = c.2 * x - c.4 * y;
    y
}

/// Accumulates level measurements over the output in order. Feed samples in the internal
/// domain (full scale = 16_777_216), unclamped.
pub struct LevelMeter {
    k: [Biquad; 2],
    k_state: [[State; 2]; 2],
    sub_len: usize,
    sub_count: usize,
    sub_energy: f64,
    /// Mean-square K-weighted energy (summed over channels) of each 100 ms sub-block.
    subs: Vec<f64>,
    peak: f64,
    over: usize,
    true_peak: f64,
    history: [VecDeque<f64>; 2],
    tp_phases: Vec<Vec<f64>>,
    /// Largest sum of |coefficients| over the phases: an interpolated value can exceed the
    /// largest neighbouring sample by at most this factor.
    tp_gain_bound: f64,
    /// Interpolate while this many more samples remain within reach of a loud one; elsewhere
    /// no interpolated value can beat the true peak found so far.
    tp_loud_left: [usize; 2],
}

impl LevelMeter {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1) as f64;
        // 4x interpolation phases at offsets 1/4, 2/4, 3/4 (Hann-windowed sinc).
        let tp_phases: Vec<Vec<f64>> = (1..4)
            .map(|p| {
                let frac = p as f64 / 4.0;
                (0..2 * TP_HALF)
                    .map(|k| {
                        let t = k as f64 - (TP_HALF as f64 - 1.0) - frac;
                        let sinc = if t.abs() < 1e-12 { 1.0 } else { (PI * t).sin() / (PI * t) };
                        let w = 0.5 + 0.5 * (PI * t / (TP_HALF as f64 + 1.0)).cos();
                        sinc * w
                    })
                    .collect()
            })
            .collect();
        let tp_gain_bound = tp_phases
            .iter()
            .map(|p: &Vec<f64>| p.iter().map(|c| c.abs()).sum::<f64>())
            .fold(1.0, f64::max);
        Self {
            tp_gain_bound,
            tp_loud_left: [0; 2],
            k: k_weighting(sr),
            k_state: [[State::default(); 2]; 2],
            sub_len: (sample_rate as usize / 10).max(1),
            sub_count: 0,
            sub_energy: 0.0,
            subs: Vec::new(),
            peak: 0.0,
            over: 0,
            true_peak: 0.0,
            history: [VecDeque::with_capacity(2 * TP_HALF), VecDeque::with_capacity(2 * TP_HALF)],
            tp_phases,
        }
    }

    pub fn feed(&mut self, frames: &[(f64, f64)]) {
        for &(l, r) in frames {
            let mut energy = 0.0;
            for (ch, v) in [l, r].into_iter().enumerate() {
                let a = v.abs();
                self.peak = self.peak.max(a);
                self.over += (a > FULL_SCALE - 1.0) as usize;
                let x = v / FULL_SCALE;
                let shelved = step(&self.k[0], &mut self.k_state[ch][0], x);
                let y = step(&self.k[1], &mut self.k_state[ch][1], shelved);
                energy += y * y;

                let h = &mut self.history[ch];
                h.push_back(v);
                if h.len() > 2 * TP_HALF {
                    h.pop_front();
                }
                // Only interpolate near samples loud enough to matter: if every sample in the
                // filter's reach is below true_peak / tp_gain_bound, no interpolated value can
                // exceed the true peak already found.
                if a * self.tp_gain_bound > self.true_peak {
                    self.tp_loud_left[ch] = 2 * TP_HALF;
                }
                if self.tp_loud_left[ch] > 0 {
                    self.tp_loud_left[ch] -= 1;
                }
                if h.len() == 2 * TP_HALF && self.tp_loud_left[ch] > 0 {
                    for phase in &self.tp_phases {
                        let s: f64 = phase.iter().zip(h.iter()).map(|(c, x)| c * x).sum();
                        self.true_peak = self.true_peak.max(s.abs());
                    }
                }
            }
            self.sub_energy += energy;
            self.sub_count += 1;
            if self.sub_count == self.sub_len {
                self.subs.push(self.sub_energy / self.sub_len as f64);
                self.sub_energy = 0.0;
                self.sub_count = 0;
            }
        }
    }

    pub fn finish(self) -> LevelReport {
        let db = |v: f64| 20.0 * (v.max(1e-9) / FULL_SCALE).log10();
        // 400 ms gating blocks with 75% overlap: four consecutive 100 ms sub-blocks.
        let blocks: Vec<f64> = self.subs.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect();
        let lufs = |z: f64| -0.691 + 10.0 * z.max(1e-30).log10();
        let gated: Vec<f64> = blocks.iter().copied().filter(|&z| lufs(z) > -70.0).collect();
        let loudness = if gated.is_empty() {
            f64::NEG_INFINITY
        } else {
            let mean = gated.iter().sum::<f64>() / gated.len() as f64;
            let relative = lufs(mean) - 10.0;
            let kept: Vec<f64> = gated.into_iter().filter(|&z| lufs(z) > relative).collect();
            lufs(kept.iter().sum::<f64>() / kept.len().max(1) as f64)
        };
        LevelReport {
            sample_peak_dbfs: db(self.peak),
            true_peak_dbtp: db(self.true_peak.max(self.peak)),
            loudness_lufs: loudness,
            over_full_scale: self.over,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, amp: f64, sr: u32, secs: f64) -> Vec<(f64, f64)> {
        (0..(secs * sr as f64) as usize)
            .map(|i| {
                let v = amp * FULL_SCALE * (2.0 * PI * freq * i as f64 / sr as f64).sin();
                (v, v)
            })
            .collect()
    }

    #[test]
    fn test_loudness_of_reference_sine() {
        // BS.1770: a 997 Hz sine at -20 dBFS peak in both channels reads about -20 LUFS + 3 dB
        // for the two channels - 3 dB for sine RMS = about -20.1 LUFS; check within 0.3 LU.
        for sr in [44_100, 48_000, 96_000] {
            let mut m = LevelMeter::new(sr);
            m.feed(&sine(997.0, 0.1, sr, 5.0));
            let r = m.finish();
            assert!((r.loudness_lufs - (-20.0)).abs() < 0.3, "{sr}: {} LUFS", r.loudness_lufs);
            assert!((r.sample_peak_dbfs + 20.0).abs() < 0.01);
            assert_eq!(r.over_full_scale, 0);
        }
    }

    #[test]
    fn test_true_peak_catches_inter_sample_peak() {
        // fs/4 sine at 45 degrees: samples at +-0.707, true peak 1.0 (+3 dB above samples).
        let sr = 48_000;
        let frames: Vec<(f64, f64)> = (0..48_000)
            .map(|i| {
                let v = 0.5 * FULL_SCALE * (PI / 2.0 * i as f64 + PI / 4.0).sin();
                (v, v)
            })
            .collect();
        let mut m = LevelMeter::new(sr);
        m.feed(&frames);
        let r = m.finish();
        assert!((r.sample_peak_dbfs - (20.0 * (0.5f64 * 0.7071).log10())).abs() < 0.05);
        assert!((r.true_peak_dbtp - 20.0 * 0.5f64.log10()).abs() < 0.3, "true peak {}", r.true_peak_dbtp);
    }

    #[test]
    fn test_over_full_scale_counted() {
        let mut m = LevelMeter::new(44_100);
        m.feed(&[(FULL_SCALE * 1.2, 0.0), (0.0, -FULL_SCALE * 1.01), (FULL_SCALE * 0.9, 0.0)]);
        assert_eq!(m.finish().over_full_scale, 2);
    }
}
