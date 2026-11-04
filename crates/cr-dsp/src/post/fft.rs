//! Minimal in-place radix-2 complex FFT, enough for the azimuth measurement.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }
    pub fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }
    pub fn mul(self, o: Self) -> Self {
        Self::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    pub fn norm(self) -> f64 {
        (self.re * self.re + self.im * self.im).sqrt()
    }
    pub fn scale(self, s: f64) -> Self {
        Self::new(self.re * s, self.im * s)
    }
}

/// In-place FFT (`inverse` = true for the inverse transform, scaled by 1/n). `x.len()` must be
/// a power of two.
pub fn fft(x: &mut [Complex], inverse: bool) {
    let n = x.len();
    assert!(n.is_power_of_two(), "FFT length must be a power of two");
    // Bit-reversal permutation.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            x.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let ang = sign * 2.0 * PI / len as f64;
        let w_len = Complex::new(ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let mut w = Complex::new(1.0, 0.0);
            for k in 0..len / 2 {
                let u = x[start + k];
                let v = x[start + k + len / 2].mul(w);
                x[start + k] = Complex::new(u.re + v.re, u.im + v.im);
                x[start + k + len / 2] = Complex::new(u.re - v.re, u.im - v.im);
                w = w.mul(w_len);
            }
        }
        len <<= 1;
    }
    if inverse {
        let s = 1.0 / n as f64;
        for v in x.iter_mut() {
            *v = v.scale(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fft_round_trip_and_tone_bin() {
        let n = 64;
        let mut x: Vec<Complex> = (0..n).map(|i| Complex::new((2.0 * PI * 5.0 * i as f64 / n as f64).cos(), 0.0)).collect();
        let orig = x.clone();
        fft(&mut x, false);
        assert!((x[5].norm() - n as f64 / 2.0).abs() < 1e-9);
        assert!(x[7].norm() < 1e-9);
        fft(&mut x, true);
        for (a, b) in x.iter().zip(&orig) {
            assert!((a.re - b.re).abs() < 1e-12 && a.im.abs() < 1e-12);
        }
    }
}
