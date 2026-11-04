//! First guess at the track splits of a whole-album rip.
//!
//! 1. Group the tracks by side (`ReleaseInfo::sides`).
//! 2. Each side's music starts at the first sustained loud stretch after the lead-in (so not at
//!    the needle drop), moved back over a count-in or quiet intro. Every side but the last
//!    ends at a long quiet stretch (run-out, the flip, the next lead-in; a needle lift or drop
//!    inside it does not break it): the one nearest where the side's track lengths say it
//!    should be, or the longest if lengths are missing. A fade-out stays with the music.
//! 3. Lay the track lengths end to end over the side's music, scaled so they fill it exactly
//!    (Discogs lengths are rounded, and the rip speed is never quite exact). A missing length
//!    gets the average of the side's known ones.
//! 4. The lengths come first: a boundary only moves to a quiet stretch within `SNAP_S` of where
//!    they put it (just before the next track's music); anything less clear, such as a segue,
//!    stays where the lengths put it for the user to place by ear. After a snap the following
//!    boundaries move by the same amount, so a late start does not add up along the side.
//!
//! Tracks on a side are linked; the first track of each side is not (the lead-in and side
//! change are dead space).

use crate::envelope::Envelope;
use crate::release::ReleaseInfo;
use crate::{Track, TrackSheet};

/// A track starts this long before its first music.
pub const PRE_ROLL_S: f64 = 0.3;
/// The last track of a side keeps this much after its music ends (the decay).
pub const TAIL_S: f64 = 1.5;
/// Shortest quiet stretch that counts as a gap between tracks.
pub const GAP_MIN_S: f64 = 0.4;
/// Shortest quiet stretch that can be a side change.
pub const SIDE_GAP_MIN_S: f64 = 2.0;
/// A boundary snaps to quiet within this many seconds of where the lengths put it.
pub const SNAP_S: f64 = 4.0;
/// A side change is looked for within this many seconds, or this fraction of the side's
/// length if larger, of where the lengths put it.
const SIDE_SEARCH_S: f64 = 30.0;
const SIDE_SEARCH_FRAC: f64 = 0.15;
/// Music is sustained: at least `MUSIC_FRAC` of the `MUSIC_MIN_S` after its start (or before
/// its end) is loud. A needle drop or lift is a loud burst of a second or so, not music.
const MUSIC_MIN_S: f64 = 3.0;
const MUSIC_FRAC: f64 = 0.8;
/// A track's first music, and a side's last, reach as far as the level stays `RISE_DB` above
/// the groove noise around them with no break longer than `BRIDGE_S`: a count-in, a quiet
/// intro, a fade-out. The noise is the median of the quiet within `NOISE_S`.
const RISE_DB: f32 = 8.0;
const BRIDGE_S: f64 = 1.5;
const NOISE_S: f64 = 8.0;
/// Lengths that need more scaling than this to fill a side get a warning.
const SCALE_WARN: f64 = 0.05;

/// How a boundary was placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    /// The start of a side: first music after the lead-in or the side change.
    SideStart,
    /// The quiet between two tracks.
    Gap,
    /// Where the track lengths put it; nothing clear to snap to.
    Lengths,
}

/// Where a track's start came from.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryNote {
    pub track: usize,
    pub snap: Snap,
    /// Placed minus expected position, seconds (0 for side starts).
    pub shift_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub sheet: TrackSheet,
    pub notes: Vec<BoundaryNote>,
    /// Factor each side's track lengths were scaled by to fill it.
    pub side_scales: Vec<f64>,
    pub warnings: Vec<String>,
}

/// Places the release's tracks on a rip of `total_frames` frames whose level is `env`.
pub fn place_tracks(release: &ReleaseInfo, env: &Envelope, total_frames: u64, source_file: &str) -> Placement {
    let mut sheet = TrackSheet::new(source_file, env.sample_rate, total_frames);
    sheet.album = release.album.clone();
    let mut notes = Vec::new();
    let mut side_scales = Vec::new();
    let mut warnings = Vec::new();
    let n_hops = env.len();
    if release.tracks.is_empty() || n_hops == 0 {
        return Placement { sheet, notes, side_scales, warnings };
    }

    let th = env.quiet_threshold();
    let loud = |h: usize| env.db[h] >= th;
    let gaps = env.quiet_runs(th, env.s_to_hops(GAP_MIN_S));
    let music_win = env.s_to_hops(MUSIC_MIN_S).max(1);
    // A side change is one quiet stretch even where a needle lift or drop interrupts it.
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for g in &gaps {
        match merged.last_mut() {
            Some(m) if g.start - m.end < music_win => m.end = g.end,
            _ => merged.push(g.clone()),
        }
    }
    let side_gaps: Vec<_> = merged.into_iter().filter(|r| r.len() >= env.s_to_hops(SIDE_GAP_MIN_S)).collect();
    let pre = env.s_to_hops(PRE_ROLL_S);
    let tail = env.s_to_hops(TAIL_S);
    let min_track = env.s_to_hops(crate::MIN_TRACK_S);
    let sustained = |r: std::ops::Range<usize>| {
        let n = r.len().max(1);
        r.filter(|&h| loud(h)).count() as f64 >= MUSIC_FRAC * n as f64
    };
    let music_starts = |h: usize| loud(h) && sustained(h..(h + music_win).min(n_hops));
    let music_ends = |h: usize| loud(h) && sustained(h.saturating_sub(music_win - 1)..h + 1);
    let last_loud = (0..n_hops).rev().find(|&h| music_ends(h)).map_or(n_hops, |h| h + 1);
    let sides = release.sides();

    // Track starts and ends in hops, filled side by side.
    let mut starts = Vec::with_capacity(release.tracks.len());
    let mut ends = Vec::with_capacity(release.tracks.len());
    let mut cursor = 0usize;
    for (k, side) in sides.iter().enumerate() {
        let side_tracks = &release.tracks[side.clone()];
        let name = side_tracks[0].side();
        let side_floor = ends.last().copied().unwrap_or(0);
        let onset = (cursor..n_hops).find(|&h| music_starts(h)).unwrap_or(cursor);
        let onset = extend_back(env, th, side_floor, onset);

        let known: Vec<f64> = side_tracks.iter().filter_map(|t| t.seconds).filter(|&s| s > 0.0).collect();
        let fill = if known.is_empty() { 1.0 } else { known.iter().sum::<f64>() / known.len() as f64 };
        let lens: Vec<f64> = side_tracks.iter().map(|t| t.seconds.filter(|&s| s > 0.0).unwrap_or(fill)).collect();
        let sum_s: f64 = lens.iter().sum();

        // Where this side's music ends, and where the next side may begin.
        let (music_end, side_limit) = if k + 1 == sides.len() {
            (extend_forward(env, th, last_loud.max(onset + 1), n_hops), n_hops)
        } else {
            let all_known = known.len() == side_tracks.len();
            let expected = onset + env.s_to_hops(sum_s);
            let reach = env.s_to_hops(SIDE_SEARCH_S.max(SIDE_SEARCH_FRAC * sum_s));
            let after = side_gaps.iter().filter(|r| r.start > onset && r.start < last_loud);
            let pick = if all_known {
                after.filter(|r| r.start.abs_diff(expected) <= reach).min_by_key(|r| r.start.abs_diff(expected))
            } else {
                after.max_by_key(|r| r.len())
            };
            match pick {
                Some(r) => (extend_forward(env, th, r.start, r.end), r.end),
                None => {
                    warnings.push(format!("Side {name}: no side change found; placed from the track lengths"));
                    let e = expected.min(n_hops);
                    (e, e)
                }
            }
        };

        let span = music_end.saturating_sub(onset).max(1);
        let scale = span as f64 / env.s_to_hops(sum_s).max(1) as f64;
        side_scales.push(scale);
        if (scale - 1.0).abs() > SCALE_WARN {
            warnings.push(format!(
                "Side {name}: track lengths add up to {} but the side's music runs {}; lengths scaled by {:.3}",
                mmss(sum_s),
                mmss(span as f64 / env.hops_per_s()),
                scale
            ));
        }

        starts.push(onset.saturating_sub(pre).max(side_floor));
        notes.push(BoundaryNote { track: side.start, snap: Snap::SideStart, shift_s: 0.0 });
        let reach = env.s_to_hops(SNAP_S);
        let mut cum = 0.0;
        let mut offset = 0i64;
        for j in 1..side_tracks.len() {
            cum += lens[j - 1];
            let expected = (onset as i64 + (cum * scale / crate::envelope::HOP_S).round() as i64 + offset).max(0) as usize;
            let lo = (*starts.last().unwrap() + min_track).max(expected.saturating_sub(reach));
            let hi = music_end.saturating_sub(min_track).min(expected + reach);
            let (b, snap) = if lo >= hi {
                let floor = *starts.last().unwrap() + 1;
                (expected.clamp(floor, music_end.max(floor)), Snap::Lengths)
            } else if let Some(g) = gaps.iter().filter(|g| g.end > lo && g.end <= hi).min_by_key(|g| g.end.abs_diff(expected)) {
                // The next track's first music, not before the middle of the gap.
                let first = extend_back(env, th, g.start + g.len() / 2, g.end);
                (first.saturating_sub(pre).max(g.start), Snap::Gap)
            } else {
                (expected.clamp(lo, hi), Snap::Lengths)
            };
            if snap == Snap::Gap {
                offset += b as i64 + pre as i64 - expected as i64;
            }
            notes.push(BoundaryNote {
                track: side.start + j,
                snap,
                shift_s: (b as f64 - expected as f64) / env.hops_per_s(),
            });
            ends.push(b);
            starts.push(b);
        }
        ends.push((music_end + tail).min(side_limit));
        cursor = side_limit;
    }

    let last = total_frames;
    sheet.tracks = release
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| Track {
            title: t.title.clone(),
            artist: t.artist.clone(),
            start: env.hop_to_frame(starts[i]).min(last),
            end: env.hop_to_frame(ends[i]).min(last),
            linked: i > 0 && starts[i] == ends[i - 1],
            source_position: t.position.clone(),
            source_seconds: t.seconds,
        })
        .collect();
    Placement { sheet, notes, side_scales, warnings }
}

fn mmss(s: f64) -> String {
    let s = s.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Median level of the quiet hops in `r`, when there are enough of them.
fn noise_level(env: &Envelope, th: f32, r: std::ops::Range<usize>) -> Option<f32> {
    let mut v: Vec<f32> = env.db[r].iter().copied().filter(|&d| d < th).collect();
    if v.len() < env.s_to_hops(0.2) {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[v.len() / 2])
}

/// Moves a music onset back over a count-in or quiet intro, not past `lo`.
fn extend_back(env: &Envelope, th: f32, lo: usize, onset: usize) -> usize {
    let Some(noise) = noise_level(env, th, onset.saturating_sub(env.s_to_hops(NOISE_S)).max(lo)..onset) else {
        return onset;
    };
    let bridge = env.s_to_hops(BRIDGE_S);
    let mut h = onset;
    while let Some(j) = (h.saturating_sub(bridge).max(lo)..h).find(|&j| env.db[j] > noise + RISE_DB) {
        h = j;
    }
    h
}

/// Moves a music end forward over a fade-out or decay, not past `hi`.
fn extend_forward(env: &Envelope, th: f32, end: usize, hi: usize) -> usize {
    let Some(noise) = noise_level(env, th, end..(end + env.s_to_hops(NOISE_S)).min(hi)) else {
        return end;
    };
    let bridge = env.s_to_hops(BRIDGE_S);
    let mut h = end;
    while let Some(j) = (h..(h + bridge).min(hi)).rev().find(|&j| env.db[j] > noise + RISE_DB) {
        h = j + 1;
    }
    h
}
