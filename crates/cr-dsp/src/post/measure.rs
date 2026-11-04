//! Measurements that decide the post-declick corrections: inter-channel balance and time
//! offset (azimuth), and low-frequency rumble in Mid and Side.
//!
//! Each is taken over many short windows and summarised by the median, with the spread
//! (interquartile range) reported so a correction is only suggested when the windows agree.

use super::fft::{fft, Complex};
use super::filters::{butterworth_highpass, filtfilt};
use super::FULL_SCALE;

/// A region of the track to measure: `start..start + len`.
pub type Window = (usize, usize);

/// GCC-PHAT band, Hz: high enough to be above most bass (where room and mic spacing dominate),
/// low enough to be below the treble where surface noise dominates.
const PHAT_LO_HZ: f64 = 1000.0;
const PHAT_HI_HZ: f64 = 8000.0;
/// Analysis segment and interpolation factor for the cross-correlation peak.
const PHAT_SEGMENT: usize = 8192;
const PHAT_UPSAMPLE: usize = 8;
/// Largest offset searched, microseconds (a misaligned cartridge gives a few to tens).
const PHAT_MAX_US: f64 = 100.0;
/// Windows whose correlation peak is weaker than this are not trusted. On real rips the
/// per-window scatter roughly halves between 0.15 and 0.5 (Sinatra, mono: interquartile range
/// 8.7 -> 5.9 us), while about three quarters of the windows still qualify.
const PHAT_MIN_CONFIDENCE: f64 = 0.5;
/// High-pass before the balance measurement (keeps rumble out of it), Hz.
const BALANCE_HP_HZ: f64 = 150.0;
/// Low band for the rumble measurements, Hz.
const RUMBLE_HZ: f64 = 150.0;
const SUBSONIC_HZ: f64 = 18.0;

/// Median and interquartile range of `v` (NaN for an empty list).
pub fn median_iqr(v: &mut [f64]) -> (f64, f64) {
    if v.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| {
        let x = p * (v.len() - 1) as f64;
        let (i, f) = (x.floor() as usize, x.fract());
        v[i] + (v[(i + 1).min(v.len() - 1)] - v[i]) * f
    };
    (q(0.5), q(0.75) - q(0.25))
}

#[derive(Debug, Clone, Default)]
pub struct ChannelMeasurements {
    /// Median left-minus-right level above 150 Hz, dB, and its interquartile range.
    pub balance_db: f64,
    pub balance_spread_db: f64,
    /// Median delay of the right channel behind the left, microseconds, and its interquartile
    /// range over the trusted windows.
    pub azimuth_us: f64,
    pub azimuth_spread_us: f64,
    /// Median delay over the first and the second half of the track (sides differ when the
    /// offset was cut into the record rather than caused by the cartridge).
    pub azimuth_halves_us: (f64, f64),
    pub azimuth_windows: usize,
    /// Standard error of the azimuth median, microseconds (scatter / sqrt(windows)): how
    /// precisely the offset is known, even when single windows scatter by several us.
    pub azimuth_se_us: f64,
    /// Rumble below 150 Hz in the silence between tracks, dBFS RMS: Side (vertical) and Mid.
    pub side_rumble_gap_db: f64,
    pub mid_rumble_gap_db: f64,
    /// Side below 150 Hz in the music, dBFS RMS (what elliptical EQ would remove there).
    pub side_lf_music_db: f64,
    /// Below 18 Hz (warp, tonearm resonance, DC) in the music, dBFS RMS, Mid and Side together.
    pub subsonic_music_db: f64,
}

fn db_rms(sum_sq: f64, n: usize) -> f64 {
    20.0 * ((sum_sq / n.max(1) as f64).sqrt().max(1e-9) / FULL_SCALE).log10()
}

/// Delay of `r` behind `l` in samples by GCC-PHAT over `PHAT_LO_HZ..PHAT_HI_HZ`, with the
/// normalised correlation peak as confidence.
pub fn gcc_phat_delay(l: &[f64], r: &[f64], sample_rate: f64) -> (f64, f64) {
    let n = l.len().min(r.len()).next_power_of_two();
    let hann = |i: usize, len: usize| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / len as f64).cos();
    let mut x: Vec<Complex> = (0..n).map(|i| Complex::new(if i < l.len() { l[i] * hann(i, l.len()) } else { 0.0 }, 0.0)).collect();
    let mut y: Vec<Complex> = (0..n).map(|i| Complex::new(if i < r.len() { r[i] * hann(i, r.len()) } else { 0.0 }, 0.0)).collect();
    fft(&mut x, false);
    fft(&mut y, false);

    // Cross-spectrum conj(X) Y gives c[t] = sum l[i] r[i + t]: its peak is at t = delay of r.
    // PHAT keeps only phase; bins outside the band are zeroed. Zero-padding the spectrum to
    // PHAT_UPSAMPLE times the length interpolates the correlation.
    let m = n * PHAT_UPSAMPLE;
    let mut g = vec![Complex::default(); m];
    let lo = (PHAT_LO_HZ / sample_rate * n as f64).ceil() as usize;
    let hi = ((PHAT_HI_HZ / sample_rate * n as f64).floor() as usize).min(n / 2 - 1);
    let mut bins = 0usize;
    for k in lo..=hi {
        let c = x[k].conj().mul(y[k]);
        let mag = c.norm();
        if mag > 0.0 {
            let p = c.scale(1.0 / mag);
            g[k] = p;
            g[m - k] = p.conj();
            bins += 1;
        }
    }
    fft(&mut g, true);
    // Normalise so a perfect match peaks at 1.
    let norm = m as f64 / (2.0 * bins.max(1) as f64);
    let max_lag = (PHAT_MAX_US * 1e-6 * sample_rate * PHAT_UPSAMPLE as f64).ceil() as isize;
    let at = |t: isize| g[t.rem_euclid(m as isize) as usize].re * norm;
    let (mut best_t, mut best) = (0isize, f64::NEG_INFINITY);
    for t in -max_lag..=max_lag {
        if at(t) > best {
            best = at(t);
            best_t = t;
        }
    }
    // Parabolic refinement around the peak.
    let (a, b, c) = (at(best_t - 1), best, at(best_t + 1));
    let denom = a - 2.0 * b + c;
    let frac = if denom.abs() > 1e-12 { 0.5 * (a - c) / denom } else { 0.0 };
    ((best_t as f64 + frac) / PHAT_UPSAMPLE as f64, best)
}

/// Measures balance, azimuth and rumble. `music` and `gaps` are one-second windows.
pub fn measure_channels(frames: &[(i32, i32)], sample_rate: u32, music: &[Window], gaps: &[Window]) -> ChannelMeasurements {
    let sr = sample_rate.max(1) as f64;
    let total = frames.len();
    let split = |w: &Window| -> (Vec<f64>, Vec<f64>) {
        let s = &frames[w.0.min(total)..(w.0 + w.1).min(total)];
        (s.iter().map(|f| f.0 as f64).collect(), s.iter().map(|f| f.1 as f64).collect())
    };

    // Balance above 150 Hz.
    let hp_bal = butterworth_highpass(BALANCE_HP_HZ, 2, sr);
    let mut balances: Vec<f64> = music
        .iter()
        .filter_map(|w| {
            let (mut l, mut r) = split(w);
            filtfilt(&hp_bal, &mut l);
            filtfilt(&hp_bal, &mut r);
            let el: f64 = l.iter().map(|v| v * v).sum();
            let er: f64 = r.iter().map(|v| v * v).sum();
            (el > 0.0 && er > 0.0).then(|| 10.0 * (el / er).log10())
        })
        .collect();
    let (balance_db, balance_spread_db) = median_iqr(&mut balances);

    // Azimuth: a centred segment of each music window.
    let seg = PHAT_SEGMENT.min(sample_rate as usize);
    let mut trusted: Vec<(usize, f64)> = Vec::new();
    for w in music {
        let start = w.0 + w.1.saturating_sub(seg) / 2;
        let (l, r) = split(&(start, seg));
        if l.len() < seg {
            continue;
        }
        let (delay, confidence) = gcc_phat_delay(&l, &r, sr);
        if confidence >= PHAT_MIN_CONFIDENCE {
            trusted.push((start, delay / sr * 1e6));
        }
    }
    let mut all: Vec<f64> = trusted.iter().map(|t| t.1).collect();
    let (azimuth_us, azimuth_spread_us) = median_iqr(&mut all);
    let mut first: Vec<f64> = trusted.iter().filter(|t| t.0 < total / 2).map(|t| t.1).collect();
    let mut second: Vec<f64> = trusted.iter().filter(|t| t.0 >= total / 2).map(|t| t.1).collect();
    let azimuth_halves_us = (median_iqr(&mut first).0, median_iqr(&mut second).0);

    // Rumble: the low band is the input minus its zero-phase high-pass.
    let hp_rumble = butterworth_highpass(RUMBLE_HZ, 4, sr);
    let hp_sub = butterworth_highpass(SUBSONIC_HZ, 4, sr);
    let low_band = |x: &[f64], hp: &[filters_alias::Biquad]| -> f64 {
        let mut y = x.to_vec();
        filtfilt(hp, &mut y);
        x.iter().zip(&y).map(|(a, b)| (a - b) * (a - b)).sum()
    };
    let ms = |w: &Window| {
        let (l, r) = split(w);
        let mid: Vec<f64> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)).collect();
        let side: Vec<f64> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a - b)).collect();
        (mid, side)
    };
    let (mut gs, mut gm, mut gn) = (0.0, 0.0, 0usize);
    for w in gaps {
        let (mid, side) = ms(w);
        gs += low_band(&side, &hp_rumble);
        gm += low_band(&mid, &hp_rumble);
        gn += mid.len();
    }
    let (mut ms_side, mut ms_sub, mut mn) = (0.0, 0.0, 0usize);
    for w in music {
        let (mid, side) = ms(w);
        ms_side += low_band(&side, &hp_rumble);
        ms_sub += low_band(&mid, &hp_sub) + low_band(&side, &hp_sub);
        mn += mid.len();
    }

    ChannelMeasurements {
        balance_db,
        balance_spread_db,
        azimuth_us,
        azimuth_spread_us,
        azimuth_halves_us,
        azimuth_windows: trusted.len(),
        // Median standard error for roughly normal scatter: 1.2533 * sigma / sqrt(n), with
        // sigma estimated as IQR / 1.349.
        azimuth_se_us: 1.2533 * (azimuth_spread_us / 1.349) / (trusted.len().max(1) as f64).sqrt(),
        side_rumble_gap_db: if gn > 0 { db_rms(gs, gn) } else { f64::NAN },
        mid_rumble_gap_db: if gn > 0 { db_rms(gm, gn) } else { f64::NAN },
        side_lf_music_db: db_rms(ms_side, mn),
        subsonic_music_db: db_rms(ms_sub, mn),
    }
}

use super::filters as filters_alias;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post::filters::{apply_fir, fractional_delay_taps};

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 11) as f64 / (1u64 << 53) as f64) - 0.5
            })
            .collect()
    }

    #[test]
    fn test_gcc_phat_finds_sub_sample_delay() {
        let sr = 44_100.0;
        let x = noise(8192, 7);
        for delay_us in [-12.0, -3.0, 0.0, 2.5, 4.5, 20.0] {
            let d = delay_us * 1e-6 * sr;
            let y = apply_fir(&fractional_delay_taps(d, 31, 8.0), &x);
            let (got, conf) = gcc_phat_delay(&x, &y, sr);
            let got_us = got / sr * 1e6;
            assert!((got_us - delay_us).abs() < 0.5, "{delay_us} us measured as {got_us:.2}");
            assert!(conf > 0.5, "confidence {conf}");
        }
    }

    #[test]
    fn test_measure_channels_balance_azimuth_rumble() {
        let sr = 44_100u32;
        let n = 40 * sr as usize;
        let src = noise(n, 9);
        let d = 6.0e-6 * sr as f64; // right lags 6 us
        let delayed = apply_fir(&fractional_delay_taps(d, 31, 8.0), &src);
        let g = 10f64.powf(-1.5 / 20.0); // right 1.5 dB quieter
        let t = |i: usize| i as f64 / sr as f64;
        let frames: Vec<(i32, i32)> = (0..n)
            .map(|i| {
                // Gaps (seconds 30..40) hold only vertical (out-of-phase) 30 Hz rumble.
                let music = if i < 30 * sr as usize { 4e6 } else { 0.0 };
                let rumble = 2e4 * (2.0 * std::f64::consts::PI * 30.0 * t(i)).sin();
                ((music * src[i] + rumble) as i32, (music * g * delayed[i] - rumble) as i32)
            })
            .collect();
        let music: Vec<Window> = (1..29).map(|s| (s * sr as usize, sr as usize)).collect();
        let gaps: Vec<Window> = (31..39).map(|s| (s * sr as usize, sr as usize)).collect();
        let m = measure_channels(&frames, sr, &music, &gaps);
        // The test delay filter trims a little of the white noise near Nyquist: about +0.07 dB.
        assert!((m.balance_db - 1.5).abs() < 0.1, "balance {}", m.balance_db);
        assert!(m.balance_spread_db < 0.1);
        assert!((m.azimuth_us - 6.0).abs() < 0.5, "azimuth {}", m.azimuth_us);
        assert_eq!(m.azimuth_windows, 28);
        assert!(m.azimuth_se_us < 0.5);
        // 2e4 peak sine -> RMS 1.41e4 in Side, nothing in Mid.
        let expect = 20.0 * (2e4 / 2f64.sqrt() / FULL_SCALE).log10();
        assert!((m.side_rumble_gap_db - expect).abs() < 0.5, "side rumble {} vs {expect}", m.side_rumble_gap_db);
        assert!(m.mid_rumble_gap_db < expect - 30.0);
    }
}
