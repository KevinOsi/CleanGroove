//! Gold-set regression and baseline tool.
//!
//! Runs every WAV/FLAC/AIFF in a folder (default `scratch/audio`, which is git-ignored: the
//! clips are copyrighted and stay on the dev machine) through the album analysis and through
//! the declick engine at fixed settings, and prints one row per clip and setting. Each render
//! gets a fingerprint of its exact output; with `--baseline FILE` the rows are compared with a
//! saved run and every changed output is flagged, so DSP changes show up clip by clip.
//!
//! `--engine enhanced` renders with the Enhanced engine instead of Classic (the analysis stays on
//! Classic) and fills two more columns: repairs made by its sustained-defect (long) path and
//! long candidates the music gate refused. `--long-list FILE` writes every long-path repair as
//! a timestamp to listen to (clip, DeClick, time, channel, length; marked `>6ms` when longer
//! than Classic can repair). `--no-analysis` skips the analysis (for full albums). The render
//! column is each render's wall time, to catch a slow or stuck engine on long files.
//!
//! usage:
//!   cargo run --release -p cr-cli --example goldset -- [DIR] [--save FILE] [--baseline FILE]
//!        [--engine classic|enhanced] [--long-list FILE] [--no-analysis]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{ChannelMode, CrParameters, Engine, Method};
use cr_dsp::process::Processor;

/// DeClick settings every clip is rendered at (Classic engine, Wavelet, stereo, no post chain).
const RENDER_DCL: [i32; 2] = [30, 45];

/// FNV-1a over the output samples: identical output gives an identical fingerprint.
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

/// Repaired bursts (both channels, single unchanged samples bridged) and changed samples.
fn repairs(orig: &[(i32, i32)], out: &[(i32, i32)]) -> (usize, usize) {
    let (mut bursts, mut changed) = (0, 0);
    for ch in 0..2 {
        let mut gap = usize::MAX;
        for (o, p) in orig.iter().zip(out) {
            let d = if ch == 0 { o.0 - p.0 } else { o.1 - p.1 };
            if d.abs() > 2 {
                changed += 1;
                if gap > 1 {
                    bursts += 1;
                }
                gap = 0;
            } else {
                gap = gap.saturating_add(1);
            }
        }
    }
    (bursts, changed)
}

fn timestamp(frame: usize, sr: u32) -> String {
    let t = frame as f64 / sr as f64;
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from("scratch/audio");
    let (mut save, mut baseline) = (None, None);
    let mut engine = Engine::Classic;
    let mut long_list: Option<PathBuf> = None;
    let mut analysis = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--engine" => {
                engine = args.get(i + 1).and_then(|v| Engine::parse(v)).expect("--engine classic|enhanced");
                i += 1;
            }
            "--long-list" => {
                long_list = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--no-analysis" => analysis = false,
            "--save" => {
                save = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--baseline" => {
                baseline = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            other => dir = PathBuf::from(other),
        }
        i += 1;
    }

    let previous: BTreeMap<String, String> = baseline
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| {
            text.lines()
                .filter_map(|l| {
                    let mut f = l.splitn(3, '\t');
                    Some((format!("{}\t{}", f.next()?, f.next()?), f.next()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut clips: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            matches!(ext.as_str(), "wav" | "flac" | "aif" | "aiff")
        })
        .collect();
    clips.sort();

    let started = Instant::now();
    let mut saved = String::new();
    let mut long_rows = String::new();
    println!("engine: {}", engine.name());
    let mut changed_outputs = 0;
    println!(
        "{:<52} {:>5} {:>6} {:>7} {:>6} {:>5} {:>5} {:>6} | {:>4} {:>6} {:>9} {:>8} {:>4} {:>4} {:>5} {:>7} {:>16}",
        "clip", "secs", "mode", "aud/s", "exp", "spots", "long", "peak", "dcl", "rep/s", "rate", "rep ms", "canc", "lrep", "gated", "render", "fingerprint"
    );
    for clip in &clips {
        let name = clip.file_name().unwrap().to_string_lossy().to_string();
        let mut reader = AudioFileReader::open(clip).unwrap();
        let sr = reader.meta.sample_rate;
        let frames = reader.read_all_frames().unwrap();
        let secs = frames.len() as f64 / sr as f64;
        let a = analysis
            .then(|| cr_dsp::analysis::analyze_album(&frames, sr, reader.meta.channels, |_, _| {}, &AtomicBool::new(false)).unwrap());
        let exposed: u32 = a.as_ref().map(|a| a.block_exposed.iter().map(|&n| n as u32).sum()).unwrap_or(0);
        let mode = match a.as_ref().map(|a| a.mode) {
            Some(ChannelMode::Stereo) => "stereo",
            Some(ChannelMode::Merge) => "mono",
            Some(ChannelMode::Mono) => "mono1",
            None => "-",
        };
        for dcl in RENDER_DCL {
            let mut p = CrParameters::preset_stereo_lp();
            p.dcl = dcl;
            p.method = Method::Wavelet;
            p.mode = ChannelMode::Stereo;
            p.engine = engine;
            let render_start = Instant::now();
            let mut processor = Processor::new(p, sr, Some(frames.len()));
            let mut out = Vec::with_capacity(frames.len());
            processor.process_chunk(&frames, &mut out);
            out.extend(processor.flush());
            let render_secs = render_start.elapsed().as_secs_f64();
            let stats = processor.stats();
            let (bursts, changed) = repairs(&frames, &out);
            for (ch, start, len) in processor.long_repair_log() {
                let ms = len as f64 * 1000.0 / sr as f64;
                long_rows.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{:.1} ms{}\n",
                    name,
                    dcl,
                    timestamp(start, sr),
                    if ch == 0 { "L" } else { "R" },
                    ms,
                    // Classic's limit: 128 band samples = 256 output samples, 5.8 ms at 44.1 kHz.
                    if len > 256 { "\t>6ms" } else { "" }
                ));
            }
            let fp = fingerprint(&out);
            let key = format!("{name}\t{dcl}");
            let row = format!("{}\t{:016x}", bursts, fp);
            let flag = match previous.get(&key) {
                Some(old) if old != &row => {
                    changed_outputs += 1;
                    let old_bursts: usize = old.split('\t').next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    format!("  CHANGED (repairs {} -> {})", old_bursts, bursts)
                }
                Some(_) => "  same".to_string(),
                None if baseline.is_some() => "  new".to_string(),
                None => String::new(),
            };
            println!(
                "{:<52} {:>5.1} {:>6} {:>7.2} {:>6} {:>5} {:>5} {:>6.1} | {:>4} {:>6.1} {:>9} {:>8.0} {:>4} {:>4} {:>5} {:>6.1}s {:016x}{}",
                if dcl == RENDER_DCL[0] { name.as_str() } else { "" },
                secs,
                mode,
                a.as_ref().map(|a| a.audible_clicks_per_sec).unwrap_or(0.0),
                exposed,
                a.as_ref().map(|a| a.problem_spots.len()).unwrap_or(0),
                a.as_ref().map(|a| a.long_repairs).unwrap_or(0),
                a.as_ref().map(|a| a.peak_dbfs).unwrap_or(0.0),
                dcl,
                bursts as f64 / secs,
                if changed > 0 { format!("1 in {:.0}", (2 * frames.len()) as f64 / changed as f64) } else { "none".to_string() },
                changed as f64 * 1000.0 / sr as f64,
                stats.cancelled_long_repairs,
                stats.long_repairs,
                stats.long_gated,
                render_secs,
                fp,
                flag
            );
            saved.push_str(&format!("{key}\t{row}\n"));
        }
    }
    println!(
        "\n{} clips in {:.1} s.{}",
        clips.len(),
        started.elapsed().as_secs_f64(),
        if baseline.is_some() { format!(" {} outputs changed against the baseline.", changed_outputs) } else { String::new() }
    );
    if let Some(path) = long_list {
        std::fs::write(&path, &long_rows).unwrap();
        println!("{} long-path repairs written to {}", long_rows.lines().count(), path.display());
    }
    if let Some(path) = save {
        std::fs::write(&path, saved).unwrap();
        println!("Saved this run as {}", Path::new(&path).display());
    }
}
