//! Pins the real-time engine's exact output, as `cr-cli/tests/golden_classic.rs` pins the
//! offline engine's. Any change to `cr_dsp::rt` that is meant to be a pure refactor must keep
//! every fingerprint; a deliberate change updates them (run with GOLDEN_PRINT=1 to print).
//!
//! The input is generated: a two-tone signal with noise, single-sample and short clicks, a
//! loud passage, and a stretch of sharp periodic pulses (a brass-like buzz) that pitch
//! protection should recognise.

use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_dsp::rt::RtProcessor;

/// FNV-1a over the output samples and the processor's final counters.
fn fingerprint(out: &[(i32, i32)], extra: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: u64| {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for &(l, r) in out {
        eat(l as u32 as u64);
        eat(r as u32 as u64);
    }
    for &e in extra {
        eat(e);
    }
    h
}

/// Deterministic test signal in the internal domain (24-bit PCM << 1).
fn generated(rate: u32, seconds: f64) -> Vec<(i32, i32)> {
    let n = (rate as f64 * seconds) as usize;
    let mut seed: u32 = 0x1234_5678;
    let mut noise = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((seed >> 8) as f64 / 16_777_216.0) - 0.5
    };
    let mut v = Vec::with_capacity(n);
    for f in 0..n {
        let t = f as f64 / rate as f64;
        let loud = if (0.9..1.3).contains(&t) { 3.0 } else { 1.0 };
        let mut l = loud * (0.15 * (2.0 * std::f64::consts::PI * 330.0 * t).sin() + 0.002 * noise());
        let mut r = loud * (0.12 * (2.0 * std::f64::consts::PI * 495.0 * t).sin() + 0.002 * noise());
        // A brass-like buzz: sharp pulses at 220 Hz from 1.5 s to 2.0 s.
        if (1.5..2.0).contains(&t) {
            let phase = (t * 220.0).fract();
            let pulse = if phase < 0.02 { 0.5 } else { 0.0 };
            l += pulse;
            r += pulse * 0.8;
        }
        v.push((l, r));
    }
    // Clicks: single samples, and short bursts of a few samples, at irregular places.
    let click_at = |s: f64| (s * rate as f64) as usize;
    for (k, s) in [0.31, 0.47, 0.62, 0.95, 1.11, 1.37, 2.21, 2.43, 2.58].iter().enumerate() {
        let i = click_at(*s);
        let width = 1 + (k % 4) * 3;
        for w in 0..width {
            if i + w < n {
                let sign = if (k + w) % 2 == 0 { 1.0 } else { -1.0 };
                v[i + w].0 += sign * 0.6;
                v[i + w].1 -= sign * 0.4;
            }
        }
    }
    v.into_iter()
        .map(|(l, r)| {
            let q = |x: f64| ((x.clamp(-0.999, 0.999) * 8_388_607.0) as i32) * 2;
            (q(l), q(r))
        })
        .collect()
}

fn run(rate: u32, params: RtParameters) -> u64 {
    let input = generated(rate, 3.0);
    let mut p = RtProcessor::new(params, rate);
    let out: Vec<(i32, i32)> = input.iter().map(|&(l, r)| p.process_frame(l, r)).collect();
    let t = p.telemetry;
    fingerprint(
        &out,
        &[t.active_l_fixes as u64, t.active_r_fixes as u64, t.click_activity.to_bits()],
    )
}

fn params(method: Method, mode: RtSoundMode, sens: RtSensitivity, pitch: bool) -> RtParameters {
    RtParameters { sensitivity: sens, method, mode, detect_pitch: pitch, reverse: false, parallel: true }
}

/// (name, rate, method, mode, sensitivity, pitch protection, expected fingerprint)
type Case = (&'static str, u32, Method, RtSoundMode, RtSensitivity, bool, u64);

const OUT: RtSoundMode = RtSoundMode::SoundOut;
const NOISE: RtSoundMode = RtSoundMode::Noise;
const IN: RtSoundMode = RtSoundMode::SoundIn;
const W: Method = Method::Wavelet;
const S: Method = Method::Simple;

const CASES: &[Case] = &[
    ("44k wavelet fine30 pitch", 44_100, W, OUT, RtSensitivity::Fine(30), true, 0xf62ca6c80e77afe0),
    ("44k wavelet fine70 nopitch", 44_100, W, OUT, RtSensitivity::Fine(70), false, 0xf79086302e522210),
    ("44k simple step5 pitch", 44_100, S, OUT, RtSensitivity::Stepped(5), true, 0xb9ac4b6cb1426a19),
    ("44k simple fine50 noise", 44_100, S, NOISE, RtSensitivity::Fine(50), true, 0x50f0a9b948db5d0a),
    ("48k wavelet step9 pitch", 48_000, W, OUT, RtSensitivity::Stepped(9), true, 0x0bb6829bcc43bf81),
    ("48k wavelet fine45 noise", 48_000, W, NOISE, RtSensitivity::Fine(45), false, 0x010680b15d907441),
    ("48k wavelet soundin", 48_000, W, IN, RtSensitivity::Fine(45), true, 0xfc9e55a7a74e7b82),
    ("96k wavelet fine40 pitch", 96_000, W, OUT, RtSensitivity::Fine(40), true, 0x10e2466ff8f50274),
    ("96k simple fine40 noise", 96_000, S, NOISE, RtSensitivity::Fine(40), true, 0xbd81fd02dd613b8d),
    ("192k wavelet fine40", 192_000, W, OUT, RtSensitivity::Fine(40), true, 0xfe53c127aa32ec3f),
    ("44k wavelet fine30 nopitch", 44_100, W, OUT, RtSensitivity::Fine(30), false, 0xfde5fc7ffb8a8855),
];

#[test]
fn the_signal_exercises_repairs_and_pitch_protection() {
    // Sound In is the input delayed; Sound Out differs only where something was repaired.
    let p = |mode, pitch| params(W, mode, RtSensitivity::Fine(30), pitch);
    assert_ne!(run(44_100, p(OUT, true)), run(44_100, p(IN, true)), "nothing was repaired");
    // Pitch protection must change the output on the buzz, or its code is not being tested.
    assert_ne!(run(44_100, p(OUT, true)), run(44_100, p(OUT, false)), "pitch protection never acted");
}

#[test]
fn realtime_output_is_unchanged() {
    let print = std::env::var("GOLDEN_PRINT").is_ok();
    let mut changed = Vec::new();
    for &(name, rate, method, mode, sens, pitch, want) in CASES {
        let got = run(rate, params(method, mode, sens, pitch));
        if print {
            println!("    (\"{name}\", {rate}, .., 0x{got:016x}),");
        }
        if got != want {
            changed.push(format!("{name}: expected {want:016x}, got {got:016x}"));
        }
    }
    assert!(changed.is_empty(), "real-time output changed:\n{}", changed.join("\n"));
}
