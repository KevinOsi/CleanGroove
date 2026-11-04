//! Exports a rip's tracks as tagged FLAC, from its saved track sheet (`<rip>.cleangroove-tracks.json`),
//! with the front cover fetched for its release.
//!
//! usage: cargo run --release -p cr-tracks --example export -- RIP OUT_ROOT [--bits 16|24] [--overwrite]
use std::path::Path;
use std::sync::atomic::AtomicBool;

use cr_tracks::cover::fetch_cover;
use cr_tracks::export::{export_album, ExportJob, ExportOptions};
use cr_tracks::TrackSheet;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (rip, root) = (Path::new(&a[0]), Path::new(&a[1]));
    let bits = a.iter().position(|x| x == "--bits").and_then(|i| a.get(i + 1)).map_or(24, |b| b.parse().unwrap());
    let sheet = TrackSheet::load(TrackSheet::sidecar_path(rip)).unwrap_or_else(|e| panic!("{e}"));
    let mut reader = cr_audio_io::AudioFileReader::open(rip).unwrap();
    let channels = reader.meta.channels;
    let frames = reader.read_all_frames().unwrap();
    let discogs = cr_discogs::Client::from_env().unwrap_or_else(|_| cr_discogs::Client::anonymous());
    let cover = match fetch_cover(&sheet.album, &discogs, "") {
        Ok((c, from)) => {
            println!("cover from {} ({} bytes, {:?})", from, c.bytes.len(), c.dimensions());
            Some(c)
        }
        Err(e) => {
            println!("no cover: {e}");
            None
        }
    };
    let opts = ExportOptions { root: root.into(), bits, overwrite: a.iter().any(|x| x == "--overwrite"), ..Default::default() };
    let job = ExportJob { sheet: &sheet, frames: &frames, channels, source_audio: Some(rip), cover: cover.as_ref() };
    let report = export_album(&job, &opts, |f, m| println!("{:>3.0}% {}", f * 100.0, m), &AtomicBool::new(false)).unwrap_or_else(|e| panic!("{e}"));
    println!("{} files in {} ({:.1} s), {} sidecars copied", report.files.len(), report.album_dir.display(), report.seconds, report.sidecars.len());
}
