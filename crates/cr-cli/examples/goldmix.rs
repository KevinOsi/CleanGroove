//! Listening mix of the gold set: every clip back to back in one track, for A/B by ear.
//!
//! Writes, into OUT (default `scratch/listen`, git-ignored):
//! - `original.wav`, and `classic_dcl30.wav`, `enhanced_dcl30.wav`, `classic_dcl45.wav`,
//!   `enhanced_dcl45.wav`: the same layout (each clip, rendered on its own exactly as `goldset`
//!   does, then 1 s of silence), so the tracks line up sample for sample in Audacity;
//! - `labels_clips.txt`: an Audacity label per clip (its name);
//! - `labels_long_repairs.txt`: a label at every repair Enhanced made through its
//!   sustained-defect (long) path (DeClick, channel, length);
//! - `labels_enhanced_vs_classic.txt`: a label wherever Enhanced's output differs from
//!   Classic's (DeClick, channel, length of the differing stretch).
//!
//! In Audacity: import the WAVs, then File > Import > Labels for each label file.
//!
//! usage: cargo run --release -p cr-cli --example goldmix -- [DIR] [--out OUT]

use std::path::PathBuf;

use rayon::prelude::*;

use cr_audio_io::{AudioFileReader, AudioFileWriter, AudioMetadata};
use cr_dsp::params::{ChannelMode, CrParameters, Engine, Method};
use cr_dsp::process::Processor;

const DCLS: [i32; 2] = [30, 45];
const GAP_SECS: usize = 1;

struct Clip {
    name: String,
    frames: Vec<(i32, i32)>,
    /// (engine, dcl) -> output, and Enhanced's long-path repairs per dcl.
    renders: Vec<((Engine, i32), Vec<(i32, i32)>)>,
    long: Vec<(i32, Vec<(usize, usize, usize)>)>,
}

fn render(frames: &[(i32, i32)], sr: u32, engine: Engine, dcl: i32) -> (Vec<(i32, i32)>, Vec<(usize, usize, usize)>) {
    let mut p = CrParameters::preset_stereo_lp();
    p.dcl = dcl;
    p.method = Method::Wavelet;
    p.mode = ChannelMode::Stereo;
    p.engine = engine;
    let mut processor = Processor::new(p, sr, Some(frames.len()));
    let mut out = Vec::with_capacity(frames.len());
    processor.process_chunk(frames, &mut out);
    out.extend(processor.flush());
    (out, processor.long_repair_log())
}

/// Stretches where `a` and `b` differ in one channel (gaps under 10 ms bridged).
fn diff_runs(a: &[(i32, i32)], b: &[(i32, i32)], ch: usize, bridge: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for i in 0..a.len().min(b.len()) {
        let d = if ch == 0 { a[i].0 != b[i].0 } else { a[i].1 != b[i].1 };
        if d {
            match runs.last_mut() {
                Some(r) if i <= r.1 + bridge => r.1 = i + 1,
                _ => runs.push((i, i + 1)),
            }
        }
    }
    runs
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from("scratch/audio");
    let mut out_dir = PathBuf::from("scratch/listen");
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--out" {
            out_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else {
            dir = PathBuf::from(&args[i]);
        }
        i += 1;
    }
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            matches!(ext.as_str(), "wav" | "flac" | "aif" | "aiff")
        })
        .collect();
    paths.sort();

    let mut sr = 0u32;
    let mut loaded = Vec::new();
    for p in &paths {
        let mut reader = AudioFileReader::open(p).unwrap();
        let rate = reader.meta.sample_rate;
        if sr == 0 {
            sr = rate;
        }
        assert_eq!(rate, sr, "{}: all clips must share one sample rate", p.display());
        loaded.push((p.file_name().unwrap().to_string_lossy().to_string(), reader.read_all_frames().unwrap()));
    }

    let clips: Vec<Clip> = loaded
        .into_par_iter()
        .map(|(name, frames)| {
            let jobs: Vec<(Engine, i32)> =
                DCLS.iter().flat_map(|&d| [(Engine::Classic, d), (Engine::Enhanced, d)]).collect();
            let results: Vec<_> = jobs.par_iter().map(|&(e, d)| ((e, d), render(&frames, sr, e, d))).collect();
            let mut renders = Vec::new();
            let mut long = Vec::new();
            for ((e, d), (out, log)) in results {
                if e == Engine::Enhanced {
                    long.push((d, log));
                }
                renders.push(((e, d), out));
            }
            Clip { name, frames, renders, long }
        })
        .collect();

    let meta = AudioMetadata { channels: 2, sample_rate: sr, bits_per_sample: 24, is_float: false, total_frames: 0 };
    let gap = vec![(0i32, 0i32); GAP_SECS * sr as usize];
    let secs = |f: usize| f as f64 / sr as f64;
    let mut tracks: Vec<(String, Option<(Engine, i32)>)> = vec![("original".to_string(), None)];
    for d in DCLS {
        for e in [Engine::Classic, Engine::Enhanced] {
            tracks.push((format!("{}_dcl{}", e.name(), d), Some((e, d))));
        }
    }
    for (file, key) in &tracks {
        let mut w = AudioFileWriter::create(out_dir.join(format!("{file}.wav")), meta).unwrap();
        for c in &clips {
            let audio = match key {
                None => &c.frames,
                Some(k) => &c.renders.iter().find(|(rk, _)| rk == k).unwrap().1,
            };
            w.write_chunk(audio).unwrap();
            w.write_chunk(&gap).unwrap();
        }
        w.finalize().unwrap();
    }

    let (mut lab_clips, mut lab_long, mut lab_diff) = (String::new(), String::new(), String::new());
    let mut at = 0usize;
    let (mut n_long, mut n_diff) = (0, 0);
    for c in &clips {
        lab_clips.push_str(&format!("{:.6}\t{:.6}\t{}\n", secs(at), secs(at + c.frames.len()), c.name));
        for (d, log) in &c.long {
            for &(ch, s, n) in log {
                n_long += 1;
                lab_long.push_str(&format!(
                    "{:.6}\t{:.6}\tlong dcl{} {} {:.1} ms\n",
                    secs(at + s),
                    secs(at + s + n),
                    d,
                    if ch == 0 { "L" } else { "R" },
                    n as f64 * 1000.0 / sr as f64
                ));
            }
        }
        for d in DCLS {
            let get = |e| &c.renders.iter().find(|(k, _)| *k == (e, d)).unwrap().1;
            for ch in 0..2 {
                for (s, e) in diff_runs(get(Engine::Classic), get(Engine::Enhanced), ch, sr as usize / 100) {
                    n_diff += 1;
                    lab_diff.push_str(&format!(
                        "{:.6}\t{:.6}\tdiff dcl{} {} {:.1} ms\n",
                        secs(at + s),
                        secs(at + e),
                        d,
                        if ch == 0 { "L" } else { "R" },
                        (e - s) as f64 * 1000.0 / sr as f64
                    ));
                }
            }
        }
        at += c.frames.len() + gap.len();
    }
    std::fs::write(out_dir.join("labels_clips.txt"), lab_clips).unwrap();
    std::fs::write(out_dir.join("labels_long_repairs.txt"), lab_long).unwrap();
    std::fs::write(out_dir.join("labels_enhanced_vs_classic.txt"), lab_diff).unwrap();
    println!(
        "{} clips, {:.1} s per track, {} tracks, {} long-path repairs, {} Enhanced/Classic differences -> {}",
        clips.len(),
        secs(at),
        tracks.len(),
        n_long,
        n_diff,
        out_dir.display()
    );
}
