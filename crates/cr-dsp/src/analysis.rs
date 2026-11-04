//! Album analysis: one pass over the whole record that characterises it, independent of the
//! user's current settings.
//!
//! The record is processed once with the offline engine at the top of the automatic range
//! (dcl 50) and every repair is logged with its peak level. From that log and per-second
//! loudness the analysis derives the click grade (from all the silence between tracks), a
//! per-second click map, the worst problem spots, recording levels and channel geometry.
//! The auto-tuner reuses it (`autotune_audio_with_analysis`), and later record checks
//! (subsonic content, cartridge alignment, gouges) belong here too.
//!
//! The grading rules are the auto-tuner's: an audible click
//! is a repair whose peak is above -40 dBFS, a loud pop above -30 dBFS, counted on both
//! channels. The audible count is flat across dcl above about 20, so grading at dcl 50 gives
//! the same grade as the auto-tuner's dcl 30.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::autotune::{
    analyze_groove_geometry, rms_db, spread, RecordCondition, Region, AUDIBLE_DB, FULL_SCALE, GAP_MAX_DB, GAP_MIN_DB,
    MAX_DCL, MIN_GAP_BLOCKS, MUSIC_MIN_DB, POP_DB,
};
use crate::params::{ChannelMode, CrParameters, Method};
use crate::post::measure::{measure_channels, ChannelMeasurements};
use crate::post::PostSettings;
use crate::process::Processor;

/// dcl the analysis runs at: the top of the automatic range, so the map holds every click the
/// auto-tuner could ask to repair.
pub const ANALYSIS_DCL: i32 = MAX_DCL;
/// Number of problem spots kept.
const PROBLEM_SPOTS: usize = 12;
/// A repair is an exposed click when its peak is at least `EXPOSED_DB` above the RMS level of
/// the second it is in, AND at least `EXPOSED_LOCAL_DB` above the original audio in the
/// `EXPOSED_LOCAL_MS` either side of it (the repaired samples left out).
///
/// The one-second test alone: every repair in the gaps between tracks (all real clicks) sits
/// 5 dB or more above its second, and none of the music repairs on a saxophone record at dcl 50 reaches 15 dB.
/// But on a clean digital reference (loud modern masters, no real clicks) 85 drum hits the
/// engine reshaped passed it. The local test separates them: a drum hit keeps the audio around
/// it loud, a click sits in quiet. Of those 85 reference events 1 stands 20 dB above its
/// surroundings, against about 55-65% of the real clicks between tracks on four rips (median
/// 20-22 dB). Below these a repair is music, or a click the music masks.
pub const EXPOSED_DB: f32 = 15.0;
pub const EXPOSED_LOCAL_DB: f32 = 20.0;
pub const EXPOSED_LOCAL_MS: f64 = 10.0;
/// Repairs longer than this are flagged: more likely a scratch or gouge than a click.
const LONG_REPAIR_MS: f64 = 2.0;
/// Post-declick suggestion thresholds.
/// Inverted channel: high-passed L/R correlation below this.
const POLARITY_CORR: f64 = -0.3;
/// Balance correction (mono records only): at least this far off, with windows agreeing.
const BALANCE_MIN_DB: f64 = 0.3;
const BALANCE_MAX_SPREAD_DB: f64 = 1.0;
/// Azimuth correction: at least this large (below it is under the threshold of hearing), the
/// median known to within this standard error, and both halves of the record agreeing (as the
/// Python tooling required of its two windows). Single windows scatter by several us from
/// surface noise and the mix, so precision comes from the number of windows.
const AZIMUTH_MIN_US: f64 = 2.0;
const AZIMUTH_MAX_SE_US: f64 = 1.0;
const AZIMUTH_HALVES_US: f64 = 3.0;
const AZIMUTH_MIN_WINDOWS: usize = 16;
/// Subsonic high-pass cutoff, and the elliptical corners: the default, and the higher one used
/// when the Side bass in the music is mostly rumble.
pub const SUBSONIC_HZ: f64 = 18.0;
pub const ELLIPTICAL_HZ: f64 = 100.0;
pub const ELLIPTICAL_HIGH_HZ: f64 = 150.0;
/// "Mostly rumble": the Side bass between tracks within this many dB of the Side bass in music.
const RUMBLE_DOMINANT_DB: f64 = 6.0;

/// Spots within this many seconds of silence are tagged `near_silence`.
const EDGE_BLOCKS: usize = 3;
/// ...and so are spots this close to the start or end of the file, where lead-in and run-out
/// grooves sit. On one rip those grooves measure -28 to -43 dBFS (rumble), too loud to look like
/// silence; a level measured above the rumble band would find them, but is not done yet.
const FILE_EDGE_BLOCKS: usize = 15;
/// Samples at or above this fraction of full scale count as clipped.
pub const CLIP_FRACTION: f64 = 0.999;
/// Differences at or below this are wavelet round-off, not repairs.
const ROUNDOFF: i32 = 2;

/// One repair the engine made at `ANALYSIS_DCL`.
#[derive(Debug, Clone, Copy)]
pub struct RepairEvent {
    pub frame: usize,
    /// 0 = left, 1 = right.
    pub channel: u8,
    pub len: u32,
    /// Peak of the removed signal, dBFS.
    pub peak_db: f32,
    /// Exposed click: stands out from both its second and its immediate surroundings (see
    /// `EXPOSED_DB`). Set by the analysis.
    pub exposed: bool,
}

impl RepairEvent {
    pub fn is_audible(&self) -> bool {
        self.peak_db as f64 > AUDIBLE_DB
    }
    pub fn is_pop(&self) -> bool {
        self.peak_db as f64 > POP_DB
    }
}

/// A stretch of music worth auditioning: the most exposed pops, then the most exposed clicks.
#[derive(Debug, Clone)]
pub struct ProblemSpot {
    pub start_frame: usize,
    pub len_frames: usize,
    /// Exposed clicks (see `EXPOSED_DB`), and how many of them are loud pops.
    pub clicks: u32,
    pub pops: u32,
    /// Longest single exposed repair in the spot.
    pub longest_repair_ms: f32,
    /// Loudness of the music there, dBFS RMS (clicks in quiet passages are the most exposed).
    pub level_db: f32,
    /// Within 3 s of silence or 15 s of the file edges: possibly a needle drop, side change or
    /// run-out groove rather than damage inside the music.
    pub near_silence: bool,
}

impl ProblemSpot {
    pub fn has_long_repair(&self) -> bool {
        self.longest_repair_ms as f64 > LONG_REPAIR_MS
    }
}

/// Result of `analyze_album`.
#[derive(Debug, Clone)]
pub struct AlbumAnalysis {
    pub sample_rate: u32,
    pub total_frames: usize,
    /// Channel mode the record was analysed in (Merge for a mono record captured in stereo).
    pub mode: ChannelMode,
    pub lr_midband_correlation: f64,
    pub side_to_mid_ratio: f64,

    /// Every repair at `ANALYSIS_DCL`, sorted by frame (music pickup included; see `EXPOSED_DB`).
    pub events: Vec<RepairEvent>,
    /// Per one-second block: RMS level (dBFS); audible clicks and loud pops (used for the
    /// grade, counted in the gaps); exposed clicks (see `EXPOSED_DB`, used for the map).
    pub block_db: Vec<f32>,
    pub block_audible: Vec<u16>,
    pub block_pops: Vec<u16>,
    pub block_exposed: Vec<u16>,
    /// Blocks of groove noise between tracks (silence that is not digital silence).
    pub gap_blocks: Vec<usize>,

    pub condition: RecordCondition,
    /// True when there was too little silence between tracks and the grade comes from the
    /// quietest music instead (it will under-read, since music masks small clicks).
    pub condition_estimated: bool,
    pub audible_clicks_per_sec: f64,
    pub loud_pops_per_sec: f64,
    /// Seconds the grade was measured over.
    pub grade_seconds: f64,

    /// Fraction of samples the engine changed over the whole file at `ANALYSIS_DCL`.
    pub repair_fraction: f64,
    /// Exposed repairs longer than 2 ms (possible scratches or gouges).
    pub long_repairs: usize,

    pub peak_dbfs: f32,
    /// Power-average level of the music blocks, dBFS RMS.
    pub music_rms_db: f32,
    pub clipped_samples: usize,

    pub problem_spots: Vec<ProblemSpot>,

    /// Balance, azimuth and rumble measurements for the post-declick corrections.
    pub channels: ChannelMeasurements,

    /// Needle thumps: thump-like rotational series (`crate::thump`). The repair is offered,
    /// not applied: the user listens and switches it on.
    pub thumps: Vec<crate::thump::ThumpSeries>,
}

impl AlbumAnalysis {
    /// True for a mono record captured in stereo (or identical channels).
    pub fn is_mono_record(&self) -> bool {
        self.mode != ChannelMode::Stereo
    }

    /// Post-declick corrections the measurements support. Each is suggested only when the
    /// measurement is consistent; see the thresholds above.
    pub fn suggested_post(&self) -> PostSettings {
        let c = &self.channels;
        let mono = self.is_mono_record();
        let mut s = PostSettings {
            flip_right_polarity: self.lr_midband_correlation < POLARITY_CORR,
            subsonic_hz: Some(SUBSONIC_HZ),
            mono_sum: mono,
            ..Default::default()
        };
        // On a stereo record a level difference is usually the mix, not the cartridge, so
        // balance is only corrected for mono records, where both channels should match.
        if mono && c.balance_db.abs() >= BALANCE_MIN_DB && c.balance_spread_db <= BALANCE_MAX_SPREAD_DB {
            s.balance_db = c.balance_db;
        }
        let (h1, h2) = c.azimuth_halves_us;
        if c.azimuth_windows >= AZIMUTH_MIN_WINDOWS
            && c.azimuth_us.abs() >= AZIMUTH_MIN_US
            && c.azimuth_se_us <= AZIMUTH_MAX_SE_US
            && (h1 - h2).abs() <= AZIMUTH_HALVES_US
        {
            s.azimuth_us = c.azimuth_us;
        }
        if !mono {
            let rumble_dominant = c.side_rumble_gap_db.is_finite() && c.side_rumble_gap_db > c.side_lf_music_db - RUMBLE_DOMINANT_DB;
            s.elliptical_hz = Some(if rumble_dominant { ELLIPTICAL_HIGH_HZ } else { ELLIPTICAL_HZ });
        }
        s
    }

    /// Frames per analysis block (one second).
    pub fn block_len(&self) -> usize {
        self.sample_rate as usize
    }

    /// True when the repair stands at least `EXPOSED_DB` above the level of its second.
    pub fn is_exposed(&self, e: &RepairEvent) -> bool {
        e.exposed
    }

    /// Repairs starting in `start..end`.
    pub fn events_in(&self, start: usize, end: usize) -> &[RepairEvent] {
        let s = self.events.partition_point(|e| e.frame < start);
        let e = self.events.partition_point(|e| e.frame < end);
        &self.events[s..e]
    }

    /// Gap blocks as processing regions (for the auto-tuner's A/B runs).
    pub(crate) fn gap_regions(&self, pad: usize, max: usize) -> Vec<Region> {
        let sr = self.block_len();
        let regions: Vec<Region> = self
            .gap_blocks
            .iter()
            .filter(|&&b| b * sr >= pad)
            .map(|&b| Region { start: b * sr, len: sr, pad })
            .collect();
        spread(&regions, max)
    }
}

/// Streams one channel's difference signal into repair events, bridging single unchanged
/// samples (zero crossings inside one repair).
struct EventRuns {
    channel: u8,
    open: Option<(usize, usize, i32)>, // (start, last changed frame, peak |diff|)
}

impl EventRuns {
    fn push(&mut self, frame: usize, diff: i32, out: &mut Vec<RepairEvent>) {
        let d = diff.abs();
        if d <= ROUNDOFF {
            if let Some((_, last, _)) = self.open {
                if frame > last + 1 {
                    self.close(out);
                }
            }
            return;
        }
        self.open = Some(match self.open {
            Some((start, last, peak)) if frame <= last + 2 => (start, frame, peak.max(d)),
            Some(_) => {
                self.close(out);
                (frame, frame, d)
            }
            None => (frame, frame, d),
        });
    }

    fn close(&mut self, out: &mut Vec<RepairEvent>) {
        if let Some((start, last, peak)) = self.open.take() {
            out.push(RepairEvent {
                frame: start,
                channel: self.channel,
                len: (last + 1 - start) as u32,
                peak_db: (20.0 * (peak.max(1) as f64 / FULL_SCALE).log10()) as f32,
                exposed: false,
            });
        }
    }
}

/// Analyses a whole record. `progress` gets (fraction, message); returns None if `cancel` is set.
pub fn analyze_album(
    frames: &[(i32, i32)],
    sample_rate: u32,
    channels: u16,
    progress: impl Fn(f32, &str),
    cancel: &AtomicBool,
) -> Option<AlbumAnalysis> {
    let sr = sample_rate.max(1) as usize;
    let total = frames.len();
    let n_blocks = total.div_ceil(sr).max(1);

    // Per-second loudness, peak and clipping from the input.
    progress(0.0, "Measuring levels...");
    let clip_level = (CLIP_FRACTION * FULL_SCALE) as i32;
    let mut peak = 0i32;
    let mut clipped_samples = 0usize;
    let mut block_db = Vec::with_capacity(n_blocks);
    for b in 0..n_blocks {
        let block = &frames[(b * sr).min(total)..((b + 1) * sr).min(total)];
        for &(l, r) in block {
            for v in [l, r] {
                let a = v.saturating_abs();
                peak = peak.max(a);
                clipped_samples += (a >= clip_level) as usize;
            }
        }
        block_db.push(if block.is_empty() { -150.0 } else { rms_db(block) as f32 });
    }

    // Groove noise between tracks; skip the first 2 s (needle drop) and partial blocks.
    let full_block = |b: usize| (b + 1) * sr <= total;
    let is_gap: Vec<bool> = (0..n_blocks)
        .map(|b| b >= 2 && full_block(b) && (block_db[b] as f64) > GAP_MIN_DB && (block_db[b] as f64) < GAP_MAX_DB)
        .collect();
    let is_music: Vec<bool> = (0..n_blocks)
        .map(|b| full_block(b) && !is_gap[b] && block_db[b] as f64 >= MUSIC_MIN_DB)
        .collect();
    let gap_blocks: Vec<usize> = (0..n_blocks).filter(|&b| is_gap[b]).collect();

    // Channel geometry on up to 64 music blocks.
    let music_regions: Vec<Region> = (0..n_blocks)
        .filter(|&b| is_music[b])
        .map(|b| Region { start: b * sr, len: sr, pad: 0 })
        .collect();
    let geometry_regions = if music_regions.is_empty() {
        vec![Region { start: 0, len: total, pad: 0 }]
    } else {
        spread(&music_regions, 64)
    };
    let (mode, lr_corr, side_to_mid) = analyze_groove_geometry(frames, sample_rate, channels, &geometry_regions);

    // Balance, azimuth and rumble, measured on the raw capture (medians shrug off clicks).
    progress(0.01, "Measuring channel balance, azimuth and rumble...");
    // Up to 256 music seconds: azimuth precision comes from the number of windows (single
    // windows scatter by several us), and GCC-PHAT is cheap.
    let music_windows: Vec<(usize, usize)> = spread(&music_regions, 256).iter().map(|r| (r.start, r.len)).collect();
    let gap_windows: Vec<(usize, usize)> = spread(&gap_blocks, 24).iter().map(|&b| (b * sr, sr)).collect();
    let channel_measurements = measure_channels(frames, sample_rate, &music_windows, &gap_windows);

    // One pass through the engine, logging every repair.
    let mut params = CrParameters::preset_stereo_lp();
    params.dcl = ANALYSIS_DCL;
    params.dcr = 0;
    params.method = Method::Wavelet;
    params.detect_pitch = false;
    params.reverse = false;
    params.mode = mode;
    let mut processor = Processor::new(params, sample_rate, Some(total));
    let mut runs = [EventRuns { channel: 0, open: None }, EventRuns { channel: 1, open: None }];
    let mut events = Vec::new();
    let mut changed = 0usize;
    let mut out_idx = 0usize;
    let mut out = Vec::with_capacity(65536);
    let mut record = |out: &[(i32, i32)], out_idx: &mut usize, events: &mut Vec<RepairEvent>| {
        for &(l, r) in out {
            if *out_idx < total {
                let (ol, or) = frames[*out_idx];
                let (dl, dr) = (ol - l, or - r);
                changed += (dl.abs() > ROUNDOFF) as usize + (dr.abs() > ROUNDOFF) as usize;
                runs[0].push(*out_idx, dl, events);
                runs[1].push(*out_idx, dr, events);
            }
            *out_idx += 1;
        }
    };
    for chunk in frames.chunks(65536) {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        processor.process_chunk(chunk, &mut out);
        record(&out, &mut out_idx, &mut events);
        progress(0.02 + 0.96 * out_idx.min(total) as f32 / total.max(1) as f32, "Analyzing the record...");
    }
    let flushed = processor.flush();
    record(&flushed, &mut out_idx, &mut events);
    for run in &mut runs {
        run.close(&mut events);
    }
    events.sort_by_key(|e| e.frame);

    // Exposure: above its second, and above the original audio just either side of it.
    let local = ((EXPOSED_LOCAL_MS / 1000.0 * sr as f64) as usize).max(1);
    for e in events.iter_mut() {
        let b = (e.frame / sr).min(n_blocks - 1);
        if !e.is_audible() || e.peak_db - block_db[b] < EXPOSED_DB {
            continue;
        }
        let (start, end) = (e.frame, (e.frame + e.len as usize).min(total));
        let (mut sum, mut n) = (0.0f64, 0usize);
        for i in (start.saturating_sub(local)..start).chain(end..(end + local).min(total)) {
            let v = if e.channel == 0 { frames[i].0 } else { frames[i].1 } as f64;
            sum += v * v;
            n += 1;
        }
        let around_db = 20.0 * ((sum / n.max(1) as f64).sqrt().max(1.0) / FULL_SCALE).log10();
        e.exposed = e.peak_db as f64 - around_db >= EXPOSED_LOCAL_DB as f64;
    }

    // Per-block click counts.
    let mut block_audible = vec![0u16; n_blocks];
    let mut block_pops = vec![0u16; n_blocks];
    let mut block_exposed = vec![0u16; n_blocks];
    let mut block_exposed_pops = vec![0u16; n_blocks];
    let mut block_longest = vec![0u32; n_blocks];
    let long_len = (LONG_REPAIR_MS / 1000.0 * sr as f64) as u32;
    let mut long_repairs = 0usize;
    for e in &events {
        let b = (e.frame / sr).min(n_blocks - 1);
        if e.is_audible() {
            block_audible[b] = block_audible[b].saturating_add(1);
        }
        if e.is_pop() {
            block_pops[b] = block_pops[b].saturating_add(1);
        }
        if e.exposed {
            block_exposed[b] = block_exposed[b].saturating_add(1);
            if e.is_pop() {
                block_exposed_pops[b] = block_exposed_pops[b].saturating_add(1);
            }
            block_longest[b] = block_longest[b].max(e.len);
            long_repairs += (e.len > long_len) as usize;
        }
    }

    // Grade from all the silence between tracks, or the quietest quarter of the music.
    let condition_estimated = gap_blocks.len() < MIN_GAP_BLOCKS;
    let grade_blocks: Vec<usize> = if condition_estimated {
        let mut music: Vec<usize> = (0..n_blocks).filter(|&b| is_music[b]).collect();
        music.sort_by(|&a, &b| block_db[a].partial_cmp(&block_db[b]).unwrap());
        let n = (music.len() / 4).max(1).min(music.len());
        music.truncate(n);
        if music.is_empty() {
            (0..n_blocks).filter(|&b| full_block(b)).collect()
        } else {
            music
        }
    } else {
        gap_blocks.clone()
    };
    let grade_seconds = grade_blocks.len() as f64;
    let sum = |v: &[u16]| grade_blocks.iter().map(|&b| v[b] as f64).sum::<f64>();
    let audible_clicks_per_sec = sum(&block_audible) / grade_seconds.max(1.0);
    let loud_pops_per_sec = sum(&block_pops) / grade_seconds.max(1.0);

    // Problem spots: music blocks with the most exposed pops, then exposed clicks, a long
    // exposed repair weighing like a few pops. Adjacent picks merge into one spot.
    let mut ranked: Vec<usize> = (0..n_blocks)
        .filter(|&b| is_music[b] && block_exposed[b] > 0)
        .collect();
    let score = |b: usize| {
        4 * block_exposed_pops[b] as u32 + block_exposed[b] as u32 + if block_longest[b] > long_len { 12 } else { 0 }
    };
    // Needle drops, side changes and run-out grooves sit next to silence or the file edges.
    // They are real disturbances but not damage inside the music, so they rank after it.
    let near_silence = |b: usize| {
        let lo = b.saturating_sub(EDGE_BLOCKS);
        let hi = (b + EDGE_BLOCKS).min(n_blocks - 1);
        b < FILE_EDGE_BLOCKS || b + FILE_EDGE_BLOCKS >= n_blocks || (lo..=hi).any(|k| (block_db[k] as f64) < GAP_MAX_DB)
    };
    ranked.sort_by_key(|&b| (near_silence(b), std::cmp::Reverse(score(b))));
    ranked.truncate(PROBLEM_SPOTS);
    ranked.sort_unstable();
    let mut problem_spots: Vec<ProblemSpot> = Vec::new();
    for b in ranked {
        let spot = ProblemSpot {
            start_frame: b * sr,
            len_frames: sr,
            clicks: block_exposed[b] as u32,
            pops: block_exposed_pops[b] as u32,
            longest_repair_ms: (block_longest[b] as f64 * 1000.0 / sr as f64) as f32,
            level_db: block_db[b],
            near_silence: near_silence(b),
        };
        match problem_spots.last_mut() {
            Some(prev) if prev.start_frame + prev.len_frames == spot.start_frame => {
                prev.len_frames += sr;
                prev.clicks += spot.clicks;
                prev.pops += spot.pops;
                prev.longest_repair_ms = prev.longest_repair_ms.max(spot.longest_repair_ms);
                prev.level_db = prev.level_db.min(spot.level_db);
                prev.near_silence |= spot.near_silence;
            }
            _ => problem_spots.push(spot),
        }
    }

    let music_power: Vec<f64> = (0..n_blocks)
        .filter(|&b| is_music[b])
        .map(|b| 10f64.powf(block_db[b] as f64 / 10.0))
        .collect();
    let music_rms_db = if music_power.is_empty() {
        -150.0
    } else {
        (10.0 * (music_power.iter().sum::<f64>() / music_power.len() as f64).log10()) as f32
    };

    progress(0.98, "Looking for needle thumps...");
    let (_, series) = crate::thump::find_thumps(frames, sample_rate);
    let thumps: Vec<crate::thump::ThumpSeries> = series.into_iter().filter(|s| s.is_thump_like()).collect();

    progress(1.0, "Analysis complete.");
    Some(AlbumAnalysis {
        sample_rate,
        total_frames: total,
        mode,
        lr_midband_correlation: lr_corr,
        side_to_mid_ratio: side_to_mid,
        events,
        block_db,
        block_audible,
        block_pops,
        block_exposed,
        gap_blocks,
        condition: RecordCondition::from_audible_rate(audible_clicks_per_sec),
        condition_estimated,
        audible_clicks_per_sec,
        loud_pops_per_sec,
        grade_seconds,
        repair_fraction: changed as f64 / (2 * total).max(1) as f64,
        long_repairs,
        peak_dbfs: (20.0 * (peak.max(1) as f64 / FULL_SCALE).log10()) as f32,
        music_rms_db,
        clipped_samples,
        problem_spots,
        channels: channel_measurements,
        thumps,
    })
}
