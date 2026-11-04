//! Export: names from the template, the files written (decoded again and compared inside
//! `export_album`), tags, cover, sidecars and the refusal to overwrite.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use cr_tracks::cover::Cover;
use cr_tracks::export::{export_album, find_sidecars, render_template, track_paths, ExportJob, ExportOptions, DEFAULT_TEMPLATE};
use cr_tracks::{Track, TrackSheet};

const SR: u32 = 8000;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cr-tracks-export-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Three tracks over 6 s; the third after a gap (side change).
fn sheet() -> TrackSheet {
    let mut s = TrackSheet::new("Album.wav", SR, 6 * SR as u64);
    s.album.album_artist = "AC/DC".into();
    s.album.title = "Back In Black".into();
    s.album.year = Some(1980);
    s.album.genre = "Rock; Hard Rock".into();
    s.album.discogs_release_id = Some(4093774);
    let t = |title: &str, artist: &str, a: u64, b: u64, linked| Track {
        title: title.into(),
        artist: artist.into(),
        start: a * SR as u64 / 2,
        end: b * SR as u64 / 2,
        linked,
        ..Default::default()
    };
    s.tracks = vec![
        t("Hells Bells", "AC/DC", 0, 3, false),
        t("What: Do You Do?", "AC/DC", 3, 6, true),
        t("Back In Black", "Guest", 8, 12, false),
    ];
    s
}

/// A rip whose samples use the full 25-bit range, so any lost bit would show.
fn frames(n: usize) -> Vec<(i32, i32)> {
    let mut seed = 7u32;
    (0..n)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let l = ((seed >> 7) as i32) - (1 << 24);
            (l, -l / 3)
        })
        .collect()
}

#[test]
fn test_template_names() {
    let s = sheet();
    assert_eq!(render_template(DEFAULT_TEMPLATE, &s, 0), "AC_DC\\Back In Black\\01 - Hells Bells");
    // Characters names cannot hold are replaced in the values, not in the template.
    assert_eq!(render_template(DEFAULT_TEMPLATE, &s, 1), "AC_DC\\Back In Black\\02 - What_ Do You Do_");
    assert_eq!(render_template("%year% %album%/$num(%track%,3) %artist% - %title%", &s, 2), "1980 Back In Black/003 Guest - Back In Black");
    // Unknown fields stay visible.
    assert_eq!(render_template("%nope% $num(x,2)", &s, 0), "%nope% x");

    let opts = ExportOptions { root: PathBuf::from("out"), ..Default::default() };
    let paths = track_paths(&s, &opts).unwrap();
    assert_eq!(paths[0], Path::new("out").join("AC_DC").join("Back In Black").join("01 - Hells Bells.flac"));
    // Two tracks with the same name are refused.
    let opts = ExportOptions { template: "%album%".into(), ..opts };
    assert!(track_paths(&s, &opts).is_err());
}

#[test]
fn test_export_writes_exact_tagged_files_with_cover_and_sidecars() {
    let dir = temp_dir("full");
    // The rip with a capture sidecar, a track sheet, labels, and an unrelated file.
    let src = dir.join("rip").join("Album_repaired.wav");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, b"not read").unwrap();
    for f in ["Album_repaired.wav.cleangroove-tracks.json", "Album.wav.cleangroove-meta.json", "Album_labels.txt", "Other.wav.cleangroove-meta.json", "Album.wav"] {
        std::fs::write(src.parent().unwrap().join(f), b"x").unwrap();
    }
    let side: Vec<String> = find_sidecars(&src).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(side, ["Album.wav.cleangroove-meta.json", "Album_labels.txt", "Album_repaired.wav.cleangroove-tracks.json"]);

    // A small PNG cover: converted to JPEG for embedding and cover.jpg.
    let mut png = Vec::new();
    image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 30])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let cover = Cover::from_bytes(png).unwrap();

    let s = sheet();
    let audio = frames(s.total_frames as usize);
    let opts = ExportOptions { root: dir.join("out"), ..Default::default() };
    let job = ExportJob { sheet: &s, frames: &audio, channels: 2, source_audio: Some(&src), cover: Some(&cover) };
    let report = export_album(&job, &opts, |_, _| {}, &AtomicBool::new(false)).unwrap();

    assert_eq!(report.files.len(), 3);
    let album = dir.join("out").join("AC_DC").join("Back In Black");
    assert_eq!(report.album_dir, album);
    for f in ["cover.jpg", "album.cleangroove-tracks.json", "CleanGroove log.txt", "Album.wav.cleangroove-meta.json", "Album_labels.txt"] {
        assert!(album.join(f).exists(), "{f} missing");
    }
    assert!(!album.join("Other.wav.cleangroove-meta.json").exists());
    assert!(std::fs::read(album.join("cover.jpg")).unwrap().starts_with(&[0xFF, 0xD8, 0xFF]));

    let tag = metaflac::Tag::read_from_path(album.join("03 - Back In Black.flac")).unwrap();
    let get = |k: &str| tag.get_vorbis(k).map(|v| v.collect::<Vec<_>>().join("|")).unwrap_or_default();
    assert_eq!(get("ALBUMARTIST"), "AC/DC");
    assert_eq!(get("ARTIST"), "Guest");
    assert_eq!(get("TRACKNUMBER"), "03");
    assert_eq!(get("TRACKTOTAL"), "3");
    assert_eq!(get("GENRE"), "Rock|Hard Rock");
    assert_eq!(get("DISCOGS_RELEASE_ID"), "4093774");
    assert_eq!(get("DATE"), "1980");
    assert_eq!(tag.pictures().count(), 1);

    // The log lists the tracks; the 24-bit stream is 1.5 s long.
    let log = std::fs::read_to_string(album.join("CleanGroove log.txt")).unwrap();
    assert!(log.contains("03  0:04.000") && log.contains("Back In Black / Guest"), "{log}");
    let r = cr_audio_io::AudioFileReader::open(album.join("01 - Hells Bells.flac")).unwrap();
    assert_eq!((r.meta.bits_per_sample, r.meta.channels, r.meta.total_frames), (24, 2, 3 * SR as usize / 2));

    // A second export stops before writing, unless told to overwrite.
    let again = export_album(&job, &opts, |_, _| {}, &AtomicBool::new(false));
    assert!(again.unwrap_err().to_string().contains("already there"));
    let opts = ExportOptions { overwrite: true, ..opts };
    assert_eq!(export_album(&job, &opts, |_, _| {}, &AtomicBool::new(false)).unwrap().files.len(), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_export_16_bit_mono_without_cover() {
    let dir = temp_dir("mono");
    let s = sheet();
    let audio = frames(s.total_frames as usize);
    let opts = ExportOptions { root: dir.clone(), bits: 16, template: "$num(%track%,2)".into(), ..Default::default() };
    let job = ExportJob { sheet: &s, frames: &audio, channels: 1, source_audio: None, cover: None };
    let report = export_album(&job, &opts, |_, _| {}, &AtomicBool::new(false)).unwrap();
    let r = cr_audio_io::AudioFileReader::open(&report.files[2]).unwrap();
    assert_eq!((r.meta.bits_per_sample, r.meta.channels), (16, 1));
    assert!(!dir.join("cover.jpg").exists());
    assert_eq!(metaflac::Tag::read_from_path(&report.files[0]).unwrap().pictures().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_export_refuses_broken_splits() {
    let mut s = sheet();
    s.tracks[1].start = 100;
    let audio = frames(s.total_frames as usize);
    let job = ExportJob { sheet: &s, frames: &audio, channels: 2, source_audio: None, cover: None };
    let opts = ExportOptions { root: temp_dir("broken"), ..Default::default() };
    let e = export_album(&job, &opts, |_, _| {}, &AtomicBool::new(false)).unwrap_err().to_string();
    assert!(e.contains("need fixing"), "{e}");
}
