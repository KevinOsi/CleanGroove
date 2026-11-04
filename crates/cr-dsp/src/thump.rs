//! Rotational thump finder (draft).
//!
//! A needle knocked by debris on the record makes a "thump": a sharp burst followed by a
//! low-frequency swing, about 5-20 ms, far louder than the music. The declicker repairs only the
//! sharp onset (its detector looks for unpredictable audio, and the swing is very predictable),
//! and the Enhanced long-repair gate refuses the rest for the same reason. Loudness alone cannot
//! tell a thump from music (an isolated drum hit in a quiet passage stands 16 dB above its
//! surroundings on the clean reference album). What music does not do is repeat exactly once
//! per turn of the record, so this module looks for loud transients that form a chain at the
//! rotation period (1.80 s at 33 1/3, 1.33 s at 45, 0.77 s at 78), within a speed tolerance.
//!
//! A steady beat can share the period (a bar of 4 at 133 bpm lasts 1.80 s), so every series
//! also reports how often there is an event of the same kind half and a quarter of a turn away:
//! a beat has its other beats there, a scratch has nothing.
//!
//! The record analysis reports thump-like series (`AlbumAnalysis::thumps`). The repair
//! (`repair_thumps`) is not part of the Classic or Enhanced engines: it runs on request, after
//! declicking and before the post-declick corrections.

use crate::autotune::FULL_SCALE;

/// Analysis frame (ms) for the level measurements.
pub const FRAME_MS: f64 = 2.0;
/// The local reference level is the median frame level within this distance (ms) either side,
/// leaving out `REF_GAP_MS` around the frame itself.
pub const REF_SPAN_MS: f64 = 150.0;
pub const REF_GAP_MS: f64 = 10.0;
/// A frame is a loud transient when it stands this far (dB) above its local reference.
pub const EVENT_DB: f64 = 12.0;
/// Frames below this level (dBFS) are never events (quiet surface noise in silence).
pub const EVENT_FLOOR_DBFS: f64 = -50.0;
/// Events closer than this (ms) are one event.
pub const EVENT_MERGE_MS: f64 = 20.0;

/// Turntable speeds: name and seconds per turn.
pub const SPEEDS: [(&str, f64); 3] = [("33", 1.8), ("45", 60.0 / 45.0), ("78", 60.0 / 78.0)];
/// A record can play a little off speed: the period is searched within this fraction.
pub const SPEED_TOLERANCE: f64 = 0.01;
/// An event belongs to a chain when it lands within this (ms) of the predicted time.
pub const CHAIN_TOL_MS: f64 = 6.0;
/// A chain may skip this many turns in a row (the debris did not hit every time).
pub const CHAIN_MAX_MISSES: usize = 2;
/// Chains with fewer events are not reported.
pub const MIN_CHAIN_EVENTS: usize = 4;
/// One record plays at one speed: a series whose period is further than this fraction from the
/// record's period (that of its longest series) is marked off-period (a chance chain in dense
/// music, most likely). On 31 rips the real series sit at 1.7999-1.8000 s.
pub const RECORD_PERIOD_TOLERANCE: f64 = 0.002;
/// A series is thump-like (rather than a repeating tick) when its events average at least this
/// long (ms) and this far (dB) above the music.
pub const THUMP_MIN_MS: f64 = 5.0;
pub const THUMP_MIN_DB: f64 = 15.0;
/// ... and keeps time: a scratch on the record is regular to about 1 ms (2 ms frames); chance
/// chains in dense music scatter more (Bob Marley, Survival: two "45 rpm" chains on an LP at
/// 1.9 and 3.0 ms).
pub const THUMP_MAX_TIMING_MS: f64 = 1.5;
/// A series looks like a beat when more than this fraction of its half/quarter-turn slots
/// hold an event too.
pub const BEAT_FRACTION: f64 = 0.25;

/// Settings chosen 2026-10-29: event 12 dB, chains of 4 within 1% of a nominal speed. With
/// them the clean reference album (no record, so no rotation) gives 0 series, every music and
/// low-damage gold-set clip 0, and the Whitney "heavy damage glob" pops come out as one series
/// of 7 at 1.8002 s.

/// One loud transient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThumpEvent {
    /// First frame of the event, in samples from the start of the file.
    pub frame: usize,
    /// Channels in which it stands `EVENT_DB` above the reference (bit 0 left, bit 1 right).
    pub channels: u8,
    /// Loudest 2 ms frame (dBFS) and how far it stands above its local reference (dB).
    pub level_db: f64,
    pub above_db: f64,
    /// How long it stays at least 6 dB above the reference (ms).
    pub duration_ms: f64,
    /// How long until it has faded into the music (two frames in a row within
    /// `FADED_DB` of the reference), for the repair span (ms).
    pub extent_ms: f64,
}

/// Loud transients repeating at a rotation period.
#[derive(Debug, Clone, PartialEq)]
pub struct ThumpSeries {
    /// Nominal speed ("33", "45", "78") and the fitted period (s).
    pub speed: &'static str,
    pub period_s: f64,
    pub events: Vec<ThumpEvent>,
    /// Turns inside the chain's span with no event (the debris missed).
    pub missed_turns: usize,
    /// RMS distance (ms) of the events from a steady period: a scratch on a record is very
    /// regular; small wow and the detector's 2 ms frames account for a few ms.
    pub timing_rms_ms: f64,
    /// Music check: of the slots half and a quarter of a turn from each event (three per event),
    /// how many hold a loud transient too. A beat fills them, a scratch does not.
    pub sub_period_hits: usize,
    pub sub_period_slots: usize,
    /// Within `RECORD_PERIOD_TOLERANCE` of the record's period (set by `find_thumps`).
    pub on_record_period: bool,
}

impl ThumpSeries {
    /// The evidence for a needle thump: on the record's period, not beat-like, and long and
    /// loud on average.
    pub fn is_thump_like(&self) -> bool {
        let n = self.events.len().max(1) as f64;
        self.on_record_period
            && self.timing_rms_ms <= THUMP_MAX_TIMING_MS
            && self.beat_fraction() <= BEAT_FRACTION
            && self.events.iter().map(|e| e.duration_ms).sum::<f64>() / n >= THUMP_MIN_MS
            && self.events.iter().map(|e| e.above_db).sum::<f64>() / n >= THUMP_MIN_DB
    }

    /// Fraction of the half/quarter-turn slots that hold an event (0 = nothing but the chain).
    pub fn beat_fraction(&self) -> f64 {
        self.sub_period_hits as f64 / self.sub_period_slots.max(1) as f64
    }
}

fn frame_levels(frames: &[(i32, i32)], ch: usize, frame_len: usize) -> Vec<f64> {
    frames
        .chunks(frame_len)
        .map(|c| {
            let ms = c.iter().map(|f| (if ch == 0 { f.0 } else { f.1 } as f64).powi(2)).sum::<f64>() / c.len() as f64;
            10.0 * (ms.max(1.0) / (FULL_SCALE * FULL_SCALE)).log10()
        })
        .collect()
}

/// Median level of the frames in the reference window around every frame.
fn local_reference(levels: &[f64], span: usize, gap: usize) -> Vec<f64> {
    let mut window: Vec<f64> = Vec::with_capacity(2 * span);
    (0..levels.len())
        .map(|f| {
            window.clear();
            window.extend_from_slice(&levels[f.saturating_sub(span)..f.saturating_sub(gap)]);
            window.extend_from_slice(&levels[(f + gap + 1).min(levels.len())..(f + span + 1).min(levels.len())]);
            if window.is_empty() {
                return levels[f];
            }
            let mid = window.len() / 2;
            *window.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1
        })
        .collect()
}

/// Every loud transient in the audio (stereo frames in the internal 25-bit domain).
pub fn find_events(frames: &[(i32, i32)], sample_rate: u32) -> Vec<ThumpEvent> {
    let ms = |v: f64| ((v / 1000.0 * sample_rate as f64) as usize).max(1);
    let frame_len = ms(FRAME_MS);
    let span = (REF_SPAN_MS / FRAME_MS) as usize;
    let gap = (REF_GAP_MS / FRAME_MS) as usize;
    let levels = [frame_levels(frames, 0, frame_len), frame_levels(frames, 1, frame_len)];
    let refs = [local_reference(&levels[0], span, gap), local_reference(&levels[1], span, gap)];
    let n = levels[0].len();
    let above = |ch: usize, f: usize| levels[ch][f] - refs[ch][f];
    let loud = |ch: usize, f: usize| levels[ch][f] >= EVENT_FLOOR_DBFS && above(ch, f) >= EVENT_DB;

    let merge = (EVENT_MERGE_MS / FRAME_MS) as usize;
    let mut events: Vec<ThumpEvent> = Vec::new();
    let mut f = 0;
    while f < n {
        if !(loud(0, f) || loud(1, f)) {
            f += 1;
            continue;
        }
        // One event: this frame and every loud frame within `merge` frames of the last one.
        let start = f;
        let mut last = f;
        let (mut channels, mut level_db, mut above_db) = (0u8, f64::MIN, f64::MIN);
        while f < n && f <= last + merge {
            for ch in 0..2 {
                if loud(ch, f) {
                    last = f;
                    channels |= 1 << ch;
                    if above(ch, f) > above_db {
                        above_db = above(ch, f);
                        level_db = levels[ch][f];
                    }
                }
            }
            f += 1;
        }
        let mut end = start;
        while end < n && (0..2).any(|ch| above(ch, end) >= 6.0) {
            end += 1;
        }
        let mut faded = end;
        let quiet = |f: usize| (0..2).all(|ch| above(ch, f) <= FADED_DB);
        while faded + 1 < n && !(quiet(faded) && quiet(faded + 1)) && faded < start + (MAX_EXTENT_MS / FRAME_MS) as usize {
            faded += 1;
        }
        events.push(ThumpEvent {
            frame: start * frame_len,
            channels,
            level_db,
            above_db,
            duration_ms: (end - start) as f64 * FRAME_MS,
            extent_ms: (faded - start) as f64 * FRAME_MS,
        });
    }
    events
}

/// Chains of events at a rotation period, best (most events) first; each event is used once.
pub fn find_series(events: &[ThumpEvent], sample_rate: u32) -> Vec<ThumpSeries> {
    let t: Vec<f64> = events.iter().map(|e| e.frame as f64 / sample_rate as f64).collect();
    let tol = CHAIN_TOL_MS / 1000.0;
    // Index of the event nearest to `x`, if within `tol`.
    let near = |x: f64| -> Option<usize> {
        let i = t.partition_point(|&v| v < x);
        [i.checked_sub(1), Some(i)]
            .into_iter()
            .flatten()
            .filter(|&k| k < t.len() && (t[k] - x).abs() <= tol)
            .min_by(|&a, &b| (t[a] - x).abs().total_cmp(&(t[b] - x).abs()))
    };

    let mut candidates: Vec<(Vec<usize>, &'static str, f64, usize)> = Vec::new();
    for &(speed, nominal) in &SPEEDS {
        let (lo, hi) = (nominal * (1.0 - SPEED_TOLERANCE), nominal * (1.0 + SPEED_TOLERANCE));
        for i in 0..t.len() {
            let first = t.partition_point(|&v| v < t[i] + lo);
            for j in first..t.len() {
                if t[j] > t[i] + hi {
                    break;
                }
                // Grow the chain from the pair, refitting the period as it grows.
                let mut chain = vec![i, j];
                let mut period = t[j] - t[i];
                let (mut misses, mut missed_total, mut k) = (0, 0, 2.0);
                loop {
                    match near(t[i] + k * period) {
                        Some(m) if !chain.contains(&m) => {
                            chain.push(m);
                            misses = 0;
                            period = (t[m] - t[i]) / k;
                        }
                        _ => {
                            misses += 1;
                            missed_total += 1;
                            if misses > CHAIN_MAX_MISSES {
                                missed_total -= misses;
                                break;
                            }
                        }
                    }
                    k += 1.0;
                }
                if chain.len() >= MIN_CHAIN_EVENTS {
                    candidates.push((chain, speed, period, missed_total));
                }
            }
        }
    }

    // Best chains first; drop chains that reuse an event already taken.
    candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.3.cmp(&b.3)));
    let mut used = vec![false; t.len()];
    let mut series = Vec::new();
    for (chain, speed, period, missed_turns) in candidates {
        if chain.iter().any(|&k| used[k]) {
            continue;
        }
        for &k in &chain {
            used[k] = true;
        }
        let t0 = t[chain[0]];
        let timing_rms_ms = 1000.0
            * (chain
                .iter()
                .map(|&k| {
                    let turns = ((t[k] - t0) / period).round();
                    (t[k] - t0 - turns * period).powi(2)
                })
                .sum::<f64>()
                / chain.len() as f64)
                .sqrt();
        let mut sub_period_hits = 0;
        for &k in &chain {
            for frac in [0.25, 0.5, 0.75] {
                if near(t[k] + frac * period).is_some_and(|m| !chain.contains(&m)) {
                    sub_period_hits += 1;
                }
            }
        }
        let mut sorted = chain.clone();
        sorted.sort_unstable();
        series.push(ThumpSeries {
            speed,
            period_s: period,
            events: sorted.iter().map(|&k| events[k]).collect(),
            missed_turns,
            timing_rms_ms,
            sub_period_hits,
            sub_period_slots: 3 * chain.len(),
            on_record_period: true,
        });
    }
    series
}

/// Events and rotational series in one call; series are marked against the record's period.
pub fn find_thumps(frames: &[(i32, i32)], sample_rate: u32) -> (Vec<ThumpEvent>, Vec<ThumpSeries>) {
    let events = find_events(frames, sample_rate);
    let mut series = find_series(&events, sample_rate);
    // The longest series sets the record's period (find_series sorts longest first).
    if let Some(record) = series.first().map(|s| s.period_s) {
        for s in &mut series {
            s.on_record_period = (s.period_s - record).abs() <= RECORD_PERIOD_TOLERANCE * record;
        }
    }
    (events, series)
}

/// Repair margins (ms) around a thump: before its first loud frame (the onset can be anywhere
/// in that 2 ms frame) and after it has faded into the music (`ThumpEvent::extent_ms`). The
/// swing decays well below the 6 dB of `duration_ms` before it is gone: ending the repair there
/// left a third of it on a synthetic thump.
pub const REPAIR_LEAD_MS: f64 = 2.0;
pub const REPAIR_TAIL_MS: f64 = 5.0;
/// A thump has faded at this many dB above the music; the extent search stops at
/// `MAX_EXTENT_MS`.
pub const FADED_DB: f64 = 3.0;
pub const MAX_EXTENT_MS: f64 = 20.0;
/// Longest repair (samples): the AR repair routine's limit (46 ms at 44.1 kHz).
pub const MAX_REPAIR: usize = 2048;

/// The span [start, end) the repair replaces for one event, in samples.
pub fn repair_span(e: &ThumpEvent, sample_rate: u32) -> (usize, usize) {
    let ms = |v: f64| (v / 1000.0 * sample_rate as f64).round() as usize;
    let start = e.frame.saturating_sub(ms(REPAIR_LEAD_MS));
    let end = e.frame + ms(e.extent_ms + REPAIR_TAIL_MS);
    (start, end.min(start + MAX_REPAIR))
}

/// Long AR repair of thumps (run on request, after declicking): every event of every
/// thump-like series is replaced, in both channels (the needle is thrown, so both groove walls
/// carry it), by the declicker's own AR interpolation (`fill_gap`) over its
/// whole span. The evidence that it is damage is the series, not the audio inside the span.
/// Returns the spans repaired.
pub fn repair_thumps(audio: &mut [(i32, i32)], sample_rate: u32, series: &[ThumpSeries]) -> Vec<(usize, usize)> {
    repair_spans(audio, 0, &thump_spans(series, sample_rate))
}

/// Audio the AR models need on each side of a span's centre: order up to 512, window
/// 2 * 512 + 256, in a ring of 8192.
const RING: usize = 8192;
/// Context (frames) a block needs on each side of the audio it keeps for every thump touching
/// that audio to be repaired exactly as in a whole-file pass (`repair_spans`).
pub const REPAIR_CONTEXT: usize = RING / 2 + MAX_REPAIR;

/// The spans `repair_thumps` replaces: every event of every thump-like series, sorted.
pub fn thump_spans(series: &[ThumpSeries], sample_rate: u32) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = series
        .iter()
        .filter(|s| s.is_thump_like())
        .flat_map(|s| s.events.iter().map(|e| repair_span(e, sample_rate)))
        .collect();
    spans.sort_unstable();
    spans.dedup();
    spans
}

/// Repairs the spans (file frame indices) that lie, with their AR context, inside `audio`,
/// which holds the file's frames from `offset` on; the rest are skipped. The same span repairs
/// the same way whatever block it is in. Returns the spans repaired.
pub fn repair_spans(audio: &mut [(i32, i32)], offset: usize, spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    use crate::reconstruct::{fill_gap, ScratchSpace};
    let inside: Vec<(usize, usize)> = spans
        .iter()
        .copied()
        .filter(|&(s, e)| s >= offset + RING / 2 && e + RING / 2 <= offset + audio.len())
        .collect();
    let mut ring = vec![0i32; RING];
    let mut fixed = vec![0i32; MAX_REPAIR];
    let mut scratch = ScratchSpace::new();
    for &(start, end) in &inside {
        let base = start + (end - start) / 2 - RING / 2 - offset;
        let info = [(start - offset - base) as isize, (end - offset - base) as isize];
        for ch in 0..2 {
            for (k, slot) in ring.iter_mut().enumerate() {
                let f = audio[base + k];
                *slot = if ch == 0 { f.0 } else { f.1 };
            }
            fill_gap(&ring, RING - 1, info, &mut fixed, &mut scratch);
            for (k, v) in fixed[..end - start].iter().enumerate() {
                let f = &mut audio[start - offset + k];
                if ch == 0 {
                    f.0 = *v;
                } else {
                    f.1 = *v;
                }
            }
        }
    }
    inside
}
