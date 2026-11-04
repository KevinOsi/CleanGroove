//! Golden-output test: the Classic engine's exact output, across its main configurations,
//! pinned by fingerprint. The Classic engine is locked; any change to its output
//! fails here. Enhanced/Max engine work must leave every entry unchanged.
//!
//! Inputs: the tracked demo track, and a generated signal (tones over a noise floor, clicks
//! graded from faint to loud, wide defects and sharp transients) at 44.1, 96 and 192 kHz so
//! the sensitivity settings and the high-rate paths are told apart.
//!
//! To re-pin after an intended, approved change: run with GOLDEN_PRINT=1 and copy the printed
//! table into EXPECTED.

use std::path::PathBuf;

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{ChannelMode, CrParameters, Method};
use cr_dsp::post::{run_post, to_internal, PostSettings};
use cr_dsp::process::Processor;

/// FNV-1a over the output samples.
fn fingerprint(frames: &[(i32, i32)]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &(l, r) in frames {
        for v in [l, r] {
            for b in v.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
    }
    h
}

/// 8 s of stereo "music" at `sr`: two swelling tone pairs over a -60 dBFS noise floor, a sharp
/// percussive hit every second, 120 one-channel clicks with peaks from -50 to -10 dBFS, and
/// three wide defects (0.5, 2 and 6 ms). Deterministic.
fn generated(sr: u32) -> Vec<(i32, i32)> {
    let fs = 16_777_216.0f64;
    let db = |d: f64| fs * 10f64.powf(d / 20.0);
    let n = 8 * sr as usize;
    let mut seed = 12345u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let tau = 2.0 * std::f64::consts::PI;
    let mut out: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let t = i as f64 / sr as f64;
            let swell = 0.55 + 0.45 * (tau * 0.3 * t).sin();
            let l = db(-24.0) * swell * ((tau * 220.0 * t).sin() + 0.4 * (tau * 1760.0 * t).sin());
            let r = db(-24.0) * swell * ((tau * 330.0 * t).sin() + 0.4 * (tau * 2640.0 * t).sin());
            let since = (i % sr as usize) as f64 / sr as f64;
            let hit = db(-14.0) * (-since / 0.02).exp() * noise();
            (l + hit + db(-60.0) * noise(), r + 0.8 * hit + db(-60.0) * noise())
        })
        .collect();
    let shape = [0.6, 1.0, -0.5, 0.2];
    for k in 0..120 {
        let pos = (n - 16) * (2 * k + 1) / 240;
        let peak = db(-50.0 + 40.0 * (k % 12) as f64 / 11.0) * if k % 2 == 0 { 1.0 } else { -1.0 };
        for (j, s) in shape.iter().enumerate() {
            if k % 3 == 0 { out[pos + j].1 += s * peak } else { out[pos + j].0 += s * peak }
        }
    }
    for (ms, at) in [(0.5, 0.31), (2.0, 0.53), (6.0, 0.77)] {
        let start = (at * n as f64) as usize;
        let len = (ms / 1000.0 * sr as f64) as usize;
        for j in 0..len {
            out[start + j].0 += db(-12.0) * (tau * j as f64 / len as f64).sin();
        }
    }
    out.into_iter().map(|(l, r)| (l.round() as i32, r.round() as i32)).collect()
}

fn config(name: &str) -> CrParameters {
    let mut p = CrParameters::preset_stereo_lp();
    match name {
        "dcl50" => {}
        "dcl30" => p.dcl = 30,
        "dcl70" => p.dcl = 70,
        "simple" => p.method = Method::Simple,
        "x2" => p.method = Method::WaveletX2,
        "x3" => p.method = Method::WaveletX3,
        "merge" => p.mode = ChannelMode::Merge,
        "mono" => p.mode = ChannelMode::Mono,
        "decrackle50" => p.dcr = 50,
        "simple_decrackle50" => {
            p.method = Method::Simple;
            p.dcr = 50;
        }
        "reverse_pitch" => {
            p.reverse = true;
            p.detect_pitch = true;
        }
        "preset_percussion" => p = CrParameters::preset_stereo_percussion(),
        "preset_78" => p = CrParameters::preset_78_rpm(),
        other => panic!("unknown config {other}"),
    }
    p
}

/// (input, config, fingerprint). Inputs: "demo", "gen44", "gen96", "gen192". Config
/// "post_default" is the default preset declicked, then the default post chain.
const EXPECTED: &[(&str, &str, u64)] = &[
    ("demo", "dcl50", 0xdcd33b22feacefd9),
    ("demo", "simple", 0x801f8214858140fa),
    ("demo", "merge", 0x52cfefaf798b2cab),
    ("demo", "mono", 0x624618a2747266d9),
    ("demo", "decrackle50", 0xcb66059d6e51f3da),
    ("demo", "simple_decrackle50", 0x3c059fe904963271),
    ("demo", "reverse_pitch", 0x75a3caf3c28b43dc),
    ("demo", "preset_percussion", 0x8493461aed89c870),
    ("demo", "preset_78", 0x61a39c54781802c9),
    ("demo", "post_default", 0xeaa29771ad2d44c3),
    ("gen44", "dcl30", 0x676f7eb505b087f6),
    ("gen44", "dcl50", 0x1bc5c8460080f573),
    ("gen44", "dcl70", 0x8438f17a45299f93),
    ("gen44", "simple", 0x0989f4ad394cf81c),
    ("gen44", "x2", 0x1bc5c8460080f573),
    ("gen44", "x3", 0x1bc5c8460080f573),
    ("gen44", "merge", 0x2d3913ca79a72836),
    ("gen44", "decrackle50", 0xdf553c3d2ebfeed3),
    ("gen44", "simple_decrackle50", 0x34426c1445f49794),
    ("gen44", "reverse_pitch", 0x42e4d62ce0999125),
    ("gen44", "post_default", 0x0b3b0e95729ebff5),
    ("gen96", "dcl50", 0xd95e2670b389057c),
    ("gen96", "x2", 0xd95e2670b389057c),
    ("gen192", "dcl50", 0x2eb8e7d789f78802),
];

#[test]
fn test_classic_output_is_unchanged() {
    // The demo track is a local file (not in the public repository). Without it, the rows on
    // generated signals still pin the Classic output; the demo rows are skipped.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scratch/demo/vinyl_demo_track.wav");
    let mut inputs = vec![
        ("gen44", 44_100, generated(44_100)),
        ("gen96", 96_000, generated(96_000)),
        ("gen192", 192_000, generated(192_000)),
    ];
    match AudioFileReader::open(&path) {
        Ok(mut reader) => {
            let demo_sr = reader.meta.sample_rate;
            inputs.push(("demo", demo_sr, reader.read_all_frames().unwrap()));
        }
        Err(_) => eprintln!("vinyl_demo_track.wav not found: checking generated signals only"),
    }

    let mut got = Vec::new();
    let mut expected = Vec::new();
    for &(input, cfg, want) in EXPECTED {
        let Some((_, sr, frames)) = inputs.iter().find(|i| i.0 == input) else { continue };
        expected.push((input, cfg, want));
        let fp = if cfg == "post_default" {
            let (declicked, _) = Processor::process_slice_aligned(CrParameters::preset_stereo_lp(), *sr, frames);
            let post = PostSettings { subsonic_hz: Some(18.0), elliptical_hz: Some(100.0), ..Default::default() };
            let mut out = Vec::with_capacity(declicked.len());
            run_post(&declicked, *sr, &post, |b| out.extend(b.iter().map(|&(l, r)| (to_internal(l), to_internal(r)))));
            fingerprint(&out)
        } else {
            let (out, _) = Processor::process_slice_aligned(config(cfg), *sr, frames);
            fingerprint(&out)
        };
        got.push((input, cfg, fp));
    }

    if std::env::var("GOLDEN_PRINT").is_ok() {
        for (input, cfg, fp) in &got {
            println!("    (\"{input}\", \"{cfg}\", 0x{fp:016x}),");
        }
    }
    let changed: Vec<String> = got
        .iter()
        .zip(&expected)
        .filter(|(g, e)| g.2 != e.2)
        .map(|(g, e)| format!("{} {}: expected {:016x}, got {:016x}", g.0, g.1, e.2, g.2))
        .collect();
    assert!(changed.is_empty(), "Classic output changed:\n{}", changed.join("\n"));
}

#[test]
fn test_generated_input_tells_the_settings_apart() {
    // The pinned table only protects settings whose outputs differ from each other.
    let frames = generated(44_100);
    let fps: Vec<u64> = ["dcl30", "dcl50", "dcl70", "simple", "merge", "reverse_pitch"]
        .iter()
        .map(|c| fingerprint(&Processor::process_slice_aligned(config(c), 44_100, &frames).0))
        .collect();
    let mut unique = fps.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), fps.len(), "two settings gave identical output on the generated input");
}
