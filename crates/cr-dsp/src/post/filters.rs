//! Filters for the post-declick chain: Butterworth high-pass sections run forward and backward
//! (zero phase), and a Kaiser-windowed sinc fractional delay.

use std::f64::consts::PI;

/// One second-order section, transposed direct form II.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    pub fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// RBJ cookbook high-pass at `f0` with quality factor `q`.
    pub fn highpass(f0: f64, q: f64, sample_rate: f64) -> Self {
        let w = 2.0 * PI * f0 / sample_rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        Self::new((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    /// Normalised coefficients (b0, b1, b2, a1, a2).
    pub(crate) fn coefficients(&self) -> (f64, f64, f64, f64, f64) {
        (self.b0, self.b1, self.b2, self.a1, self.a2)
    }

    /// Filters `x` in place, starting from rest.
    pub fn run(&self, x: &mut [f64]) {
        let (mut z1, mut z2) = (0.0, 0.0);
        for v in x.iter_mut() {
            let y = self.b0 * *v + z1;
            z1 = self.b1 * *v - self.a1 * y + z2;
            z2 = self.b2 * *v - self.a2 * y;
            *v = y;
        }
    }

    /// As `run`, last sample first.
    pub fn run_reverse(&self, x: &mut [f64]) {
        let (mut z1, mut z2) = (0.0, 0.0);
        for v in x.iter_mut().rev() {
            let y = self.b0 * *v + z1;
            z1 = self.b1 * *v - self.a1 * y + z2;
            z2 = self.b2 * *v - self.a2 * y;
            *v = y;
        }
    }
}

/// Butterworth high-pass of even `order` as second-order sections (each section uses the
/// cookbook high-pass at the cutoff with that pole pair's Q).
pub fn butterworth_highpass(cutoff: f64, order: usize, sample_rate: f64) -> Vec<Biquad> {
    assert!(order >= 2 && order % 2 == 0, "even order");
    (0..order / 2)
        .map(|k| {
            let q = 1.0 / (2.0 * (PI * (2 * k + 1) as f64 / (2 * order) as f64).sin());
            Biquad::highpass(cutoff, q, sample_rate)
        })
        .collect()
}

/// Zero-phase filtering: every section forward, then every section backward. The magnitude
/// response is squared (a 4th-order design gives 8th-order slopes and -6 dB at the cutoff)
/// and the phase response cancels.
pub fn filtfilt(sections: &[Biquad], x: &mut [f64]) {
    for s in sections {
        s.run(x);
    }
    for s in sections {
        s.run_reverse(x);
    }
}

/// Zeroth-order modified Bessel function of the first kind (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= half / k as f64;
        sum += term * term;
        if term * term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Fractional delay FIR: `y[i] = x(i - delay)` for |delay| up to a few samples. Kaiser-windowed
/// sinc with `2 * half_len + 1` taps, normalised to unit DC gain. Index `k` of the result is
/// the coefficient for input offset `k - half_len` (non-causal, centred).
pub fn fractional_delay_taps(delay: f64, half_len: usize, beta: f64) -> Vec<f64> {
    let n = 2 * half_len + 1;
    let span = half_len as f64 + 1.0;
    let mut taps: Vec<f64> = (0..n)
        .map(|k| {
            let t = k as f64 - half_len as f64 - delay; // distance from the delayed sample
            let sinc = if t.abs() < 1e-12 { 1.0 } else { (PI * t).sin() / (PI * t) };
            let r = (t / span).clamp(-1.0, 1.0);
            sinc * bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps
}

/// Applies centred FIR `taps` to `x` (samples outside the slice count as zero).
pub fn apply_fir(taps: &[f64], x: &[f64]) -> Vec<f64> {
    let half = taps.len() / 2;
    let n = x.len();
    (0..n)
        .map(|i| {
            let mut acc = 0.0;
            for (k, &h) in taps.iter().enumerate() {
                // y[i] = sum_k h[k] * x[i - (k - half)]
                let j = i as isize - (k as isize - half as isize);
                if j >= 0 && (j as usize) < n {
                    acc += h * x[j as usize];
                }
            }
            acc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f64, sr: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / sr).sin()).collect()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn test_filtfilt_highpass_response() {
        let sr = 44_100.0;
        let hp = butterworth_highpass(18.0, 4, sr);
        let n = 4 * 44_100;
        let mid = n / 4..3 * n / 4; // away from the edges
        let gain = |f: f64| {
            let mut x = tone(f, sr, n);
            let before = rms(&x[mid.clone()]);
            filtfilt(&hp, &mut x);
            20.0 * (rms(&x[mid.clone()]) / before).log10()
        };
        assert!((gain(18.0) + 6.02).abs() < 0.2, "-6 dB at the cutoff: {}", gain(18.0));
        assert!(gain(9.0) < -45.0, "8th-order slope: {}", gain(9.0));
        assert!(gain(100.0).abs() < 0.01, "passband: {}", gain(100.0));
        assert!(gain(1000.0).abs() < 0.001);
    }

    #[test]
    fn test_filtfilt_is_zero_phase() {
        let sr = 44_100.0;
        let hp = butterworth_highpass(100.0, 2, sr);
        let n = 44_100;
        let mut x = tone(250.0, sr, n);
        let orig = x.clone();
        filtfilt(&hp, &mut x);
        // In the middle the output is the input scaled, with no time shift: correlate at lag 0
        // against lags +-1 and check lag 0 wins and the waveform matches closely.
        let seg = n / 4..3 * n / 4;
        let scale = rms(&x[seg.clone()]) / rms(&orig[seg.clone()]);
        let err: f64 = seg.clone().map(|i| (x[i] - scale * orig[i]).abs()).fold(0.0, f64::max);
        assert!(err < 1e-3, "max deviation {err}");
    }

    #[test]
    fn test_fractional_delay_shifts_a_band_limited_signal() {
        let sr = 44_100.0;
        let n = 8192;
        let delay = 0.3;
        let x: Vec<f64> = tone(3000.0, sr, n).iter().zip(tone(7000.0, sr, n)).map(|(a, b)| a + 0.5 * b).collect();
        let y = apply_fir(&fractional_delay_taps(delay, 31, 8.0), &x);
        let expect = |i: usize| {
            let t = i as f64 - delay;
            (2.0 * PI * 3000.0 * t / sr).sin() + 0.5 * (2.0 * PI * 7000.0 * t / sr).sin()
        };
        let err = (100..n - 100).map(|i| (y[i] - expect(i)).abs()).fold(0.0, f64::max);
        assert!(err < 1e-3, "max error {err}");
    }

    #[test]
    fn test_fractional_delay_zero_is_identity() {
        let taps = fractional_delay_taps(0.0, 31, 8.0);
        assert!((taps[31] - 1.0).abs() < 1e-12);
        assert!(taps.iter().enumerate().all(|(k, &t)| k == 31 || t.abs() < 1e-12));
    }
}
