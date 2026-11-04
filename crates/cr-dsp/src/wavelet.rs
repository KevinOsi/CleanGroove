//! Daubechies D4 two-band filter bank: splits a signal into low and high bands at half the
//! rate, and joins them again.
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 3.1.

use crate::params::round_sym;

const SQRT3: f64 = 1.7320508075688772;
const FOUR_SQRT2: f64 = 5.656854249492381;

/// Low-pass (scaling) taps, oldest input first.
pub const LOW: [f64; 4] = [
    (1.0 - SQRT3) / FOUR_SQRT2,
    (3.0 - SQRT3) / FOUR_SQRT2,
    (3.0 + SQRT3) / FOUR_SQRT2,
    (1.0 + SQRT3) / FOUR_SQRT2,
];

/// High-pass (wavelet) taps: the low-pass taps reversed with alternating signs.
pub const HIGH: [f64; 4] = [LOW[3], -LOW[2], LOW[1], -LOW[0]];

/// Two-band analysis, one pair of input samples at a time; remembers the previous pair.
#[derive(Debug, Clone, Copy, Default)]
pub struct D4Analyzer {
    prev_even: i32,
    prev_odd: i32,
}

impl D4Analyzer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Splits the pair (`even`, `odd`) into one low-band and one high-band sample, rounded half
    /// away from zero.
    #[inline]
    pub fn analyze(&mut self, even: i32, odd: i32) -> (i32, i32) {
        let x = [self.prev_even as f64, self.prev_odd as f64, even as f64, odd as f64];
        let lo = round_sym(LOW[0] * x[0] + LOW[1] * x[1] + LOW[2] * x[2] + LOW[3] * x[3]);
        let hi = round_sym(HIGH[0] * x[0] + HIGH[1] * x[1] + HIGH[2] * x[2] + HIGH[3] * x[3]);
        self.prev_even = even;
        self.prev_odd = odd;
        (lo, hi)
    }
}

/// Joins two consecutive band pairs back into two output samples, truncated toward zero.
#[inline]
fn synthesize_truncated(lo0: i32, hi0: i32, lo1: i32, hi1: i32) -> (i32, i32) {
    let (lo0, hi0, lo1, hi1) = (lo0 as f64, hi0 as f64, lo1 as f64, hi1 as f64);
    let even = (LOW[2] * lo0 + HIGH[2] * hi0 + LOW[0] * lo1 + HIGH[0] * hi1) as i32;
    let odd = (LOW[3] * lo0 + HIGH[3] * hi0 + LOW[1] * lo1 + HIGH[1] * hi1) as i32;
    (even, odd)
}

/// Synthesis as used inside a channel: truncation's downward bias is compensated by adding 1
/// to every result that is zero or positive.
#[inline]
pub fn synthesize_zero_up(lo0: i32, hi0: i32, lo1: i32, hi1: i32) -> (i32, i32) {
    let (even, odd) = synthesize_truncated(lo0, hi0, lo1, hi1);
    (even + (even >= 0) as i32, odd + (odd >= 0) as i32)
}

/// Synthesis as used for the >48 kHz split: adds 1 to every positive result.
#[inline]
pub fn synthesize_positive_up(lo0: i32, hi0: i32, lo1: i32, hi1: i32) -> (i32, i32) {
    let (even, odd) = synthesize_truncated(lo0, hi0, lo1, hi1);
    (even + (even > 0) as i32, odd + (odd > 0) as i32)
}

/// Maps a span of output samples `[start, end)` to the band samples that cover it.
pub fn time_span_to_band_span(span: [isize; 2], is_wavelet: bool) -> ([isize; 2], [isize; 2]) {
    if is_wavelet {
        let start = (span[0] + 1) / 2 + 1;
        let end = span[1] / 2;
        ([start, end], [start, end])
    } else {
        (span, span)
    }
}

/// Maps the low and high bands' spans back to one span of output samples covering both.
pub fn band_spans_to_time_span(lo: [isize; 2], hi: [isize; 2], is_wavelet: bool) -> [isize; 2] {
    if is_wavelet {
        let lo_valid = lo[1] > lo[0];
        let hi_valid = hi[1] > hi[0];
        if lo_valid && hi_valid {
            [2 * (lo[0].min(hi[0]) - 1), 2 * lo[1].max(hi[1])]
        } else if lo_valid {
            [2 * (lo[0] - 1), 2 * lo[1]]
        } else if hi_valid {
            [2 * (hi[0] - 1), 2 * hi[1]]
        } else {
            let s = 2 * (lo[0] - 1);
            [s, s]
        }
    } else {
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_taps_are_orthonormal() {
        let dot = |a: &[f64; 4], b: &[f64; 4]| (0..4).map(|i| a[i] * b[i]).sum::<f64>();
        assert!((dot(&LOW, &LOW) - 1.0).abs() < 1e-14);
        assert!((dot(&HIGH, &HIGH) - 1.0).abs() < 1e-14);
        assert!(dot(&LOW, &HIGH).abs() < 1e-14);
    }

    #[test]
    fn analysis_then_synthesis_reconstructs() {
        let mut analyzer = D4Analyzer::new();
        let input: Vec<(i32, i32)> = (0..512)
            .map(|i| {
                let t = |k: f64| (k * 0.05).sin() * 5_000_000.0;
                (t(2.0 * i as f64) as i32, t(2.0 * i as f64 + 1.0) as i32)
            })
            .collect();

        let mut bands: Vec<(i32, i32)> = input.iter().map(|&(e, o)| analyzer.analyze(e, o)).collect();
        bands.push(analyzer.analyze(0, 0));
        bands.push(analyzer.analyze(0, 0));

        for i in 2..500 {
            let (lo0, hi0) = bands[i];
            let (lo1, hi1) = bands[i + 1];
            let (even, odd) = synthesize_zero_up(lo0, hi0, lo1, hi1);
            assert!((input[i].0 - even).abs() <= 2, "frame {}: {} vs {}", i, input[i].0, even);
            assert!((input[i].1 - odd).abs() <= 2, "frame {}: {} vs {}", i, input[i].1, odd);
        }
    }

    #[test]
    fn spans_map_between_time_and_bands() {
        let (lo, hi) = time_span_to_band_span([100, 150], true);
        assert_eq!((lo, hi), ([51, 75], [51, 75]));
        assert_eq!(band_spans_to_time_span(lo, hi, true), [100, 150]);
    }
}
