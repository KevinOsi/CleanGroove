//! Engine statistics that do not affect processing, and the engines' reach for long defects.

use cr_dsp::params::{CrParameters, Engine};
use cr_dsp::process::Processor;

/// Quiet tone with a loud noise burst of `burst_ms` at 2 s, in the left channel.
fn tone_with_burst(burst_ms: f64) -> Vec<(i32, i32)> {
    let sr = 44_100usize;
    let mut seed = 99u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let start = 2 * sr;
    let len = (burst_ms / 1000.0 * sr as f64) as usize;
    (0..4 * sr)
        .map(|i| {
            let t = i as f64 / sr as f64;
            let tone = 300_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 2_000.0 * noise();
            let burst = if i >= start && i < start + len { 8_000_000.0 * noise() } else { 0.0 };
            ((tone + burst) as i32, tone as i32)
        })
        .collect()
}

#[test]
fn test_classic_repair_reach_for_long_defects() {
    // Classic Wavelet at 44.1 kHz: a loud noise burst on a quiet tone is repaired in full up to
    // about 5 ms; at 6 ms it is split into two repairs and part is left; from 8 ms on the
    // detector abandons it (its click window times out after 256 samples) and nothing is
    // repaired, so it never even reaches the cancel-for-length step. This pins that
    // behaviour; the Enhanced engine is meant to extend it.
    let tone = |i: usize| 300_000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44_100.0).sin();
    for (ms, repaired) in [(1.0, true), (3.0, true), (5.0, true), (8.0, false), (12.0, false), (24.0, false)] {
        let x = tone_with_burst(ms);
        let (out, stats) = Processor::process_slice_aligned(CrParameters::preset_stereo_lp(), 44_100, &x);
        let start = 2 * 44_100;
        let len = (ms / 1000.0 * 44_100.0) as usize;
        let worst = (start..start + len).map(|i| (out[i].0 as f64 - tone(i)).abs()).fold(0.0, f64::max);
        if repaired {
            assert!(worst < 5_000.0, "{ms} ms: residual {worst:.0}");
            assert_eq!(stats.declick_events, 1, "{ms} ms: one repair");
        } else {
            assert!(worst > 1_000_000.0, "{ms} ms: expected to be left unrepaired, residual {worst:.0}");
            assert_eq!(stats.declick_events, 0, "{ms} ms: no repair");
        }
        assert_eq!(stats.cancelled_long_repairs, 0, "{ms} ms: never reaches the cancel step");
    }
}

fn enhanced() -> CrParameters {
    let mut p = CrParameters::preset_stereo_lp();
    p.engine = Engine::Enhanced;
    p
}

#[test]
fn test_enhanced_repairs_long_defects() {
    // The same bursts through the Enhanced engine: they stand far clear of the quiet tone, so its
    // sustained-defect path repairs them in one piece up to its 23 ms limit. Beyond that (30 ms)
    // the defect is left, as in Classic.
    let tone = |i: usize| 300_000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44_100.0).sin();
    for (ms, repaired) in [(5.0, true), (8.0, true), (12.0, true), (20.0, true), (30.0, false)] {
        let x = tone_with_burst(ms);
        let (out, stats) = Processor::process_slice_aligned(enhanced(), 44_100, &x);
        let start = 2 * 44_100;
        let len = (ms / 1000.0 * 44_100.0) as usize;
        let worst = (start..start + len).map(|i| (out[i].0 as f64 - tone(i)).abs()).fold(0.0, f64::max);
        if repaired {
            assert!(worst < 30_000.0, "{ms} ms: residual {worst:.0}");
            assert_eq!(stats.long_repairs, 1, "{ms} ms: one long repair");
        } else {
            assert!(worst > 1_000_000.0, "{ms} ms: expected to be left unrepaired, residual {worst:.0}");
            assert_eq!(stats.long_repairs, 0, "{ms} ms: no long repair");
        }
        // The right channel is the clean tone: Enhanced must leave it exactly as Classic does
        // (both differ from the input by the wavelet split's +-1 rounding only).
        let (classic, _) = Processor::process_slice_aligned(CrParameters::preset_stereo_lp(), 44_100, &x);
        assert!(out.iter().zip(&classic).all(|(e, c)| e.1 == c.1), "{ms} ms: clean channel differs from Classic");
    }
}

#[test]
fn test_enhanced_leaves_note_onsets_alone() {
    // Music the long path must not eat: plucked notes (abrupt onset, harmonics decaying over
    // half a second) every 400 ms over a quiet floor. Each onset looks like a sustained defect to
    // the detector, but the audio after it stays loud, so the gate refuses every one.
    let sr = 44_100usize;
    let mut seed = 7u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let notes = [220.0, 330.0, 247.0, 392.0, 294.0, 440.0, 196.0, 523.0];
    let x: Vec<(i32, i32)> = (0..6 * sr)
        .map(|i| {
            let k = i / (sr * 2 / 5);
            let t = (i % (sr * 2 / 5)) as f64 / sr as f64;
            let f = notes[k % notes.len()];
            let env = 4_000_000.0 * (-t / 0.15).exp();
            let note: f64 = (1..=6).map(|h| (2.0 * std::f64::consts::PI * f * h as f64 * t).sin() / h as f64).sum();
            let v = (env * note + 3_000.0 * noise()) as i32;
            (v, v)
        })
        .collect();
    for dcl in [30, 45, 60] {
        let mut p = enhanced();
        p.dcl = dcl;
        let (_, stats) = Processor::process_slice_aligned(p, sr as u32, &x);
        assert_eq!(stats.long_repairs, 0, "DeClick {dcl}: long repairs on note onsets");
        assert!(stats.long_gated > 0, "DeClick {dcl}: the onsets should reach the gate (and be refused)");
    }
}
