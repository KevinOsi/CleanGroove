//! Album analysis on synthetic records: groove noise, music, silence between tracks, and
//! clicks injected at a known density.

use std::sync::atomic::AtomicBool;

use cr_dsp::analysis::{analyze_album, AlbumAnalysis};
use cr_dsp::autotune::{autotune_audio, autotune_audio_with_analysis, RecordCondition};

const SR: u32 = 44_100;
const FULL_SCALE: f64 = 16_777_216.0;

struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn noise(&mut self) -> f64 {
        (self.next_f64() + self.next_f64() + self.next_f64() + self.next_f64() - 2.0) * 1.732
    }
}

fn db(level: f64) -> f64 {
    FULL_SCALE * 10f64.powf(level / 20.0)
}

/// 3 s lead-in, then music / gap / music / gap / music (12 / 4 / 12 / 4 / 10 s), groove noise at
/// -60 dBFS throughout, clicks at `clicks_per_sec` (both channels together), peaks -34..-22 dBFS.
fn record(clicks_per_sec: f64, seed: u64) -> Vec<(i32, i32)> {
    record_at(clicks_per_sec, seed, -20.0)
}

/// As `record`, with the music tones at `music_db`.
fn record_at(clicks_per_sec: f64, seed: u64, music_db: f64) -> Vec<(i32, i32)> {
    record_with(clicks_per_sec, seed, music_db, -34.0)
}

/// As `record_at`, with click peaks between `click_db` and `click_db + 12`.
fn record_with(clicks_per_sec: f64, seed: u64, music_db: f64, click_db: f64) -> Vec<(i32, i32)> {
    let sr = SR as usize;
    let n = 45 * sr;
    let segments = [(3, 15), (19, 31), (35, 45)];
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / SR as f64;
        let (mut l, mut r) = (db(-60.0) * rng.noise(), db(-60.0) * rng.noise());
        if segments.iter().any(|&(a, b)| i >= a * sr && i < b * sr) {
            let swell = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 0.25 * t).sin();
            l += db(music_db) * swell * ((2.0 * std::f64::consts::PI * 330.0 * t).sin() + 0.5 * (2.0 * std::f64::consts::PI * 1210.0 * t).sin());
            r += db(music_db) * swell * ((2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.5 * (2.0 * std::f64::consts::PI * 990.0 * t).sin());
        }
        out.push((l as i32, r as i32));
    }
    let shape = [0.6, 1.0, -0.5, 0.2];
    let count = (clicks_per_sec * n as f64 / SR as f64).round() as usize;
    if count > 0 {
        let spacing = n / count;
        for k in 0..count {
            let pos = k * spacing + (rng.next_f64() * (spacing - 8) as f64) as usize;
            let peak = db(click_db + 12.0 * rng.next_f64()) * if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            let left = rng.next_f64() < 0.5;
            for (j, s) in shape.iter().enumerate() {
                let v = (s * peak) as i32;
                let f = &mut out[pos + j];
                if left { f.0 = f.0.saturating_add(v) } else { f.1 = f.1.saturating_add(v) }
            }
        }
    }
    out
}

fn analyze(frames: &[(i32, i32)]) -> AlbumAnalysis {
    analyze_album(frames, SR, 2, |_, _| {}, &AtomicBool::new(false)).expect("not cancelled")
}

#[test]
fn test_analysis_measures_click_density_from_all_gaps() {
    let a = analyze(&record(16.0, 2));
    assert!(!a.condition_estimated);
    // Gap blocks: lead-in second 2, then 15-18 and 31-34.
    assert_eq!(a.gap_blocks, vec![2, 15, 16, 17, 18, 31, 32, 33, 34]);
    let err = (a.audible_clicks_per_sec - 16.0).abs() / 16.0;
    assert!(err < 0.2, "measured {:.2} audible clicks/s for 16 injected", a.audible_clicks_per_sec);
    assert_eq!(a.condition, RecordCondition::GoodPlus);
    assert_eq!(a.block_db.len(), 45);
    assert!(a.events.windows(2).all(|w| w[0].frame <= w[1].frame), "events sorted");
    assert!(a.repair_fraction > 0.0 && a.repair_fraction < 0.01);
}

#[test]
fn test_analysis_clean_record() {
    let a = analyze(&record(0.0, 1));
    assert_eq!(a.condition, RecordCondition::NearMint);
    assert_eq!(a.clipped_samples, 0);
    // Two tones at -20 dBFS (1.5x peak) with a 0.2-1.0 swell.
    assert!(a.peak_dbfs > -20.0 && a.peak_dbfs < -14.0, "peak {}", a.peak_dbfs);
    assert!(a.music_rms_db < a.peak_dbfs);
}

#[test]
fn test_problem_spots_are_music_and_ordered() {
    // Quiet music (-36 dBFS tones, about -40 dBFS RMS) with clicks peaking -16..-4 dBFS: they
    // stand well clear of both their second and the audio around them.
    let a = analyze(&record_with(8.0, 3, -36.0, -16.0));
    assert!(!a.problem_spots.is_empty());
    let sr = SR as usize;
    for w in a.problem_spots.windows(2) {
        assert!(w[0].start_frame + w[0].len_frames <= w[1].start_frame, "spots ordered and separate");
    }
    for s in &a.problem_spots {
        let block = s.start_frame / sr;
        assert!(!a.gap_blocks.contains(&block), "spot at {} s is in a gap", block);
        assert!(s.clicks > 0);
        assert!(s.level_db >= -45.0, "spot at {} s is not music ({} dBFS)", block, s.level_db);
    }
}

#[test]
fn test_clipping_is_counted() {
    let mut frames = record(0.0, 4);
    for f in frames.iter_mut().skip(5 * SR as usize).take(100) {
        f.0 = 16_777_215;
    }
    assert_eq!(analyze(&frames).clipped_samples, 100);
}

#[test]
fn test_cancel_returns_none() {
    let frames = record(4.0, 5);
    assert!(analyze_album(&frames, SR, 2, |_, _| {}, &AtomicBool::new(true)).is_none());
}

#[test]
fn test_autotune_with_analysis_agrees_with_plain_autotune() {
    let frames = record(16.0, 6);
    let a = analyze(&frames);
    let with = autotune_audio_with_analysis(&frames, SR, 2, Some(&a), |_, _| {});
    let without = autotune_audio(&frames, SR, 2);
    assert_eq!(with.condition, a.condition);
    assert_eq!(with.audible_clicks_per_sec, a.audible_clicks_per_sec);
    assert_eq!(with.recommended.mode, without.recommended.mode);
    assert!(
        (with.recommended.dcl - without.recommended.dcl).abs() <= 5,
        "dcl {} with analysis vs {} without",
        with.recommended.dcl,
        without.recommended.dcl
    );
}

#[test]
fn test_loud_music_masks_clicks_from_problem_spots() {
    // Clicks peaking -34..-22 dBFS under -20 dBFS music never stand 15 dB clear of it.
    let a = analyze(&record(8.0, 3));
    assert!(a.problem_spots.is_empty(), "{} spots", a.problem_spots.len());
    assert!(a.block_exposed.iter().enumerate().all(|(b, &n)| n == 0 || a.gap_blocks.contains(&b) || a.block_db[b] < -45.0));
}

/// Broadband "music" (noise) in three segments with silence between, the right channel
/// delayed by `delay_us` and scaled by `right_db`. With `mono` both channels carry the same
/// signal (before the delay and gain).
fn noise_record(delay_us: f64, right_db: f64, mono: bool) -> Vec<(i32, i32)> {
    use cr_dsp::post::filters::{apply_fir, fractional_delay_taps};
    let sr = SR as usize;
    let n = 45 * sr;
    let mut rng = Rng(11);
    let a: Vec<f64> = (0..n).map(|_| rng.noise()).collect();
    let b: Vec<f64> = if mono { a.clone() } else { (0..n).map(|_| rng.noise()).collect() };
    // Stereo: a mix with a strong centre (as most records have) plus a side component.
    let left: Vec<f64> = a.iter().zip(&b).map(|(x, y)| x + if mono { 0.0 } else { 0.3 * y }).collect();
    let right_src: Vec<f64> = a.iter().zip(&b).map(|(x, y)| x - if mono { 0.0 } else { 0.3 * y }).collect();
    let right = apply_fir(&fractional_delay_taps(delay_us * 1e-6 * SR as f64, 31, 8.0), &right_src);
    let g = 10f64.powf(right_db / 20.0);
    let segments = [(3, 15), (19, 31), (35, 45)];
    (0..n)
        .map(|i| {
            let music = segments.iter().any(|&(s, e)| i >= s * sr && i < e * sr);
            let level = if music { db(-24.0) } else { 0.0 };
            let floor = db(-62.0);
            ((level * left[i] + floor * rng.noise()) as i32, (level * g * right[i] + floor * rng.noise()) as i32)
        })
        .collect()
}

#[test]
fn test_suggested_post_for_stereo_record_with_azimuth_offset() {
    let a = analyze(&noise_record(6.0, -1.0, false));
    assert_eq!(a.mode, cr_dsp::params::ChannelMode::Stereo);
    let p = a.suggested_post();
    assert!((p.azimuth_us - 6.0).abs() < 0.7, "azimuth {} (measured {} +- {})", p.azimuth_us, a.channels.azimuth_us, a.channels.azimuth_se_us);
    assert_eq!(p.balance_db, 0.0, "balance is not corrected on stereo records");
    assert!(!p.flip_right_polarity && !p.mono_sum);
    assert_eq!(p.subsonic_hz, Some(18.0));
    assert_eq!(p.elliptical_hz, Some(100.0));
}

#[test]
fn test_suggested_post_for_mono_record() {
    let a = analyze(&noise_record(0.0, -0.8, true));
    assert!(a.is_mono_record(), "mode {:?}", a.mode);
    let p = a.suggested_post();
    assert!(p.mono_sum);
    assert_eq!(p.elliptical_hz, None);
    assert!((p.balance_db - 0.8).abs() < 0.1, "balance {}", p.balance_db);
    assert_eq!(p.azimuth_us, 0.0, "no offset to correct: measured {}", a.channels.azimuth_us);
}

#[test]
fn test_inverted_channel_is_flipped() {
    let frames: Vec<(i32, i32)> = noise_record(0.0, 0.0, true).into_iter().map(|(l, r)| (l, -r)).collect();
    assert!(analyze(&frames).suggested_post().flip_right_polarity);
}

#[test]
fn test_drum_hits_are_not_problem_spots() {
    // Loud percussive hits (instant attack, 30 ms decay, -6 dBFS) over a quiet bed, and no
    // clicks: the engine reshapes some hits at dcl 50 and the removed part can stand 15 dB above
    // its second, but the audio around a hit stays loud, so none counts as an exposed click.
    let sr = SR as usize;
    let mut frames = record_at(0.0, 13, -40.0);
    let mut rng = Rng(17);
    for hit in (3 * sr..44 * sr).step_by(sr / 2) {
        for k in 0..(sr * 3 / 10) {
            let env = (-(k as f64) / (0.03 * SR as f64)).exp();
            let v = db(-6.0) * env * rng.noise() * 0.5;
            let f = &mut frames[hit + k];
            f.0 = f.0.saturating_add(v as i32);
            f.1 = f.1.saturating_add((0.9 * v) as i32);
        }
    }
    let a = analyze(&frames);
    let passes_second_test = a
        .events
        .iter()
        .filter(|e| e.is_audible() && e.peak_db - a.block_db[e.frame / sr] >= cr_dsp::analysis::EXPOSED_DB)
        .count();
    assert!(passes_second_test > 0, "the hits must pass the one-second test for this to test anything");
    assert!(a.problem_spots.is_empty(), "{} spots, first at {} s", a.problem_spots.len(), a.problem_spots.first().map_or(0, |s| s.start_frame / sr));
    assert_eq!(a.events.iter().filter(|e| e.exposed).count(), 0);
}
