//! Album metadata and track splits for a whole-album rip.
//!
//! A `TrackSheet` holds the album fields and the tracks, each a span of sample frames in the
//! rip. A track is *linked* when it starts exactly where the previous one ends (one boundary,
//! moved as one); an unlinked track has dead space before it (lead-in, side change, run-out)
//! that no track gets. The sheet is saved next to the audio as `<file>.cleangroove-tracks.json`.
//!
//! Tracks are numbered 01..N straight through the album; the provider's vinyl position ("A1")
//! is kept only as source information. Album artist and track artist are separate fields.
//!
//! `place` makes the first guess at the splits from the provider's track lengths and the quiet
//! between tracks; `release` turns a provider's release into album fields and a track list.

pub mod cover;
pub mod envelope;
pub mod export;
pub mod flac;
pub mod place;
pub mod providers;
pub mod release;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use envelope::Envelope;
pub use place::{place_tracks, BoundaryNote, Placement, Snap};
pub use release::{ReleaseInfo, ReleaseTrack};

/// A boundary placed by hand settles on the quietest sample within this many seconds: a
/// track played on its own then starts and ends close to zero, without a click, and the move
/// is too small to see or hear. Linked tracks share the boundary, so gapless playback is exact
/// wherever it falls.
pub const SETTLE_S: f64 = 0.005;

/// The frame within `radius` of `frame` where |L| + |R| is smallest (the nearest on a tie).
/// The start and end of the file stay where they are.
pub fn quiet_sample(frames: &[(i32, i32)], frame: u64, radius: u64) -> u64 {
    if frame == 0 || frame >= frames.len() as u64 {
        return frame;
    }
    let last = frames.len() as u64 - 1;
    let lo = frame.saturating_sub(radius).min(last);
    let hi = (frame + radius).min(last);
    (lo..=hi)
        .min_by_key(|&f| {
            let (l, r) = frames[f as usize];
            ((l as i64).abs() + (r as i64).abs(), f.abs_diff(frame))
        })
        .unwrap_or(frame)
}

/// Sidecar format version written by this code.
pub const SHEET_VERSION: u32 = 1;
/// Shortest track the editing operations allow, in seconds.
pub const MIN_TRACK_S: f64 = 1.0;

#[derive(Error, Debug)]
pub enum TracksError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not read the track sheet: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported track sheet version {0}")]
    Version(u32),
}

/// Album-level fields, written to every track's tags.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Album {
    pub album_artist: String,
    pub title: String,
    pub year: Option<u32>,
    pub label: String,
    pub catalog_number: String,
    pub genre: String,
    pub style: String,
    pub discogs_release_id: Option<u64>,
    pub musicbrainz_release_id: Option<String>,
    /// The front cover: an image file next to the rip, by name.
    pub cover_file: Option<String>,
}

/// One track: a span of the rip, in sample frames (`start..end`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub title: String,
    /// The track artist; on most records the same as the album artist.
    pub artist: String,
    pub start: u64,
    pub end: u64,
    /// Starts exactly where the previous track ends. Always false for the first track.
    pub linked: bool,
    /// Position as the provider prints it ("A1"), source information only.
    pub source_position: String,
    /// Length the provider gives, in seconds, for comparison with the placed length.
    pub source_seconds: Option<f64>,
}

impl Track {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// The album and its tracks for one rip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackSheet {
    pub version: u32,
    /// File name of the rip the frames refer to (no directory).
    pub source_file: String,
    pub sample_rate: u32,
    pub total_frames: u64,
    pub album: Album,
    pub tracks: Vec<Track>,
}

impl TrackSheet {
    pub fn new(source_file: impl Into<String>, sample_rate: u32, total_frames: u64) -> Self {
        TrackSheet {
            version: SHEET_VERSION,
            source_file: source_file.into(),
            sample_rate,
            total_frames,
            album: Album::default(),
            tracks: Vec::new(),
        }
    }

    /// The track number as written in names and tags: "01", "02", ... (at least two digits).
    pub fn number_label(&self, index: usize) -> String {
        let width = self.tracks.len().to_string().len().max(2);
        format!("{:0width$}", index + 1)
    }

    pub fn seconds(&self, frames: u64) -> f64 {
        frames as f64 / self.sample_rate.max(1) as f64
    }

    fn min_len(&self) -> u64 {
        (MIN_TRACK_S * self.sample_rate as f64) as u64
    }

    /// Moves the start of track `i`, clamped so no track gets shorter than `MIN_TRACK_S` and
    /// none overlap. A linked start moves the previous track's end with it.
    pub fn set_start(&mut self, i: usize, frame: u64) {
        let min = self.min_len();
        let linked = i > 0 && self.tracks[i].linked;
        let lo = match i {
            0 => 0,
            _ if linked => self.tracks[i - 1].start + min,
            _ => self.tracks[i - 1].end,
        };
        let hi = self.tracks[i].end.saturating_sub(min).max(lo);
        let f = frame.clamp(lo, hi);
        self.tracks[i].start = f;
        if linked {
            self.tracks[i - 1].end = f;
        }
    }

    /// Moves the end of track `i`; a linked next track's start moves with it.
    pub fn set_end(&mut self, i: usize, frame: u64) {
        let min = self.min_len();
        let next_linked = self.tracks.get(i + 1).is_some_and(|t| t.linked);
        let lo = self.tracks[i].start + min;
        let hi = match self.tracks.get(i + 1) {
            None => self.total_frames,
            Some(n) if next_linked => n.end.saturating_sub(min),
            Some(n) => n.start,
        }
        .max(lo);
        let f = frame.clamp(lo, hi);
        self.tracks[i].end = f;
        if next_linked {
            self.tracks[i + 1].start = f;
        }
    }

    /// Links track `i` to the previous one: the previous track is extended to where this one
    /// starts, so the dead space between them becomes the previous track's tail.
    pub fn link(&mut self, i: usize) {
        if i == 0 || i >= self.tracks.len() {
            return;
        }
        self.tracks[i - 1].end = self.tracks[i].start;
        self.tracks[i].linked = true;
    }

    /// Unlinks track `i` from the previous one; nothing moves until a side is dragged.
    pub fn unlink(&mut self, i: usize) {
        if let Some(t) = self.tracks.get_mut(i) {
            t.linked = false;
        }
    }

    /// Splits the track that contains `frame` there; the new second half is linked to the
    /// first. On an empty sheet the whole file becomes one track first. Returns the index of
    /// the new track, or None when `frame` is in dead space or too near a track's ends.
    pub fn split_at(&mut self, frame: u64) -> Option<usize> {
        if self.tracks.is_empty() {
            let artist = self.album.album_artist.clone();
            self.tracks.push(Track { end: self.total_frames, artist, ..Default::default() });
        }
        let min = self.min_len();
        let i = self.tracks.iter().position(|t| frame >= t.start + min && frame + min <= t.end)?;
        let t = &mut self.tracks[i];
        let second = Track {
            start: frame,
            end: t.end,
            linked: true,
            artist: t.artist.clone(),
            ..Default::default()
        };
        t.end = frame;
        self.tracks.insert(i + 1, second);
        Some(i + 1)
    }

    /// Merges track `i` into the previous one, which then ends where `i` ended (the dead space
    /// before `i`, if any, joins it). The previous track keeps its title.
    pub fn merge_with_previous(&mut self, i: usize) {
        if i == 0 || i >= self.tracks.len() {
            return;
        }
        let t = self.tracks.remove(i);
        self.tracks[i - 1].end = t.end;
    }

    /// Removes track `i`; its span becomes dead space, so the next track is no longer linked.
    pub fn remove(&mut self, i: usize) {
        if i >= self.tracks.len() {
            return;
        }
        self.tracks.remove(i);
        if let Some(n) = self.tracks.get_mut(i) {
            n.linked = false;
        }
    }

    /// Problems with the sheet, empty when it is consistent.
    pub fn check(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            let n = self.number_label(i);
            if t.is_empty() {
                out.push(format!("track {n} ends before it starts"));
            }
            if t.end > self.total_frames {
                out.push(format!("track {n} ends after the end of the file"));
            }
            if i == 0 && t.linked {
                out.push("the first track cannot be linked".into());
            }
            if i > 0 {
                let p = &self.tracks[i - 1];
                if t.start < p.end {
                    out.push(format!("track {n} overlaps the previous track"));
                }
                if t.linked && t.start != p.end {
                    out.push(format!("track {n} is linked but does not start where the previous one ends"));
                }
            }
        }
        out
    }

    /// `<audio file>.cleangroove-tracks.json`.
    pub fn sidecar_path(audio: impl AsRef<Path>) -> PathBuf {
        let mut s = audio.as_ref().as_os_str().to_os_string();
        s.push(".cleangroove-tracks.json");
        PathBuf::from(s)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), TracksError> {
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, TracksError> {
        let sheet: TrackSheet = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if sheet.version > SHEET_VERSION {
            return Err(TracksError::Version(sheet.version));
        }
        Ok(sheet)
    }

    /// Audacity label track: one region label per track, "01 Title".
    pub fn audacity_labels(&self) -> String {
        self.tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!("{:.6}\t{:.6}\t{} {}\n", self.seconds(t.start), self.seconds(t.end), self.number_label(i), t.title)
            })
            .collect()
    }
}
