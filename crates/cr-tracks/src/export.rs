//! Exporting an album: one tagged FLAC per track, named from a template, into an album
//! folder that also gets the cover, the track sheet, copies of the rip's sidecar files and a
//! log of what was done.
//!
//! Every file is decoded again after writing and compared sample for sample with its span of
//! the rip before it takes its final name, so a finished export is known to be exact.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cr_dsp::params::{internal_to_i16, internal_to_i24_clamped, scale_i16_to_internal, scale_i24_to_internal};
use thiserror::Error;

use crate::cover::Cover;
use crate::TrackSheet;

/// The default name template: `Album Artist\Album\01 - Title.flac`.
pub const DEFAULT_TEMPLATE: &str = "%albumartist%\\%album%\\$num(%track%,2) - %title%";

#[derive(Error, Debug)]
pub enum ExportError {
    #[error("{0}")]
    Plan(String),
    #[error("I/O error on {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("FLAC encoding failed for {0}: {1}")]
    Encode(PathBuf, String),
    #[error("tagging failed for {0}: {1}")]
    Tag(PathBuf, String),
    #[error("{0} does not decode to the same audio as the rip ({1})")]
    Verify(PathBuf, String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportOptions {
    /// The folder the template's path starts in.
    pub root: PathBuf,
    pub template: String,
    /// 16 or 24.
    pub bits: u16,
    /// Replace files that are already there; otherwise the export stops before writing.
    pub overwrite: bool,
    /// Embed the cover in every track.
    pub embed_cover: bool,
    /// Write `cover.jpg` in the album folder.
    pub cover_jpg: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { root: PathBuf::new(), template: DEFAULT_TEMPLATE.into(), bits: 24, overwrite: false, embed_cover: true, cover_jpg: true }
    }
}

/// What to export.
pub struct ExportJob<'a> {
    pub sheet: &'a TrackSheet,
    /// The whole rip, in the internal sample domain.
    pub frames: &'a [(i32, i32)],
    /// 1 writes mono files (the left channel), 2 stereo.
    pub channels: u16,
    /// The rip itself: its sidecar files are copied, and the log names it.
    pub source_audio: Option<&'a Path>,
    pub cover: Option<&'a Cover>,
}

#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub album_dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub sidecars: Vec<PathBuf>,
    pub log: PathBuf,
    pub seconds: f64,
}

/// Characters a file or folder name cannot hold (Windows is the strictest).
fn clean_component(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '"' => '\'',
            '<' | '>' | ':' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces.
    let t = mapped.trim().trim_end_matches(['.', ' ']).to_string();
    if t.is_empty() {
        "_".into()
    } else {
        t
    }
}

/// The value of a template field for track `i`.
fn field(sheet: &TrackSheet, i: usize, name: &str) -> Option<String> {
    let a = &sheet.album;
    let t = &sheet.tracks[i];
    Some(match name.to_ascii_lowercase().as_str() {
        "albumartist" | "album artist" => if a.album_artist.is_empty() { t.artist.clone() } else { a.album_artist.clone() },
        "album" => a.title.clone(),
        "artist" => t.artist.clone(),
        "title" => t.title.clone(),
        "track" | "tracknumber" => (i + 1).to_string(),
        "tracktotal" | "totaltracks" => sheet.tracks.len().to_string(),
        "year" | "date" => a.year.map(|y| y.to_string()).unwrap_or_default(),
        "label" => a.label.clone(),
        "catalog" | "catalognumber" => a.catalog_number.clone(),
        "genre" => a.genre.clone(),
        _ => return None,
    })
}

/// Renders an MP3tag-style template for track `i`: `%field%` and `$num(x,n)`. Field values
/// have path separators and other characters names cannot hold replaced; the template's own
/// `\` or `/` make folders.
pub fn render_template(template: &str, sheet: &TrackSheet, i: usize) -> String {
    let chars: Vec<char> = template.chars().collect();
    render_span(&chars, sheet, i)
}

fn render_span(c: &[char], sheet: &TrackSheet, i: usize) -> String {
    let mut out = String::new();
    let mut k = 0;
    while k < c.len() {
        if c[k] == '%' {
            if let Some(end) = c[k + 1..].iter().position(|&x| x == '%') {
                let name: String = c[k + 1..k + 1 + end].iter().collect();
                match field(sheet, i, &name) {
                    Some(v) => out.push_str(&clean_component(&v)),
                    None => out.push_str(&format!("%{}%", name)),
                }
                k += end + 2;
                continue;
            }
        }
        if c[k] == '$' {
            let rest: String = c[k..].iter().take(5).collect();
            if rest.eq_ignore_ascii_case("$num(") {
                // Find the matching ')' and the top-level ','.
                let (mut depth, mut comma, mut close) = (0i32, None, None);
                for (j, &ch) in c.iter().enumerate().skip(k + 5) {
                    match ch {
                        '(' => depth += 1,
                        ')' if depth == 0 => {
                            close = Some(j);
                            break;
                        }
                        ')' => depth -= 1,
                        ',' if depth == 0 && comma.is_none() => comma = Some(j),
                        _ => {}
                    }
                }
                if let (Some(comma), Some(close)) = (comma, close) {
                    let value = render_span(&c[k + 5..comma], sheet, i);
                    let width: usize = c[comma + 1..close].iter().collect::<String>().trim().parse().unwrap_or(0);
                    out.push_str(&match value.trim().parse::<u64>() {
                        Ok(n) => format!("{:0width$}", n, width = width),
                        Err(_) => value,
                    });
                    k = close + 1;
                    continue;
                }
            }
        }
        out.push(c[k]);
        k += 1;
    }
    out
}

/// The file each track is written to.
pub fn track_paths(sheet: &TrackSheet, opts: &ExportOptions) -> Result<Vec<PathBuf>, ExportError> {
    if sheet.tracks.is_empty() {
        return Err(ExportError::Plan("there are no tracks to export".into()));
    }
    let problems = sheet.check();
    if !problems.is_empty() {
        return Err(ExportError::Plan(format!("the track splits need fixing first: {}", problems.join("; "))));
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for i in 0..sheet.tracks.len() {
        let rel = render_template(&opts.template, sheet, i);
        let mut p = opts.root.clone();
        for part in rel.split(['\\', '/']).filter(|s| !s.trim().is_empty()) {
            p.push(clean_component(part));
        }
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        p.set_file_name(format!("{}.flac", name));
        if paths.iter().any(|q| q.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy())) {
            return Err(ExportError::Plan(format!(
                "two tracks would be written to {}; give every track a title, or put %track% in the name template",
                p.display()
            )));
        }
        paths.push(p);
    }
    Ok(paths)
}

/// The album folder: the folder of the first track.
pub fn album_dir(paths: &[PathBuf]) -> PathBuf {
    paths.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_default()
}

/// The rip's sidecar files: everything next to it named after it (`name.wav.cleangroove-meta.json`,
/// `name.wav.cleangroove-tracks.json`, `name_labels.txt`, ...), and those of the raw take it was
/// rendered from (`name_repaired.wav` comes from `name.wav`). Audio files are left out.
pub fn find_sidecars(audio: &Path) -> Vec<PathBuf> {
    let Some(dir) = audio.parent() else { return Vec::new() };
    let file = audio.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = audio.file_stem().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let mut stems = vec![stem.clone()];
    if let Some(raw) = stem.strip_suffix("_repaired") {
        stems.push(raw.to_string());
    }
    let audio_ext = ["wav", "flac", "aif", "aiff", "mp3", "m4a", "ogg", "aup3", "jpg", "jpeg", "png"];
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.as_path() != audio)
        .filter(|p| {
            let name = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let ours = name.starts_with(&format!("{}.", file))
                || stems.iter().any(|s| name.starts_with(&format!("{}.", s)) || name.starts_with(&format!("{}_", s)));
            ours && !audio_ext.contains(&ext.as_str())
        })
        .collect();
    out.sort();
    out
}

/// One track's samples, interleaved, at the export bit depth.
fn track_samples(frames: &[(i32, i32)], channels: u16, bits: u16) -> Vec<i32> {
    let conv = |x: i32| if bits == 16 { internal_to_i16(x) as i32 } else { internal_to_i24_clamped(x) };
    let mut out = Vec::with_capacity(frames.len() * channels as usize);
    for &(l, r) in frames {
        out.push(conv(l));
        if channels == 2 {
            out.push(conv(r));
        }
    }
    out
}

fn encode_flac(path: &Path, samples: &[i32], channels: u16, bits: u16, sample_rate: u32) -> Result<(), ExportError> {
    let bytes = crate::flac::encode(samples, channels as usize, bits as u32, sample_rate);
    std::fs::write(path, bytes).map_err(|e| ExportError::Io(path.into(), e))
}

/// The Vorbis comments for track `i`.
pub fn track_tags(sheet: &TrackSheet, i: usize) -> Vec<(&'static str, Vec<String>)> {
    let a = &sheet.album;
    let t = &sheet.tracks[i];
    let split = |s: &str| s.split(';').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
    let album_artist = if a.album_artist.is_empty() { t.artist.clone() } else { a.album_artist.clone() };
    let mut tags: Vec<(&'static str, Vec<String>)> = vec![
        ("ALBUMARTIST", vec![album_artist]),
        ("ARTIST", vec![t.artist.clone()]),
        ("ALBUM", vec![a.title.clone()]),
        ("TITLE", vec![t.title.clone()]),
        ("TRACKNUMBER", vec![sheet.number_label(i)]),
        ("TRACKTOTAL", vec![sheet.tracks.len().to_string()]),
        ("DATE", a.year.map(|y| y.to_string()).into_iter().collect()),
        ("LABEL", vec![a.label.clone()]),
        ("CATALOGNUMBER", vec![a.catalog_number.clone()]),
        ("GENRE", split(&a.genre)),
        ("STYLE", split(&a.style)),
        ("DISCOGS_RELEASE_ID", a.discogs_release_id.map(|id| id.to_string()).into_iter().collect()),
        ("MUSICBRAINZ_ALBUMID", a.musicbrainz_release_id.clone().into_iter().collect()),
    ];
    for (_, v) in tags.iter_mut() {
        v.retain(|s| !s.trim().is_empty());
    }
    tags.retain(|(_, v)| !v.is_empty());
    tags
}

fn tag_flac(path: &Path, tags: &[(&'static str, Vec<String>)], cover: Option<&Cover>) -> Result<(), ExportError> {
    let err = |e: metaflac::Error| ExportError::Tag(path.into(), e.to_string());
    let mut tag = metaflac::Tag::read_from_path(path).map_err(err)?;
    for (k, v) in tags {
        tag.set_vorbis(*k, v.clone());
    }
    if let Some(c) = cover {
        tag.add_picture(c.mime, metaflac::block::PictureType::CoverFront, c.bytes.clone());
    }
    tag.save().map_err(err)
}

/// Decodes `path` and compares it with what was meant to be written.
fn verify_flac(path: &Path, expected: &[(i32, i32)], channels: u16, bits: u16) -> Result<(), ExportError> {
    let fail = |m: String| ExportError::Verify(path.into(), m);
    let mut reader = cr_audio_io::AudioFileReader::open(path).map_err(|e| fail(e.to_string()))?;
    if reader.meta.bits_per_sample != bits || reader.meta.channels != channels {
        return Err(fail(format!("{} bit, {} ch", reader.meta.bits_per_sample, reader.meta.channels)));
    }
    let got = reader.read_all_frames().map_err(|e| fail(e.to_string()))?;
    if got.len() != expected.len() {
        return Err(fail(format!("{} frames instead of {}", got.len(), expected.len())));
    }
    let back = |x: i32| if bits == 16 { scale_i16_to_internal(internal_to_i16(x)) } else { scale_i24_to_internal(internal_to_i24_clamped(x)) };
    for (k, (&(gl, gr), &(el, er))) in got.iter().zip(expected).enumerate() {
        let (wl, wr) = (back(el), back(er));
        if gl != wl || (channels == 2 && gr != wr) {
            return Err(fail(format!("first difference at frame {}", k)));
        }
    }
    Ok(())
}

/// Exports the album. `progress` gets (fraction, message); stops between tracks when
/// `cancel` is set (files already finished stay).
pub fn export_album(job: &ExportJob, opts: &ExportOptions, progress: impl Fn(f32, &str), cancel: &AtomicBool) -> Result<ExportReport, ExportError> {
    let t0 = std::time::Instant::now();
    let sheet = job.sheet;
    if opts.bits != 16 && opts.bits != 24 {
        return Err(ExportError::Plan(format!("{} bit is not offered; choose 16 or 24", opts.bits)));
    }
    if sheet.tracks.iter().any(|t| t.end as usize > job.frames.len()) {
        return Err(ExportError::Plan("the track splits run past the end of the audio; is this the right file?".into()));
    }
    let paths = track_paths(sheet, opts)?;
    let dir = album_dir(&paths);
    if !opts.overwrite {
        let existing: Vec<_> = paths.iter().filter(|p| p.exists()).collect();
        if !existing.is_empty() {
            return Err(ExportError::Plan(format!(
                "{} of the files are already there (first: {}); tick Overwrite to replace them",
                existing.len(),
                existing[0].display()
            )));
        }
    }
    let channels = job.channels.clamp(1, 2);
    let cover = match (job.cover, opts.embed_cover || opts.cover_jpg) {
        (Some(c), true) => Some(c.as_jpeg().map_err(ExportError::Plan)?),
        _ => None,
    };

    let mut report = ExportReport { album_dir: dir.clone(), ..Default::default() };
    let n = sheet.tracks.len();
    for (i, path) in paths.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let t = &sheet.tracks[i];
        progress(i as f32 / n as f32, &format!("Track {} of {}: {}", i + 1, n, t.title));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ExportError::Io(parent.into(), e))?;
        }
        let span = &job.frames[t.start as usize..t.end as usize];
        let part = path.with_extension("flac.part");
        let result = (|| {
            encode_flac(&part, &track_samples(span, channels, opts.bits), channels, opts.bits, sheet.sample_rate)?;
            tag_flac(&part, &track_tags(sheet, i), cover.as_ref().filter(|_| opts.embed_cover))?;
            verify_flac(&part, span, channels, opts.bits)?;
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| ExportError::Io(path.clone(), e))?;
            }
            std::fs::rename(&part, path).map_err(|e| ExportError::Io(path.clone(), e))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result?;
        report.files.push(path.clone());
    }

    progress(1.0, "Cover, track sheet and log...");
    if let (Some(c), true) = (cover.as_ref(), opts.cover_jpg) {
        let p = dir.join("cover.jpg");
        std::fs::write(&p, &c.bytes).map_err(|e| ExportError::Io(p, e))?;
    }
    let sheet_copy = dir.join("album.cleangroove-tracks.json");
    sheet.save(&sheet_copy).map_err(|e| ExportError::Plan(e.to_string()))?;
    if let Some(src) = job.source_audio {
        for s in find_sidecars(src) {
            let to = dir.join(s.file_name().unwrap());
            std::fs::copy(&s, &to).map_err(|e| ExportError::Io(to.clone(), e))?;
            report.sidecars.push(to);
        }
    }
    report.seconds = t0.elapsed().as_secs_f64();
    report.log = dir.join("CleanGroove log.txt");
    let log = export_log(job, opts, &report, cover.as_ref());
    std::fs::write(&report.log, log).map_err(|e| ExportError::Io(report.log.clone(), e))?;
    Ok(report)
}

fn hms(frames: u64, sr: u32) -> String {
    let ms = (frames as f64 * 1000.0 / sr.max(1) as f64).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// The readable record of the export.
fn export_log(job: &ExportJob, opts: &ExportOptions, report: &ExportReport, cover: Option<&Cover>) -> String {
    let s = job.sheet;
    let a = &s.album;
    let mut out = String::new();
    let mut line = |l: String| {
        out.push_str(&l);
        out.push('\n');
    };
    line(format!("CleanGroove {} track export", env!("CARGO_PKG_VERSION")));
    line(String::new());
    line(format!("Album:        {} - {}", a.album_artist, a.title));
    line(format!("Year:         {}", a.year.map(|y| y.to_string()).unwrap_or_default()));
    line(format!("Label:        {}  {}", a.label, a.catalog_number));
    line(format!("Genre:        {}  ({})", a.genre, a.style));
    if let Some(id) = a.discogs_release_id {
        line(format!("Discogs:      https://www.discogs.com/release/{}", id));
    }
    if let Some(id) = &a.musicbrainz_release_id {
        line(format!("MusicBrainz:  https://musicbrainz.org/release/{}", id));
    }
    line(String::new());
    line(format!("Source:       {}", job.source_audio.map(|p| p.display().to_string()).unwrap_or_else(|| s.source_file.clone())));
    line(format!("              {} Hz, {}, {} frames ({})", s.sample_rate, if job.channels == 1 { "mono" } else { "stereo" }, s.total_frames, hms(s.total_frames, s.sample_rate)));
    line(format!("Written:      FLAC {} bit, {} tracks, name template {}", opts.bits, s.tracks.len(), opts.template));
    line(format!("Cover:        {}", match cover {
        Some(c) => format!("{} bytes{}{}", c.bytes.len(), if opts.embed_cover { ", embedded" } else { "" }, if opts.cover_jpg { ", cover.jpg" } else { "" }),
        None => "none".into(),
    }));
    line("Verified:     every file decoded again and matched the rip sample for sample".into());
    line(String::new());
    line("#   start        end          length     linked  title / artist".into());
    for (i, t) in s.tracks.iter().enumerate() {
        line(format!(
            "{}  {:<12} {:<12} {:<10} {:<7} {} / {}",
            s.number_label(i),
            hms(t.start, s.sample_rate),
            hms(t.end, s.sample_rate),
            hms(t.len(), s.sample_rate),
            if t.linked { "yes" } else { "" },
            t.title,
            t.artist
        ));
    }
    line(String::new());
    line("Files:".into());
    for f in &report.files {
        line(format!("  {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    }
    if !report.sidecars.is_empty() {
        line("Copied from the rip's folder (the originals stay there):".into());
        for f in &report.sidecars {
            line(format!("  {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    line("Track sheet:  album.cleangroove-tracks.json (open it with the rip to export again)".into());
    line(format!("Time:         {:.1} s", report.seconds));
    out
}
