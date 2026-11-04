//! Post-declick chain: corrections applied after click repair, in a fixed order.
//!
//! 1. Polarity: flip the right channel when it is inverted.
//! 2. Channel balance: equal and opposite gains so the channels match.
//! 3. Azimuth: a fractional delay split +-d/2 across the two channels, so both get identical
//!    filtering, aligning a cartridge's inter-channel time offset.
//! 4. Subsonic high-pass (zero phase): removes DC, warp and tonearm resonance.
//! 5. Elliptical EQ (stereo): the Side signal is high-passed (zero phase), summing the bass to
//!    mono and removing vertical rumble; Mid is untouched.
//! 6. Mono sum (mono records): both channels replaced by their average.
//! 7. Output gain (the level check's album gain, when the chain pushed peaks over full scale).
//!
//! These run after declicking because the ClickRepair manual's one firm recommendation is to
//! declick the original data before any filtering: filters smear clicks (harder to detect,
//! wider repairs) and clicks make filters ring. The declick engine itself is not changed.
//!
//! Zero-phase filtering needs future samples, so the chain works in blocks with a second of
//! context on each side; the high-pass transients die out well within that, so block edges do
//! not show in the output.

pub mod fft;
pub mod filters;
pub mod levels;
pub mod measure;

use filters::{apply_fir, butterworth_highpass, filtfilt, fractional_delay_taps};
pub use levels::{LevelMeter, LevelReport};

/// Internal full scale (24-bit PCM << 1).
pub const FULL_SCALE: f64 = 16_777_216.0;

/// Order of the subsonic Butterworth design (run forward and backward: 8th-order slopes).
const SUBSONIC_ORDER: usize = 4;
/// Order of the elliptical (Side) Butterworth design (forward and backward: 4th-order slopes).
const ELLIPTICAL_ORDER: usize = 2;
/// Fractional delay filter half length (63 taps) and Kaiser beta.
const DELAY_HALF_TAPS: usize = 31;
const DELAY_KAISER_BETA: f64 = 8.0;
/// Block length and the context processed on each side of it, seconds.
const BLOCK_SECS: f64 = 4.0;
pub const CONTEXT_SECS: f64 = 1.0;

/// What the chain does. `Default` does nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PostSettings {
    pub flip_right_polarity: bool,
    /// Measured left-minus-right level difference to remove, dB.
    pub balance_db: f64,
    /// Measured delay of the right channel behind the left to remove, microseconds.
    pub azimuth_us: f64,
    /// Subsonic high-pass cutoff (-6 dB point), Hz.
    pub subsonic_hz: Option<f64>,
    /// Elliptical EQ: Side high-pass cutoff (-6 dB point), Hz. Ignored when `mono_sum` is on.
    pub elliptical_hz: Option<f64>,
    /// Replace both channels with their average (mono records).
    pub mono_sum: bool,
    /// Output gain, dB.
    pub gain_db: f64,
}

impl PostSettings {
    /// True when the chain would leave the audio untouched.
    pub fn is_identity(&self) -> bool {
        !self.flip_right_polarity
            && self.balance_db == 0.0
            && self.azimuth_us == 0.0
            && self.subsonic_hz.is_none()
            && self.elliptical_hz.is_none()
            && !self.mono_sum
            && self.gain_db == 0.0
    }
}

/// Context frames needed on each side of a block.
pub fn context_frames(sample_rate: u32) -> usize {
    (CONTEXT_SECS * sample_rate as f64) as usize
}

/// Runs the chain on `input`, returning output for `keep` (indices into `input`). Frames of
/// `input` outside `keep` are context only; give at least `context_frames` on each side for
/// the zero-phase filters to settle (less is fine at the start or end of a file).
pub fn process_block(input: &[(i32, i32)], sample_rate: u32, settings: &PostSettings, keep: std::ops::Range<usize>) -> Vec<(f64, f64)> {
    let sr = sample_rate.max(1) as f64;
    let mut l: Vec<f64> = input.iter().map(|f| f.0 as f64).collect();
    let mut r: Vec<f64> = input.iter().map(|f| f.1 as f64).collect();

    if settings.flip_right_polarity {
        r.iter_mut().for_each(|v| *v = -*v);
    }
    if settings.balance_db != 0.0 {
        let gl = 10f64.powf(-settings.balance_db / 40.0);
        let gr = 10f64.powf(settings.balance_db / 40.0);
        l.iter_mut().for_each(|v| *v *= gl);
        r.iter_mut().for_each(|v| *v *= gr);
    }
    if settings.azimuth_us != 0.0 {
        // Right lags left by d: delay the left by d/2 and advance the right by d/2.
        let d = settings.azimuth_us * 1e-6 * sr;
        l = apply_fir(&fractional_delay_taps(d / 2.0, DELAY_HALF_TAPS, DELAY_KAISER_BETA), &l);
        r = apply_fir(&fractional_delay_taps(-d / 2.0, DELAY_HALF_TAPS, DELAY_KAISER_BETA), &r);
    }
    if let Some(fc) = settings.subsonic_hz {
        let hp = butterworth_highpass(fc, SUBSONIC_ORDER, sr);
        filtfilt(&hp, &mut l);
        filtfilt(&hp, &mut r);
    }
    if settings.mono_sum {
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let m = 0.5 * (*a + *b);
            *a = m;
            *b = m;
        }
    } else if let Some(fc) = settings.elliptical_hz {
        let mut side: Vec<f64> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a - b)).collect();
        filtfilt(&butterworth_highpass(fc, ELLIPTICAL_ORDER, sr), &mut side);
        for i in 0..l.len() {
            let mid = 0.5 * (l[i] + r[i]);
            l[i] = mid + side[i];
            r[i] = mid - side[i];
        }
    }
    let g = 10f64.powf(settings.gain_db / 20.0);
    keep.map(|i| (l[i] * g, r[i] * g)).collect()
}

/// Runs the chain over a whole in-memory track block by block, handing each block of output
/// (in order, unclamped, internal domain) to `sink`.
pub fn run_post(frames: &[(i32, i32)], sample_rate: u32, settings: &PostSettings, mut sink: impl FnMut(&[(f64, f64)])) {
    let n = frames.len();
    let block = (BLOCK_SECS * sample_rate as f64) as usize;
    let ctx = context_frames(sample_rate);
    let mut start = 0;
    while start < n {
        let end = (start + block).min(n);
        let lo = start.saturating_sub(ctx);
        let hi = (end + ctx).min(n);
        let out = process_block(&frames[lo..hi], sample_rate, settings, (start - lo)..(end - lo));
        sink(&out);
        start = end;
    }
}

/// Rounds and clamps an internal-domain sample to the integer range the writers accept.
pub fn to_internal(v: f64) -> i32 {
    v.round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32
}

/// Output gain that brings the sample peak back under full scale (to -0.1 dBFS) when the chain
/// pushed it over; 0 dB otherwise. One gain for the whole album: no limiting or compression.
pub fn album_gain_db(report: &LevelReport) -> f64 {
    if report.over_full_scale > 0 || report.sample_peak_dbfs > 0.0 {
        -0.1 - report.sample_peak_dbfs
    } else {
        0.0
    }
}

/// Measures the chain's output (pass 1), picks the album gain, and returns the settings to
/// render with (gain included) and the level report the rendered output will have.
pub fn plan_levels(frames: &[(i32, i32)], sample_rate: u32, settings: &PostSettings) -> (PostSettings, LevelReport) {
    let mut meter = LevelMeter::new(sample_rate);
    let mut unity = settings.clone();
    unity.gain_db = 0.0;
    run_post(frames, sample_rate, &unity, |b| meter.feed(b));
    let measured = meter.finish();
    let gain = album_gain_db(&measured);
    let mut out = unity;
    out.gain_db = gain;
    (out, measured.with_gain(gain, if gain < 0.0 { 0 } else { measured.over_full_scale }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 11) as f64 / (1u64 << 53) as f64) - 0.5
            })
            .collect()
    }

    fn collect(frames: &[(i32, i32)], sr: u32, s: &PostSettings) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        run_post(frames, sr, s, |b| out.extend_from_slice(b));
        out
    }

    #[test]
    fn test_identity_settings_pass_through() {
        let frames: Vec<(i32, i32)> = noise(50_000, 1).iter().zip(noise(50_000, 2)).map(|(a, b)| ((a * 1e6) as i32, (b * 1e6) as i32)).collect();
        let s = PostSettings::default();
        assert!(s.is_identity());
        let out = collect(&frames, 44_100, &s);
        assert_eq!(out.len(), frames.len());
        assert!(out.iter().zip(&frames).all(|(o, f)| to_internal(o.0) == f.0 && to_internal(o.1) == f.1));
    }

    #[test]
    fn test_blocks_match_whole_signal_processing() {
        // Block-wise zero-phase filtering equals filtering the whole signal at once.
        let sr = 44_100;
        let n = 11 * sr as usize;
        let frames: Vec<(i32, i32)> = noise(n, 3).iter().zip(noise(n, 4)).map(|(a, b)| ((a * 4e6) as i32, (b * 4e6) as i32)).collect();
        let s = PostSettings { subsonic_hz: Some(18.0), elliptical_hz: Some(100.0), azimuth_us: 5.0, ..Default::default() };
        let blocks = collect(&frames, sr, &s);
        let whole = process_block(&frames, sr, &s, 0..n);
        let worst = blocks.iter().zip(&whole).map(|(a, b)| (a.0 - b.0).abs().max((a.1 - b.1).abs())).fold(0.0, f64::max);
        assert!(worst < 2.0, "block edge error {worst} (internal units, 1 = 1/16.7M of full scale)");
    }

    #[test]
    fn test_polarity_balance_and_mono_sum() {
        let sr = 44_100;
        let n = 20_000;
        let x = noise(n, 5);
        // Right is inverted and 2 dB quieter.
        let g = 10f64.powf(-2.0 / 20.0);
        let frames: Vec<(i32, i32)> = x.iter().map(|v| ((v * 4e6) as i32, (-v * 4e6 * g) as i32)).collect();
        let s = PostSettings { flip_right_polarity: true, balance_db: 2.0, mono_sum: true, ..Default::default() };
        let out = collect(&frames, sr, &s);
        // After the flip and balance both channels equal the source at -1 dB; the sum keeps that.
        let target = 10f64.powf(-1.0 / 20.0);
        for (i, (l, r)) in out.iter().enumerate().skip(10).take(n - 20) {
            assert_eq!(l, r);
            assert!((l - x[i] * 4e6 * target).abs() < 2.0, "frame {i}");
        }
    }

    #[test]
    fn test_elliptical_removes_side_bass_only() {
        let sr = 44_100;
        let n = 6 * sr as usize;
        let tone = |f: f64, i: usize| (2.0 * PI * f * i as f64 / sr as f64).sin();
        // 40 Hz out of phase (vertical rumble) plus 1 kHz stereo content.
        let frames: Vec<(i32, i32)> = (0..n)
            .map(|i| {
                let rumble = 1e6 * tone(40.0, i);
                let music = 1e6 * tone(1000.0, i);
                ((rumble + music) as i32, (-rumble + 0.5 * music) as i32)
            })
            .collect();
        let s = PostSettings { elliptical_hz: Some(150.0), ..Default::default() };
        let out = collect(&frames, sr, &s);
        let mid = 2 * sr as usize..4 * sr as usize;
        let side_40: f64 = mid.clone().map(|i| 0.5 * (out[i].0 - out[i].1) * tone(40.0, i)).sum::<f64>() * 2.0 / mid.len() as f64;
        let side_1k: f64 = mid.clone().map(|i| 0.5 * (out[i].0 - out[i].1) * tone(1000.0, i)).sum::<f64>() * 2.0 / mid.len() as f64;
        assert!(side_40.abs() < 0.03 * 1e6, "40 Hz side left: {side_40}");
        assert!((side_1k - 0.25e6).abs() < 0.01e6, "1 kHz side kept: {side_1k}");
    }

    #[test]
    fn test_plan_levels_pulls_overs_back_under_full_scale() {
        let sr = 44_100;
        // A square-ish wave at full scale: the subsonic filter's overshoot pushes it over.
        let frames: Vec<(i32, i32)> = (0..3 * sr as usize)
            .map(|i| {
                let v = if (i / 2205) % 2 == 0 { 16_700_000 } else { -16_700_000 };
                (v, v)
            })
            .collect();
        let s = PostSettings { subsonic_hz: Some(18.0), ..Default::default() };
        let (planned, report) = plan_levels(&frames, sr, &s);
        assert!(planned.gain_db < 0.0, "gain {}", planned.gain_db);
        assert!((report.sample_peak_dbfs + 0.1).abs() < 1e-6);
        let mut over = 0;
        run_post(&frames, sr, &planned, |b| over += b.iter().filter(|(l, r)| l.abs() >= FULL_SCALE || r.abs() >= FULL_SCALE).count());
        assert_eq!(over, 0);
    }
}
