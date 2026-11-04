//! Level envelope for finding the quiet between tracks.
//!
//! The level is measured above the rumble band: lead-in and run-out grooves carry rumble at
//! -28 to -43 dBFS (one rip), loud enough to hide them from a plain level, while above about
//! 120 Hz they are as quiet as any other gap. A moving median then removes single clicks, which
//! would otherwise break a gap into pieces.

use cr_dsp::params::internal_to_f64;

/// Envelope hop, seconds.
pub const HOP_S: f64 = 0.010;
/// High-pass corner that keeps rumble out of the level, Hz.
pub const HIGHPASS_HZ: f64 = 120.0;
/// Moving-median width, in hops (310 ms).
const MEDIAN_HOPS: usize = 31;
/// Level floor, dBFS: digital silence reads this instead of minus infinity.
const FLOOR_DB: f32 = -120.0;

/// Level of the mid signal above the rumble band, one value per `HOP_S`, median-smoothed.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub sample_rate: u32,
    /// Frames per hop.
    pub hop: usize,
    pub db: Vec<f32>,
}

impl Envelope {
    /// From frames in the internal sample domain.
    pub fn from_frames(frames: &[(i32, i32)], sample_rate: u32) -> Self {
        let hop = ((sample_rate as f64 * HOP_S).round() as usize).max(1);
        let mut hp = Biquad::highpass(HIGHPASS_HZ, sample_rate as f64);
        let mut raw = Vec::with_capacity(frames.len() / hop + 1);
        for chunk in frames.chunks(hop) {
            let mut sum = 0.0f64;
            for &(l, r) in chunk {
                let y = hp.run(0.5 * (internal_to_f64(l) + internal_to_f64(r)));
                sum += y * y;
            }
            let rms = (sum / chunk.len() as f64).sqrt();
            raw.push(if rms > 0.0 { ((20.0 * rms.log10()) as f32).max(FLOOR_DB) } else { FLOOR_DB });
        }
        Envelope { sample_rate, hop, db: moving_median(&raw, MEDIAN_HOPS) }
    }

    pub fn len(&self) -> usize {
        self.db.len()
    }

    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    pub fn hops_per_s(&self) -> f64 {
        1.0 / HOP_S
    }

    pub fn hop_to_frame(&self, h: usize) -> u64 {
        (h * self.hop) as u64
    }

    pub fn frame_to_hop(&self, f: u64) -> usize {
        (f as usize / self.hop).min(self.db.len().saturating_sub(1))
    }

    pub fn s_to_hops(&self, s: f64) -> usize {
        (s / HOP_S).round().max(0.0) as usize
    }

    /// The level below which a hop counts as quiet: 10 dB above the noise floor (the 3rd
    /// percentile, as gaps, lead-in and run-out are a few percent of an LP), but never closer
    /// than 20 dB to the median, so quiet music on a noisy record stays music.
    pub fn quiet_threshold(&self) -> f32 {
        if self.db.is_empty() {
            return FLOOR_DB;
        }
        let mut v = self.db.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        (pct(0.03) + 10.0).min(pct(0.5) - 20.0)
    }

    /// Runs of quiet hops (`start..end`, in hops) at least `min_hops` long.
    pub fn quiet_runs(&self, threshold: f32, min_hops: usize) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut start = None;
        for (h, &d) in self.db.iter().chain(std::iter::once(&f32::INFINITY)).enumerate() {
            match (d < threshold, start) {
                (true, None) => start = Some(h),
                (false, Some(s)) => {
                    if h - s >= min_hops {
                        out.push(s..h);
                    }
                    start = None;
                }
                _ => {}
            }
        }
        out
    }
}

fn moving_median(x: &[f32], width: usize) -> Vec<f32> {
    let half = width / 2;
    let mut buf = Vec::with_capacity(width);
    (0..x.len())
        .map(|i| {
            buf.clear();
            buf.extend_from_slice(&x[i.saturating_sub(half)..(i + half + 1).min(x.len())]);
            buf.sort_by(|a, b| a.partial_cmp(b).unwrap());
            buf[buf.len() / 2]
        })
        .collect()
}

/// Second-order Butterworth high-pass (bilinear transform).
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn highpass(fc: f64, fs: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * fc / fs;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        Biquad {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}
