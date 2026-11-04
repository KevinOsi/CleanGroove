//! Auto-tuner tests on synthetic records: groove noise, music, silence between tracks, and
//! clicks injected at a known density.

use cr_dsp::autotune::{autotune_audio, AutoTuneReport, RecordCondition};
use cr_dsp::params::{ChannelMode, Method};

const SR: u32 = 44_100;
const FULL_SCALE: f64 = 16_777_216.0;

/// Small deterministic generator (64-bit LCG).
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Roughly Gaussian, unit variance (sum of 4 uniforms).
    fn noise(&mut self) -> f64 {
        (self.next_f64() + self.next_f64() + self.next_f64() + self.next_f64() - 2.0) * 1.732
    }
}

fn db(level: f64) -> f64 {
    FULL_SCALE * 10f64.powf(level / 20.0)
}

#[derive(Clone, Copy, PartialEq)]
enum Music {
    /// Different sustained tones in each channel.
    Stereo,
    /// The same tones in both channels, right 1 dB quieter (mono LP captured in stereo).
    DualMono,
}

/// Builds a record side: 3 s lead-in, then music / gap / music / gap / music
/// (12 / 4 / 12 / 4 / 10 s), or 45 s of continuous music when `gaps` is false.
/// Groove noise at -60 dBFS throughout. Clicks are injected at `clicks_per_sec` (summed over
/// both channels), one channel each, peaks between -34 and -22 dBFS.
fn record(music: Music, gaps: bool, clicks_per_sec: f64, seed: u64) -> Vec<(i32, i32)> {
    let sr = SR as usize;
    let n = 45 * sr;
    let segments: [(usize, usize); 3] = [(3, 15), (19, 31), (35, 45)];
    let in_music = |i: usize| !gaps || segments.iter().any(|&(a, b)| i >= a * sr && i < b * sr);
    let mut rng = Rng(seed);
    let floor = db(-60.0);
    let tone = db(-20.0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / SR as f64;
        let (mut l, mut r) = (floor * rng.noise(), floor * rng.noise());
        if in_music(i) {
            let swell = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 0.25 * t).sin();
            match music {
                Music::Stereo => {
                    l += tone * swell * ((2.0 * std::f64::consts::PI * 330.0 * t).sin() + 0.5 * (2.0 * std::f64::consts::PI * 1210.0 * t).sin());
                    r += tone * swell * ((2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.5 * (2.0 * std::f64::consts::PI * 990.0 * t).sin());
                }
                Music::DualMono => {
                    let m = tone * swell * ((2.0 * std::f64::consts::PI * 330.0 * t).sin() + 0.5 * (2.0 * std::f64::consts::PI * 1210.0 * t).sin());
                    l += m;
                    r += m * 0.891;
                }
            }
        }
        out.push((l as i32, r as i32));
    }

    // Clicks: four-sample impulse, random channel, sign and level.
    let shape = [0.6, 1.0, -0.5, 0.2];
    let count = (clicks_per_sec * n as f64 / SR as f64).round() as usize;
    if count > 0 {
        let spacing = n / count;
        for k in 0..count {
            let pos = k * spacing + (rng.next_f64() * (spacing - 8) as f64) as usize;
            let peak = db(-34.0 + 12.0 * rng.next_f64()) * if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
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

fn tune(frames: &[(i32, i32)]) -> AutoTuneReport {
    let report = autotune_audio(frames, SR, 2);
    for r in &report.rationale {
        println!("  {}", r);
    }
    println!(
        "  -> {} | {:.2} audible/s | dcl {} | mode {:?} | reverse {} | pitch {}",
        report.condition, report.audible_clicks_per_sec, report.recommended.dcl,
        report.recommended.mode, report.recommended.reverse, report.recommended.detect_pitch
    );
    report
}

#[test]
fn test_autotune_clean_record_grades_near_mint_with_light_pass() {
    let report = tune(&record(Music::Stereo, true, 0.0, 1));
    assert!(!report.condition_estimated, "gaps between tracks must be found");
    assert_eq!(report.condition, RecordCondition::NearMint);
    assert!(report.recommended.dcl <= 15, "near mint should get a light pass, got dcl {}", report.recommended.dcl);
    assert_eq!(report.recommended.mode, ChannelMode::Stereo);
    assert_eq!(report.recommended.method, Method::Wavelet);
    assert_eq!(report.recommended.dcr, 0);
}

#[test]
fn test_autotune_heavy_wear_measures_click_density() {
    let injected = 16.0;
    let report = tune(&record(Music::Stereo, true, injected, 2));
    assert!(!report.condition_estimated);
    let err = (report.audible_clicks_per_sec - injected).abs() / injected;
    assert!(err < 0.2, "measured {:.2} audible clicks/s for {} injected", report.audible_clicks_per_sec, injected);
    assert_eq!(report.condition, RecordCondition::GoodPlus);
    assert!((40..=50).contains(&report.recommended.dcl), "heavy wear should land in the 40s, got {}", report.recommended.dcl);
    assert!(report.repair_fraction <= 1.0 / 200.0, "repair rate guard: 1 in {:.0}", 1.0 / report.repair_fraction);
}

#[test]
fn test_autotune_light_wear_grades_between_and_orders_dcl() {
    let light = tune(&record(Music::Stereo, true, 2.0, 3));
    let moderate = tune(&record(Music::Stereo, true, 6.0, 4));
    assert_eq!(light.condition, RecordCondition::VeryGoodPlus);
    assert_eq!(moderate.condition, RecordCondition::VeryGood);
    assert!(
        light.recommended.dcl < moderate.recommended.dcl,
        "light wear dcl {} should be below moderate wear dcl {}",
        light.recommended.dcl,
        moderate.recommended.dcl
    );
}

#[test]
fn test_autotune_mono_lp_detects_merge_mode() {
    let report = tune(&record(Music::DualMono, true, 4.0, 5));
    assert_eq!(report.recommended.mode, ChannelMode::Merge);
}

#[test]
fn test_autotune_without_gaps_reports_estimate() {
    let report = tune(&record(Music::Stereo, false, 4.0, 6));
    assert!(report.condition_estimated, "continuous music must fall back to an estimated grade");
    assert!((10..=50).contains(&report.recommended.dcl));
}
