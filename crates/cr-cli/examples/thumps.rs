//! Lists rotational thump series (see `cr_dsp::thump`) in audio files.
//!
//! For each file (or each WAV/FLAC/AIFF in a folder): how many loud transients it has, and
//! every chain of them at a rotation period, with the evidence: the period and speed, how
//! regular the timing is, and the music check (events half and a quarter of a turn away, which
//! a beat has and a scratch does not). Each series lists its event times for listening.
//!
//! By default only thump-like series are printed (on the record's period, not beat-like, long
//! and loud: `ThumpSeries::is_thump_like`); `--all` prints every series with its flags.
//!
//! usage: cargo run --release -p cr-cli --example thumps -- PATH... [--all]

use std::path::PathBuf;
use std::time::Instant;

use cr_audio_io::AudioFileReader;
use cr_dsp::thump::{find_thumps, BEAT_FRACTION};

fn timestamp(frame: usize, sr: u32) -> String {
    let t = frame as f64 / sr as f64;
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let all = args.iter().any(|a| a == "--all");
    let mut files: Vec<PathBuf> = Vec::new();
    for a in args.iter().filter(|a| !a.starts_with("--")) {
        let p = PathBuf::from(a);
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&p)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    matches!(ext.as_str(), "wav" | "flac" | "aif" | "aiff")
                })
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(p);
        }
    }

    for path in &files {
        let started = Instant::now();
        let mut reader = AudioFileReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let sr = reader.meta.sample_rate;
        let frames = reader.read_all_frames().unwrap();
        let (events, series) = find_thumps(&frames, sr);
        let shown: Vec<_> = series.iter().filter(|s| all || s.is_thump_like()).collect();
        println!(
            "\n{}: {:.0} s, {} loud transients, {} rotational series: {} thump-like, {} off the record's period, \
             {} look like a beat [{:.1} s]",
            path.file_name().unwrap().to_string_lossy(),
            frames.len() as f64 / sr as f64,
            events.len(),
            series.len(),
            series.iter().filter(|s| s.is_thump_like()).count(),
            series.iter().filter(|s| !s.on_record_period).count(),
            series.iter().filter(|s| s.beat_fraction() > BEAT_FRACTION).count(),
            started.elapsed().as_secs_f64()
        );
        for s in shown {
            let n = s.events.len() as f64;
            println!(
                "  {} rpm, period {:.4} s, {} events, {} missed turns, timing {:.1} ms rms, beat slots {}/{}, \
                 {:.0} dB above (mean), {:.0} ms long (mean){}",
                s.speed,
                s.period_s,
                s.events.len(),
                s.missed_turns,
                s.timing_rms_ms,
                s.sub_period_hits,
                s.sub_period_slots,
                s.events.iter().map(|e| e.above_db).sum::<f64>() / n,
                s.events.iter().map(|e| e.duration_ms).sum::<f64>() / n,
                if s.is_thump_like() {
                    "  THUMP"
                } else if !s.on_record_period {
                    "  off-period"
                } else if s.beat_fraction() > BEAT_FRACTION {
                    "  BEAT?"
                } else {
                    ""
                }
            );
            let times: Vec<String> = s
                .events
                .iter()
                .map(|e| {
                    format!(
                        "{}{}",
                        timestamp(e.frame, sr),
                        match e.channels {
                            1 => "L",
                            2 => "R",
                            _ => "",
                        }
                    )
                })
                .collect();
            println!("    {}", times.join(" "));
        }
    }
}
