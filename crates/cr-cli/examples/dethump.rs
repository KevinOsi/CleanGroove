//! Thump repair on request, with A/B excerpts for listening.
//!
//! Renders FILE with the declicker (Classic by default, Wavelet, stereo, DeClick 30), then runs
//! the long AR thump repair (`cr_dsp::thump::repair_thumps`) on the thump-like rotational
//! series the finder reports in the original. For each series it writes three aligned excerpts
//! (the series plus 3 s either side) into OUT (default `scratch/listen/thump`, git-ignored):
//! `NAME_MMSS_original.wav`, `..._declicked.wav`, `..._dethumped.wav`, and
//! `..._labels.txt` (an Audacity label on every repaired span).
//!
//! usage: cargo run --release -p cr-cli --example dethump -- FILE [--dcl N]
//!        [--engine classic|enhanced] [--out DIR]

use std::path::PathBuf;

use cr_audio_io::{AudioFileReader, AudioFileWriter, AudioMetadata};
use cr_dsp::params::{ChannelMode, CrParameters, Engine, Method};
use cr_dsp::process::Processor;
use cr_dsp::thump::{find_thumps, repair_span, repair_thumps};

fn timestamp(frame: usize, sr: u32) -> String {
    let t = frame as f64 / sr as f64;
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file = None;
    let mut dcl = 30;
    let mut engine = Engine::Classic;
    let mut out_dir = PathBuf::from("scratch/listen/thump");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dcl" => {
                dcl = args[i + 1].parse().unwrap();
                i += 1;
            }
            "--engine" => {
                engine = Engine::parse(&args[i + 1]).expect("--engine classic|enhanced");
                i += 1;
            }
            "--out" => {
                out_dir = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            other => file = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let file = file.expect("usage: dethump FILE [--dcl N] [--engine E] [--out DIR]");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut reader = AudioFileReader::open(&file).unwrap();
    let sr = reader.meta.sample_rate;
    let frames = reader.read_all_frames().unwrap();
    let stem: String = file
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();

    let (_, series) = find_thumps(&frames, sr);
    let thumps: Vec<_> = series.into_iter().filter(|s| s.is_thump_like()).collect();
    println!("{}: {} thump-like series", file.display(), thumps.len());
    if thumps.is_empty() {
        return;
    }

    let mut p = CrParameters::preset_stereo_lp();
    p.dcl = dcl;
    p.method = Method::Wavelet;
    p.mode = ChannelMode::Stereo;
    p.engine = engine;
    let (declicked, _) = Processor::process_slice_aligned(p, sr, &frames);
    let mut dethumped = declicked.clone();
    let spans = repair_thumps(&mut dethumped, sr, &thumps);
    println!(
        "{} {} DeClick {}: {} spans repaired, {:.1} ms in all",
        engine.name(),
        "declick",
        dcl,
        spans.len(),
        spans.iter().map(|(a, b)| b - a).sum::<usize>() as f64 * 1000.0 / sr as f64
    );

    let meta = AudioMetadata { channels: 2, sample_rate: sr, bits_per_sample: 24, is_float: false, total_frames: 0 };
    let pad = 3 * sr as usize;
    for s in &thumps {
        let first = s.events.first().unwrap().frame;
        let last = s.events.last().unwrap().frame;
        let (a, b) = (first.saturating_sub(pad), (last + pad).min(frames.len()));
        let t = first / sr as usize;
        let name = format!("{stem}_{:02}{:02}", t / 60, t % 60);
        for (suffix, audio) in [("original", &frames), ("declicked", &declicked), ("dethumped", &dethumped)] {
            let mut w = AudioFileWriter::create(out_dir.join(format!("{name}_{suffix}.wav")), meta).unwrap();
            w.write_chunk(&audio[a..b]).unwrap();
            w.finalize().unwrap();
        }
        let labels: String = s
            .events
            .iter()
            .map(|e| {
                let (sa, sb) = repair_span(e, sr);
                format!(
                    "{:.6}\t{:.6}\tthump {} ({:.1} ms)\n",
                    (sa - a) as f64 / sr as f64,
                    (sb - a) as f64 / sr as f64,
                    timestamp(e.frame, sr),
                    (sb - sa) as f64 * 1000.0 / sr as f64
                )
            })
            .collect();
        std::fs::write(out_dir.join(format!("{name}_labels.txt")), labels).unwrap();
        println!(
            "  {} .. {}: {} thumps, period {:.4} s -> {}_*.wav",
            timestamp(first, sr),
            timestamp(last, sr),
            s.events.len(),
            s.period_s,
            name
        );
    }
}
