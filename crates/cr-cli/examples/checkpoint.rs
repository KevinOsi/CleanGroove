//! Repairs at an ear-verified checkpoint, Classic against Enhanced.
//!
//! Renders the whole file (Wavelet, stereo, no post chain) with both engines at DeClick 30 and
//! 45 and lists every repair (changed run, single unchanged samples bridged) that overlaps the
//! window, with its channel, start time and length, so a moment the user has listened to can
//! be checked: music should gain no repairs, damage should be repaired. Repairs Enhanced made
//! through its sustained-defect (long) path are marked.
//!
//! usage: cargo run --release -p cr-cli --example checkpoint -- FILE MM:SS[.ss] [SECONDS]
//!        (window from the time on, default 1 s)

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{ChannelMode, CrParameters, Engine, Method};
use cr_dsp::process::Processor;

fn parse_time(s: &str) -> f64 {
    match s.split_once(':') {
        Some((m, sec)) => m.parse::<f64>().unwrap() * 60.0 + sec.parse::<f64>().unwrap(),
        None => s.parse().unwrap(),
    }
}

fn timestamp(frame: usize, sr: usize) -> String {
    let t = frame as f64 / sr as f64;
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

/// Changed runs overlapping [from, to): (channel, first frame, length).
fn runs_in(orig: &[(i32, i32)], out: &[(i32, i32)], from: usize, to: usize) -> Vec<(usize, usize, usize)> {
    let mut runs = Vec::new();
    let lo = from.saturating_sub(4096);
    let hi = (to + 4096).min(orig.len());
    for ch in 0..2 {
        let get = |v: &[(i32, i32)], i: usize| if ch == 0 { v[i].0 } else { v[i].1 };
        let mut start: Option<usize> = None;
        let mut last = 0;
        for i in lo..hi {
            if (get(orig, i) - get(out, i)).abs() > 2 {
                if start.is_some() && i > last + 2 {
                    runs.push((ch, start.unwrap(), last + 1 - start.unwrap()));
                    start = None;
                }
                start.get_or_insert(i);
                last = i;
            }
        }
        if let Some(s) = start {
            runs.push((ch, s, last + 1 - s));
        }
    }
    runs.retain(|&(_, s, n)| s < to && s + n > from);
    runs.sort_by_key(|r| (r.1, r.0));
    runs
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let at = parse_time(&args[1]);
    let secs: f64 = args.get(2).map(|v| v.parse().unwrap()).unwrap_or(1.0);
    let mut reader = AudioFileReader::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let sr = reader.meta.sample_rate as usize;
    let frames = reader.read_all_frames().unwrap();
    let from = (at * sr as f64) as usize;
    let to = ((at + secs) * sr as f64) as usize;
    println!("{path}: {} to {}", timestamp(from, sr), timestamp(to, sr));

    let jobs: Vec<(Engine, i32)> = [Engine::Classic, Engine::Enhanced]
        .into_iter()
        .flat_map(|e| [30, 45].map(|d| (e, d)))
        .collect();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|&(engine, dcl)| {
                let frames = &frames;
                s.spawn(move || {
                    let mut p = CrParameters::preset_stereo_lp();
                    p.dcl = dcl;
                    p.method = Method::Wavelet;
                    p.mode = ChannelMode::Stereo;
                    p.engine = engine;
                    let mut processor = Processor::new(p, sr as u32, Some(frames.len()));
                    let mut out = Vec::with_capacity(frames.len());
                    processor.process_chunk(frames, &mut out);
                    out.extend(processor.flush());
                    (runs_in(frames, &out, from, to), processor.long_repair_log())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for ((engine, dcl), (runs, long_log)) in jobs.iter().zip(&results) {
        println!("\n{} DeClick {}: {} repairs in the window", engine.name(), dcl, runs.len());
        for &(ch, s, n) in runs {
            let long = long_log.iter().any(|&(c, ls, ln)| c == ch && ls < s + n + 8 && ls + ln + 8 > s);
            println!(
                "  {} {} {:>5} samples ({:.2} ms){}",
                timestamp(s, sr),
                if ch == 0 { "L" } else { "R" },
                n,
                n as f64 * 1000.0 / sr as f64,
                if long { "  long path" } else { "" }
            );
        }
    }
}
