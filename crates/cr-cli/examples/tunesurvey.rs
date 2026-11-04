//! The auto-tuner's suggestion for every rip in the given folders or files: grade, whether it
//! was estimated (no silence between tracks), the dcl the grade maps to, the music-pickup
//! crossover, the suggested dcl, and the Reverse / Pitch Protection choices with their cut in
//! music repairs.
//!
//! usage: cargo run --release -p cr-cli --example tunesurvey -- PATH... > survey.tsv

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use cr_audio_io::AudioFileReader;

fn main() {
    let mut files: Vec<PathBuf> = Vec::new();
    for a in std::env::args().skip(1) {
        let p = PathBuf::from(&a);
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&p)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("wav")))
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(p);
        }
    }
    println!("file\tgrade\testimated\taudible/s\tgrade dcl\tpickup\tsuggested\treverse\treverse cut\tpitch\tpitch cut\tsecs");
    for path in files {
        let started = Instant::now();
        let mut r = AudioFileReader::open(&path).unwrap();
        let (sr, ch) = (r.meta.sample_rate, r.meta.channels);
        let frames = r.read_all_frames().unwrap();
        let a = cr_dsp::analysis::analyze_album(&frames, sr, ch, |_, _| {}, &AtomicBool::new(false)).unwrap();
        let t = cr_dsp::autotune::autotune_audio_with_analysis(&frames, sr, ch, Some(&a), |_, _| {});
        println!(
            "{}\t{}\t{}\t{:.2}\t{}\t{}\t{}\t{}\t{:+.0}%\t{}\t{:+.0}%\t{:.0}",
            path.file_stem().unwrap().to_string_lossy(),
            t.condition,
            if t.condition_estimated { "yes" } else { "" },
            t.audible_clicks_per_sec,
            cr_dsp::autotune::dcl_for_audible_rate(t.audible_clicks_per_sec),
            t.music_pickup_dcl.map_or("-".to_string(), |d| d.to_string()),
            t.recommended.dcl,
            if t.recommended.reverse { "on" } else { "off" },
            -100.0 * t.reverse_music_cut,
            if t.recommended.detect_pitch { "on" } else { "off" },
            -100.0 * t.pitch_music_cut,
            started.elapsed().as_secs_f64()
        );
    }
}
