//! Prints the first guess at the track splits for a whole-album rip and a Discogs release.
//!
//! usage: cargo run --release -p cr-tracks --example propose -- AUDIO RELEASE_ID
//!            [--cache DIR] [--labels FILE] [--save]
//!
//! The release is fetched (with `DISCOGS_TOKEN` if set; release details also work without), or read from `DIR/<id>.json` when `--cache`
//! has it (and saved there after a fetch). `--labels` writes an Audacity label track to check
//! the splits by ear; `--save` writes `<AUDIO>.cleangroove-tracks.json`.

use std::path::{Path, PathBuf};

use cr_audio_io::AudioFileReader;
use cr_tracks::{place_tracks, Envelope, ReleaseInfo, TrackSheet};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let save = args.iter().any(|a| a == "--save");
    let skip: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--cache" || *a == "--labels")
        .map(|(i, _)| i + 1)
        .collect();
    let positional: Vec<&String> = args.iter().enumerate().filter(|(i, a)| !a.starts_with("--") && !skip.contains(i)).map(|(_, a)| a).collect();
    let [audio, id] = positional[..] else {
        eprintln!("usage: propose AUDIO RELEASE_ID [--cache DIR] [--labels FILE] [--save]");
        std::process::exit(2);
    };
    let id: u64 = id.parse().expect("RELEASE_ID must be a number");

    let json = release_json(id, opt("--cache").map(PathBuf::from));
    let release = cr_discogs::parse_release(&json).unwrap_or_else(|e| panic!("{e}"));
    let info = ReleaseInfo::from_discogs(&release);

    let t0 = std::time::Instant::now();
    let mut reader = AudioFileReader::open(audio).unwrap_or_else(|e| panic!("{audio}: {e}"));
    let sr = reader.meta.sample_rate;
    let frames = reader.read_all_frames().unwrap();
    let env = Envelope::from_frames(&frames, sr);
    let name = Path::new(audio).file_name().unwrap().to_string_lossy();
    let p = place_tracks(&info, &env, frames.len() as u64, &name);
    let s = &p.sheet;

    println!("{} - {} ({}), Discogs {}", s.album.album_artist, s.album.title, s.album.year.unwrap_or(0), id);
    println!(
        "{name}: {}, quiet below {:.1} dBFS (above {} Hz); side scales {:?}; {:.1} s",
        hms(frames.len() as f64 / sr as f64),
        env.quiet_threshold(),
        cr_tracks::envelope::HIGHPASS_HZ,
        p.side_scales.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>(),
        t0.elapsed().as_secs_f64()
    );
    println!("#   pos  start      end        length   discogs  diff    link  placed       title / artist");
    for (i, t) in s.tracks.iter().enumerate() {
        let len = s.seconds(t.len());
        let note = &p.notes[i];
        let (disc, diff) = match t.source_seconds {
            Some(d) => (hms(d), format!("{:+.1}", len - d)),
            None => ("-".into(), "-".into()),
        };
        let artist = if t.artist == s.album.album_artist { String::new() } else { format!(" / {}", t.artist) };
        println!(
            "{}  {:<4} {:<10} {:<10} {:<8} {:<8} {:<7} {:<5} {:<12} {}{}",
            s.number_label(i),
            t.source_position,
            hms(s.seconds(t.start)),
            hms(s.seconds(t.end)),
            hms(len),
            disc,
            diff,
            if t.linked { "yes" } else { "" },
            format!("{:?} {:+.1}", note.snap, note.shift_s),
            t.title,
            artist
        );
    }
    if !info.omitted.is_empty() {
        let pos: Vec<_> = info.omitted.iter().map(|t| t.position.as_str()).collect();
        println!("left out {} tracks of another medium in this release: {}", pos.len(), pos.join(" "));
    }
    for w in &p.warnings {
        println!("warning: {w}");
    }
    for c in s.check() {
        println!("problem: {c}");
    }
    if let Some(f) = opt("--labels") {
        std::fs::write(&f, s.audacity_labels()).unwrap();
        println!("labels: {f}");
    }
    if save {
        let f = TrackSheet::sidecar_path(audio);
        s.save(&f).unwrap();
        println!("saved: {}", f.display());
    }
}

fn release_json(id: u64, cache: Option<PathBuf>) -> String {
    let cached = cache.as_ref().map(|d| d.join(format!("{id}.json")));
    if let Some(Ok(j)) = cached.as_ref().map(std::fs::read_to_string) {
        return j;
    }
    let client = cr_discogs::Client::from_env().unwrap_or_else(|_| cr_discogs::Client::anonymous());
    let j = client.release_json(id).unwrap_or_else(|e| panic!("{e}"));
    if let Some(f) = cached {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, &j).unwrap();
    }
    j
}

fn hms(s: f64) -> String {
    let ms = (s * 1000.0).round() as u64;
    format!("{}:{:02}.{:01}", ms / 60_000, ms / 1000 % 60, ms / 100 % 10)
}
