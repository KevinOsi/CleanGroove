//! Auto-tuner: grade the record's click condition, then suggest starting settings.
//!
//! The constants come from measurements on real records. In short:
//!
//! 1. The silent gaps between tracks carry no music, so the rate of audible clicks found
//!    there (peak above -40 dBFS) grades the record. That rate is flat across dcl once
//!    dcl is above about 15, so it measures the record, not the setting.
//! 2. The grade maps to a starting dcl on a log scale (near mint about 12, G+ about 45),
//!    capped at 50.
//! 3. The estimated repair rate is checked against 1 changed sample in 200 (the ClickRepair
//!    manual's suspicion line for vinyl) and a warning is given past it.
//! 4. Reverse is recommended unless an A/B run shows it doing harm: more repairs in the
//!    music, or real clicks lost in the gaps. On the detection benchmark it found the same
//!    clicks as forward scanning with a third to a half of the false repairs.
//!    Pitch Protection is switched on only when the A/B run shows it cuts repairs in the
//!    music without losing the real clicks in the gaps.
//!
//! The method is always Wavelet and DeCrackle is left off; see the design doc for why.

use crate::analysis::AlbumAnalysis;
use crate::params::{ChannelMode, CrParameters, Method};
use crate::process::Processor;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Internal full scale (24-bit PCM << 1).
pub(crate) const FULL_SCALE: f64 = 16_777_216.0;
/// Peak level above which a repaired burst counts as an audible click.
pub(crate) const AUDIBLE_DB: f64 = -40.0;
/// Peak level above which a repaired burst counts as a loud pop.
pub(crate) const POP_DB: f64 = -30.0;
/// Gap blocks: groove noise between tracks, louder than digital silence.
pub(crate) const GAP_MIN_DB: f64 = -75.0;
pub(crate) const GAP_MAX_DB: f64 = -45.0;
/// Music windows must be at least this loud.
pub(crate) const MUSIC_MIN_DB: f64 = -45.0;
const MAX_GAP_BLOCKS: usize = 24;
pub(crate) const MIN_GAP_BLOCKS: usize = 4;
/// One-second windows sampled evenly over the file.
const SAMPLE_WINDOWS: usize = 64;
/// dcl used to count clicks in the gaps (any value above ~20 gives the same audible count).
const GRADE_DCL: i32 = 30;
/// dcl grid swept over the music windows.
const SWEEP: [i32; 9] = [10, 15, 20, 25, 30, 35, 40, 45, 50];
const MIN_DCL: i32 = 10;
pub(crate) const MAX_DCL: i32 = 50;
/// ClickRepair manual: repairing more than 1 sample in 200 on vinyl is suspect (reported as a warning).
pub(crate) const MAX_REPAIR_FRACTION: f64 = 1.0 / 200.0;
/// Pitch Protection is recommended when it cuts music repairs by at least this fraction...
const AB_MIN_MUSIC_CUT: f64 = 0.15;
/// ...while the audible clicks found in the gaps change by no more than this fraction. Reverse
/// uses the same gap guard.
const AB_MAX_GAP_LOSS: f64 = 0.05;
/// With no silence between tracks, a record that still grades G+ (heavy wear) is not capped
/// below this. Music hits alone graded Whitney VG (4.5/s); Brubeck at Storyville (live, 1954,
/// 17/s) would otherwise drop to `MIN_DCL`, its applause and crackle filling the music too.
const ESTIMATED_HEAVY_FLOOR: i32 = 30;

/// Click condition of the record, on the Discogs/Goldmine grade scale. It only reflects
/// clicks and pops, not groove wear distortion or warps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCondition {
    /// Fewer than 0.5 audible clicks per second.
    NearMint,
    /// 0.5 to 3 audible clicks per second.
    VeryGoodPlus,
    /// 3 to 10 audible clicks per second.
    VeryGood,
    /// 10 or more audible clicks per second.
    GoodPlus,
}

impl RecordCondition {
    pub(crate) fn from_audible_rate(rate: f64) -> Self {
        if rate < 0.5 {
            RecordCondition::NearMint
        } else if rate < 3.0 {
            RecordCondition::VeryGoodPlus
        } else if rate < 10.0 {
            RecordCondition::VeryGood
        } else {
            RecordCondition::GoodPlus
        }
    }
}

impl std::fmt::Display for RecordCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordCondition::NearMint => write!(f, "NM (near mint)"),
            RecordCondition::VeryGoodPlus => write!(f, "VG+ (light wear)"),
            RecordCondition::VeryGood => write!(f, "VG (moderate wear)"),
            RecordCondition::GoodPlus => write!(f, "G+ (heavy wear)"),
        }
    }
}

/// Music-window measurements at one dcl.
#[derive(Debug, Clone)]
pub struct SweepPointMetrics {
    pub dcl: i32,
    /// Repaired bursts per second, summed over both channels.
    pub music_repairs_per_sec: f64,
    /// Fraction of samples changed by the repair.
    pub repair_fraction: f64,
}

/// Auto-tuner result.
#[derive(Debug, Clone)]
pub struct AutoTuneReport {
    pub recommended: CrParameters,
    pub condition: RecordCondition,
    /// True when no silence between tracks was found and the grade comes from the quietest
    /// music instead (it will under-read, since music masks small clicks).
    pub condition_estimated: bool,
    /// Audible clicks (peak > -40 dBFS) per second in the gaps, summed over both channels.
    pub audible_clicks_per_sec: f64,
    /// Loud pops (peak > -30 dBFS) per second in the gaps, summed over both channels.
    pub loud_pops_per_sec: f64,
    pub gap_seconds_analyzed: f64,
    /// Seconds of audio sampled across the file for the repair-rate sweep.
    pub sampled_seconds: f64,
    pub lr_midband_correlation: f64,
    pub side_to_mid_ratio: f64,
    /// Fraction of music samples changed at the recommended settings.
    pub repair_fraction: f64,
    /// Highest swept dcl at which music repairs per second stay at or below the gap click
    /// rate. Above it, repairs start catching music. None if every swept point is above.
    pub music_pickup_dcl: Option<i32>,
    /// Fractional cut in music repairs with Reverse on (negative means more repairs).
    pub reverse_music_cut: f64,
    /// Fractional cut in music repairs with Pitch Protection on.
    pub pitch_music_cut: f64,
    pub sweep_table: Vec<SweepPointMetrics>,
    pub rationale: Vec<String>,
}

/// Region of the input to process: `start..start + len`, preceded by `pad` warm-up frames.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region {
    pub(crate) start: usize,
    pub(crate) len: usize,
    pub(crate) pad: usize,
}

/// Repairs found over a set of regions.
#[derive(Debug, Clone, Copy, Default)]
struct RepairCount {
    bursts: usize,
    audible: usize,
    pops: usize,
    changed_samples: usize,
    total_samples: usize,
    seconds: f64,
}

impl RepairCount {
    fn per_sec(n: usize, secs: f64) -> f64 {
        n as f64 / secs.max(1e-9)
    }
    fn bursts_per_sec(&self) -> f64 {
        Self::per_sec(self.bursts, self.seconds)
    }
    fn audible_per_sec(&self) -> f64 {
        Self::per_sec(self.audible, self.seconds)
    }
    fn pops_per_sec(&self) -> f64 {
        Self::per_sec(self.pops, self.seconds)
    }
    fn repair_fraction(&self) -> f64 {
        self.changed_samples as f64 / self.total_samples.max(1) as f64
    }
}

/// Automatically tune restoration parameters for given audio frames.
pub fn autotune_audio(frames: &[(i32, i32)], sample_rate: u32, channels: u16) -> AutoTuneReport {
    autotune_audio_with_progress(frames, sample_rate, channels, |_, _| {})
}

/// Automatically tune restoration parameters with a progress callback reporting (fraction, message).
pub fn autotune_audio_with_progress(
    frames: &[(i32, i32)],
    sample_rate: u32,
    channels: u16,
    progress_cb: impl Fn(f32, &str) + Send + Sync,
) -> AutoTuneReport {
    autotune_audio_with_analysis(frames, sample_rate, channels, None, progress_cb)
}

/// As `autotune_audio_with_progress`, reusing an album analysis of the same audio when one is
/// available: the channel mode, the gaps between tracks and the grade (measured over all the
/// silence on the record rather than a 24 s sample) come from it.
pub fn autotune_audio_with_analysis(
    frames: &[(i32, i32)],
    sample_rate: u32,
    channels: u16,
    analysis: Option<&AlbumAnalysis>,
    progress_cb: impl Fn(f32, &str) + Send + Sync,
) -> AutoTuneReport {
    let mut rationale = Vec::new();
    let sr = sample_rate as usize;

    // Warm-up pad before each region, scaled with the processor's decimation.
    let decimation = if sample_rate > 96_000 { 4 } else if sample_rate > 48_000 { 2 } else { 1 };
    let pad = 8192 * decimation;

    progress_cb(0.05, "Finding silence between tracks and sampling the file...");
    let gaps = match analysis {
        Some(a) => a.gap_regions(pad, MAX_GAP_BLOCKS),
        None => find_gap_blocks(frames, sr, pad),
    };
    // Windows sampled evenly over the whole file estimate its repair rate. No loudness
    // filter: quiet passages carry most of the repairs (one worn rip: 1 in 87 samples in the
    // quietest fifth, about 1 in 1000 in the loud passages).
    let samples = find_sample_windows(frames, sr, pad);
    let overlaps_gap = |r: &Region| gaps.iter().any(|g| g.start < r.start + r.len && r.start < g.start + g.len);
    let is_music: Vec<bool> = samples
        .iter()
        .map(|r| !overlaps_gap(r) && rms_db(&frames[r.start..r.start + r.len]) >= MUSIC_MIN_DB)
        .collect();
    let music: Vec<Region> = samples.iter().zip(&is_music).filter(|(_, &m)| m).map(|(r, _)| *r).collect();

    progress_cb(0.10, "Checking groove geometry (stereo vs mono)...");
    let geometry_regions = if music.is_empty() { &samples } else { &music };
    let (mode, lr_corr, side_to_mid) = match analysis {
        Some(a) => (a.mode, a.lr_midband_correlation, a.side_to_mid_ratio),
        None => analyze_groove_geometry(frames, sample_rate, channels, geometry_regions),
    };
    rationale.push(match mode {
        ChannelMode::Merge => format!(
            "Mode: Merge. The channels are nearly identical above 150 Hz (correlation {:.3}, side/mid {:.4}), so this looks like a mono record.",
            lr_corr, side_to_mid
        ),
        ChannelMode::Mono => "Mode: Mono (single-channel or identical channels).".to_string(),
        ChannelMode::Stereo => format!(
            "Mode: Stereo (correlation {:.3}, side/mid {:.4}).",
            lr_corr, side_to_mid
        ),
    });

    let mut base = CrParameters::default();
    base.dcr = 0;
    base.detect_pitch = false;
    base.reverse = false;
    base.method = Method::Wavelet;
    base.mode = mode;

    // Grade the record from the gaps, or from the quietest music when there are none.
    progress_cb(0.20, "Counting clicks between tracks...");
    let condition_estimated = match analysis {
        Some(a) => a.condition_estimated,
        None => gaps.len() < MIN_GAP_BLOCKS,
    };
    let grade_regions: Vec<Region> = if condition_estimated {
        quietest_quarter(frames, geometry_regions)
    } else {
        gaps.clone()
    };
    // (audible clicks/s, loud pops/s, seconds measured)
    let (audible_rate, pops_rate, grade_seconds) = match analysis {
        Some(a) => (a.audible_clicks_per_sec, a.loud_pops_per_sec, a.grade_seconds),
        None => {
            let g = total(&count_repairs(&with_dcl(&base, GRADE_DCL), frames, sample_rate, &grade_regions), None);
            (g.audible_per_sec(), g.pops_per_sec(), g.seconds)
        }
    };
    let condition = RecordCondition::from_audible_rate(audible_rate);
    rationale.push(if condition_estimated {
        format!(
            "Condition: {} (estimated). No silence between tracks was found, so clicks were counted in the quietest {:.0} s of music: {:.1} audible clicks/s, {:.1} loud pops/s. Music masks small clicks, so this may under-read.",
            condition, grade_seconds, audible_rate, pops_rate
        )
    } else {
        format!(
            "Condition: {}. {:.1} audible clicks/s and {:.1} loud pops/s in {:.0} s of silence between tracks.",
            condition, audible_rate, pops_rate, grade_seconds
        )
    });

    let condition_dcl = dcl_for_audible_rate(audible_rate);

    // Sweep the sampled windows: repair rate over all of them, repairs/s over the music ones.
    progress_cb(0.35, "Measuring repairs across sensitivities...");
    let sweep: Vec<SweepPointMetrics> = par_map(&SWEEP, |&dcl| {
        let per_window = count_repairs(&with_dcl(&base, dcl), frames, sample_rate, &samples);
        SweepPointMetrics {
            dcl,
            music_repairs_per_sec: total(&per_window, Some(&is_music)).bursts_per_sec(),
            repair_fraction: total(&per_window, None).repair_fraction(),
        }
    });
    let sampled_seconds = samples.iter().map(|r| r.len).sum::<usize>() as f64 / sample_rate as f64;

    // Music pickup: highest dcl whose music repair rate stays within the surface's own rate.
    // On a near-mint record the surface rate is close to zero, so allow 1 repair/s.
    let pickup_limit = if condition == RecordCondition::NearMint { audible_rate.max(1.0) } else { audible_rate };
    let music_pickup_dcl = sweep
        .iter()
        .take_while(|m| m.music_repairs_per_sec <= pickup_limit)
        .last()
        .map(|m| m.dcl);
    rationale.push(match music_pickup_dcl {
        Some(d) if d == MAX_DCL => format!(
            "Music pickup: repairs in the music stay within the surface click rate up to dcl {}.",
            MAX_DCL
        ),
        Some(d) => format!(
            "Music pickup: above about dcl {} repairs in the music outnumber the clicks on the surface, so some will be music.",
            d
        ),
        None => format!(
            "Music pickup: even at dcl {} repairs in the music outnumber the clicks on the surface; check the removed noise carefully.",
            MIN_DCL
        ),
    });

    // The repair rate is reported, not used to move dcl. A few click-dense seconds (scratches,
    // run-out grooves) dominate it, so 64 sampled seconds only pin it to about +-50% (95%),
    // and worn records sit close to the 1 in 200 line (one rip at dcl 45: 1 in 284 over the
    // whole file). The cap at 50 already sits on that line for worn records.
    let chosen_dcl = choose_dcl(condition, condition_dcl, music_pickup_dcl, condition_estimated);
    rationale.push(if chosen_dcl < condition_dcl {
        format!(
            "DeClick: {} clicks/s maps to dcl {}, but with no silence between tracks those were counted in the music, where music hits look like clicks. Starting lower, at {}{}. Raise it if clicks remain.",
            format_rate(audible_rate),
            condition_dcl,
            chosen_dcl,
            if music_pickup_dcl.is_some_and(|d| d >= chosen_dcl) { ", where music pickup starts" } else { " (heavy wear)" }
        )
    } else {
        format!("DeClick: {} audible clicks/s maps to dcl {}.", format_rate(audible_rate), condition_dcl)
    });

    // Reverse and Pitch Protection A/B at the chosen dcl.
    progress_cb(0.70, "Comparing Reverse and Pitch Protection...");
    let chosen = with_dcl(&base, chosen_dcl);
    let mut rev = chosen.clone();
    rev.reverse = true;
    let mut pitch = chosen.clone();
    pitch.detect_pitch = true;
    let gap_regions = &grade_regions;
    // Per option: (all sampled windows, music windows only, gaps).
    let runs: Vec<(RepairCount, RepairCount, RepairCount)> = par_map(&[chosen.clone(), rev, pitch], |p| {
        let per_window = count_repairs(p, frames, sample_rate, &samples);
        (
            total(&per_window, None),
            total(&per_window, Some(&is_music)),
            total(&count_repairs(p, frames, sample_rate, gap_regions), None),
        )
    });
    let (base_all, base_music, base_gap) = runs[0];
    // With no silence between tracks the "gaps" are the quietest music, where an option that
    // removes false repairs would look like it loses real clicks: decide on the music alone.
    let ab = |run: &(RepairCount, RepairCount, RepairCount)| {
        let gaps = (!condition_estimated).then_some((base_gap.audible, run.2.audible));
        ab_choice(base_music.bursts, run.1.bursts, gaps)
    };
    let reverse_gaps = (!condition_estimated).then_some((base_gap.audible, runs[1].2.audible));
    let (reverse_music_cut, reverse_verdict) = reverse_choice(base_music.bursts, runs[1].1.bursts, reverse_gaps);
    let use_reverse = reverse_verdict == ReverseVerdict::On;
    let (pitch_music_cut, use_pitch) = ab(&runs[2]);
    rationale.push(reverse_sentence(reverse_music_cut, reverse_verdict));
    rationale.push(ab_sentence("Pitch Protection", pitch_music_cut, use_pitch));

    let mut recommended = chosen;
    recommended.reverse = use_reverse;
    recommended.detect_pitch = use_pitch;

    // With both options on there is no combined run; the plain run is an upper bound.
    let repair_fraction = match (use_reverse, use_pitch) {
        (true, false) => runs[1].0.repair_fraction(),
        (false, true) => runs[2].0.repair_fraction(),
        _ => base_all.repair_fraction(),
    };
    rationale.push(if repair_fraction > 0.0 {
        format!(
            "Repair rate at these settings: roughly 1 in {:.0} samples, estimated from {:.0} s sampled across the file (the full run reports the exact figure).",
            1.0 / repair_fraction,
            sampled_seconds
        )
    } else {
        "Repair rate at these settings: no samples changed in the windows measured.".to_string()
    });
    // Only warn beyond the sampling error: on one rip the estimate read 1 in 192 where the
    // full run measured 1 in 309.
    if repair_fraction > 1.5 * MAX_REPAIR_FRACTION {
        rationale.push(
            "Warning: that is likely past the ClickRepair manual's 1 in 200 line for vinyl. It may still be the right trade on a worn record; listen to the removed noise and lower dcl if it carries any of the music.".to_string(),
        );
    } else {
        rationale.push("Listen to the removed noise; it should carry no hint of the music.".to_string());
    }

    progress_cb(1.0, "Calibration complete.");
    AutoTuneReport {
        recommended,
        condition,
        condition_estimated,
        audible_clicks_per_sec: audible_rate,
        loud_pops_per_sec: pops_rate,
        gap_seconds_analyzed: grade_seconds,
        sampled_seconds,
        lr_midband_correlation: lr_corr,
        side_to_mid_ratio: side_to_mid,
        repair_fraction,
        music_pickup_dcl,
        reverse_music_cut,
        pitch_music_cut,
        sweep_table: sweep,
        rationale,
    }
}

/// Maps audible clicks/s to a starting dcl on a log scale through two calibration anchors:
/// 0.2 clicks/s (near mint) -> 12 and 17 clicks/s (G+) -> 45. Rounded to 5, clamped 10..=50.
pub fn dcl_for_audible_rate(rate: f64) -> i32 {
    let raw = 12.0 + 17.0 * (rate.max(1e-6) / 0.2).log10();
    let rounded = (raw / 5.0).round() as i32 * 5;
    rounded.clamp(MIN_DCL, MAX_DCL)
}

/// The suggested dcl. With silence between tracks it is the condition's dcl. Without (an
/// estimated grade, counted in the quietest music), music hits count as clicks and inflate it
/// (Whitney: 4.5/s in the music -> 35, where pickup starts above 15 and the user's ear chose
/// 13 with Reverse), so it is capped at the music-pickup crossover (`MIN_DCL` if even that
/// catches music). A gapless record that grades G+ keeps at least `ESTIMATED_HEAVY_FLOOR`.
/// On 31 rips (7 gapless) the suggestions became: Whitney 35 -> 15, Billy Idol 40 -> 35, Duke
/// & Louis 45 -> 35, Doors 50 -> 40, Whipped Cream 50 -> 40, Jeff Beck Truth 45 -> 30,
/// Storyville 45 -> 30; the 24 records with gaps are unchanged.
pub(crate) fn choose_dcl(condition: RecordCondition, condition_dcl: i32, music_pickup_dcl: Option<i32>, estimated: bool) -> i32 {
    if !estimated {
        return condition_dcl;
    }
    let floor = if condition == RecordCondition::GoodPlus { ESTIMATED_HEAVY_FLOOR } else { MIN_DCL };
    condition_dcl.min(music_pickup_dcl.unwrap_or(MIN_DCL).max(floor))
}

/// A/B decision for Pitch Protection. Returns the fractional cut in music repairs and whether
/// to switch it on: it must cut music repairs by at least 15% while keeping the audible clicks
/// in the gaps within 5%. On a saxophone record it cut music repairs by 26% with the gap
/// count unchanged (see the design doc).
fn ab_choice(base_music: usize, music: usize, gaps: Option<(usize, usize)>) -> (f64, bool) {
    if base_music == 0 {
        return (0.0, false);
    }
    let music_cut = 1.0 - music as f64 / base_music as f64;
    // (audible clicks in the gaps without and with the option); None: no gaps to guard.
    let gap_loss = gaps.map_or(0.0, |(base_gap_audible, gap_audible)| 1.0 - gap_audible as f64 / base_gap_audible.max(1) as f64);
    (music_cut, music_cut >= AB_MIN_MUSIC_CUT && gap_loss <= AB_MAX_GAP_LOSS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReverseVerdict {
    On,
    /// More repairs in the music than forward scanning: likely slow attacks with sudden ends,
    /// which Reverse sees as sharp attacks.
    MoreMusicRepairs,
    /// Real clicks lost in the gaps between tracks.
    LosesGapClicks,
}

/// Reverse decision. Reverse is the default: on the detection benchmark (real record noise on
/// clean music) it found the same clicks as forward scanning with a
/// third to a half of the false repairs at every DeClick. A cut in the music's repair count
/// understates that (real clicks are counted too), so no minimum cut is asked for; Reverse is
/// turned off only on evidence of harm. Returns the fractional cut in music repairs too.
fn reverse_choice(base_music: usize, music: usize, gaps: Option<(usize, usize)>) -> (f64, ReverseVerdict) {
    let music_cut = if base_music == 0 { 0.0 } else { 1.0 - music as f64 / base_music as f64 };
    let gap_loss = gaps
        .filter(|&(base_gap_audible, _)| base_gap_audible > 0)
        .map_or(0.0, |(base_gap_audible, gap_audible)| 1.0 - gap_audible as f64 / base_gap_audible as f64);
    let verdict = if gap_loss > AB_MAX_GAP_LOSS {
        ReverseVerdict::LosesGapClicks
    } else if music > base_music {
        ReverseVerdict::MoreMusicRepairs
    } else {
        ReverseVerdict::On
    };
    (music_cut, verdict)
}

fn reverse_sentence(cut: f64, verdict: ReverseVerdict) -> String {
    match verdict {
        ReverseVerdict::On if cut >= 0.005 => format!(
            "Reverse: on (the default). It cut repairs in the music by {:.0}%, fewer false repairs on drum hits and other sharp attacks, while keeping the clicks between tracks.",
            cut * 100.0
        ),
        ReverseVerdict::On => "Reverse: on (the default), which avoids false repairs on drum hits and other sharp attacks.".to_string(),
        ReverseVerdict::MoreMusicRepairs => format!(
            "Reverse: off. It made {:.0}% more repairs in the music; this record may have sounds that stop suddenly (gated drums, cut-off notes), which Reverse mistakes for clicks.",
            -cut * 100.0
        ),
        ReverseVerdict::LosesGapClicks => {
            "Reverse: off. It missed some of the clicks between tracks that forward scanning repairs.".to_string()
        }
    }
}

fn format_rate(rate: f64) -> String {
    if rate < 1.0 { format!("{:.2}", rate) } else { format!("{:.1}", rate) }
}

fn ab_sentence(name: &str, cut: f64, chosen: bool) -> String {
    if chosen {
        format!(
            "{}: on. It cut repairs in the music by {:.0}% while keeping the clicks between tracks.",
            name,
            cut * 100.0
        )
    } else if cut.abs() < 0.005 {
        format!("{}: off (no change in repairs in the music).", name)
    } else {
        format!("{}: off (repairs in the music changed by {:+.0}%).", name, -cut * 100.0)
    }
}

fn with_dcl(base: &CrParameters, dcl: i32) -> CrParameters {
    let mut p = base.clone();
    p.dcl = dcl;
    p
}

#[cfg(feature = "parallel")]
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R> {
    items.par_iter().map(f).collect()
}

#[cfg(not(feature = "parallel"))]
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R> {
    items.iter().map(f).collect()
}

pub(crate) fn rms_db(frames: &[(i32, i32)]) -> f64 {
    let sum: f64 = frames
        .iter()
        .map(|&(l, r)| 0.5 * ((l as f64) * (l as f64) + (r as f64) * (r as f64)))
        .sum();
    let rms = (sum / frames.len().max(1) as f64).sqrt();
    20.0 * (rms.max(1e-9) / FULL_SCALE).log10()
}

/// Picks up to `n` items spread evenly over `items`.
pub(crate) fn spread<T: Copy>(items: &[T], n: usize) -> Vec<T> {
    if items.len() <= n {
        return items.to_vec();
    }
    (0..n).map(|k| items[k * items.len() / n]).collect()
}

/// 1 s blocks of groove noise between tracks, skipping the first 2 s (needle drop).
fn find_gap_blocks(frames: &[(i32, i32)], sr: usize, pad: usize) -> Vec<Region> {
    let first = (2 * sr).max(pad);
    let mut blocks = Vec::new();
    let mut start = first;
    while start + sr <= frames.len() {
        let db = rms_db(&frames[start..start + sr]);
        if db > GAP_MIN_DB && db < GAP_MAX_DB {
            blocks.push(Region { start, len: sr, pad });
        }
        start += sr;
    }
    spread(&blocks, MAX_GAP_BLOCKS)
}

/// Up to 64 one-second windows spaced evenly over the file after the needle drop. Short
/// files get back-to-back windows. 64 windows estimate a whole album's repair rate within
/// about 15%; 16 two-second windows could be off by 3x.
fn find_sample_windows(frames: &[(i32, i32)], sr: usize, pad: usize) -> Vec<Region> {
    let total = frames.len();
    let first = (2 * sr).max(pad);
    if total < first + sr {
        // Under 3 s: one window over whatever there is.
        let start = pad.min(total);
        return vec![Region { start, len: total - start, pad: start }];
    }
    let slots = (total - first) / sr;
    let n = slots.min(SAMPLE_WINDOWS);
    (0..n)
        .map(|k| Region { start: first + (slots * k / n) * sr, len: sr, pad })
        .collect()
}

/// The quietest quarter of the music windows (at least one).
fn quietest_quarter(frames: &[(i32, i32)], music: &[Region]) -> Vec<Region> {
    let mut by_level: Vec<(f64, Region)> = music
        .iter()
        .map(|r| (rms_db(&frames[r.start..r.start + r.len]), *r))
        .collect();
    by_level.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let n = (music.len() / 4).max(1).min(by_level.len());
    by_level.into_iter().take(n).map(|(_, r)| r).collect()
}

/// Sums per-region counts, optionally only those where `mask` is true.
fn total(counts: &[RepairCount], mask: Option<&[bool]>) -> RepairCount {
    let mut sum = RepairCount::default();
    for (i, c) in counts.iter().enumerate() {
        if mask.map_or(true, |m| m[i]) {
            sum.bursts += c.bursts;
            sum.audible += c.audible;
            sum.pops += c.pops;
            sum.changed_samples += c.changed_samples;
            sum.total_samples += c.total_samples;
            sum.seconds += c.seconds;
        }
    }
    sum
}

/// Processes each region with `params` and counts the repaired bursts in the difference
/// (original - repaired), per channel, summed over both channels. One count per region.
fn count_repairs(params: &CrParameters, frames: &[(i32, i32)], sample_rate: u32, regions: &[Region]) -> Vec<RepairCount> {
    let audible = FULL_SCALE * 10f64.powf(AUDIBLE_DB / 20.0);
    let pop = FULL_SCALE * 10f64.powf(POP_DB / 20.0);
    let mut counts = Vec::with_capacity(regions.len());
    for r in regions {
        let mut count = RepairCount::default();
        let slice = &frames[r.start - r.pad..r.start + r.len];
        let (out, _) = Processor::process_slice_aligned(params.clone(), sample_rate, slice);
        for ch in 0..2 {
            let diff: Vec<i32> = (0..r.len)
                .map(|i| {
                    let (o, p) = (slice[r.pad + i], out[r.pad + i]);
                    if ch == 0 { o.0 - p.0 } else { o.1 - p.1 }
                })
                .collect();
            for (s, e) in diff_bursts(&diff) {
                let peak = diff[s..e].iter().map(|v| v.unsigned_abs()).max().unwrap_or(0) as f64;
                count.bursts += 1;
                count.audible += (peak > audible) as usize;
                count.pops += (peak > pop) as usize;
            }
            count.changed_samples += diff.iter().filter(|v| v.abs() > 2).count();
        }
        count.total_samples += 2 * r.len;
        count.seconds += r.len as f64 / sample_rate as f64;
        counts.push(count);
    }
    counts
}

/// Groups changed samples (|d| > 2, above wavelet round-off) into bursts `[start, end)`,
/// bridging single unchanged samples (zero crossings inside one repair).
fn diff_bursts(d: &[i32]) -> Vec<(usize, usize)> {
    let n = d.len();
    let active = |k: usize| d[k].abs() > 2;
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if active(i) {
            let start = i;
            let mut end = i + 1;
            while end < n {
                if active(end) {
                    end += 1;
                } else if end + 1 < n && active(end + 1) {
                    end += 2;
                } else {
                    break;
                }
            }
            out.push((start, end));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Stereo vs Merge (mono record captured in stereo) vs Mono (identical channels).
/// High-passes both channels at 150 Hz to remove vertical rumble, then compares them.
pub(crate) fn analyze_groove_geometry(
    frames: &[(i32, i32)],
    sample_rate: u32,
    channels: u16,
    music: &[Region],
) -> (ChannelMode, f64, f64) {
    // Checked on the music windows, so digital silence at the start cannot fake a mono file.
    let identical = music
        .iter()
        .all(|r| frames[r.start..r.start + r.len].iter().all(|&(l, rr)| l == rr));
    if channels == 1 || identical {
        return (ChannelMode::Mono, 1.0, 0.0);
    }

    let alpha = 1.0 / (1.0 + 2.0 * std::f64::consts::PI * 150.0 / sample_rate as f64);
    let (mut sum_l, mut sum_r, mut sum_ll, mut sum_rr, mut sum_lr) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut side, mut mid) = (0.0f64, 0.0f64);
    let mut n = 0usize;
    for r in music {
        let (mut yl, mut yr, mut xl, mut xr) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &(l, rr) in &frames[r.start..r.start + r.len] {
            let (l, rr) = (l as f64, rr as f64);
            yl = alpha * (yl + l - xl);
            yr = alpha * (yr + rr - xr);
            xl = l;
            xr = rr;
            sum_l += yl;
            sum_r += yr;
            sum_ll += yl * yl;
            sum_rr += yr * yr;
            sum_lr += yl * yr;
            side += (yl - yr) * (yl - yr);
            mid += (yl + yr) * (yl + yr);
            n += 1;
        }
    }
    let nf = n.max(1) as f64;
    let cov = sum_lr - sum_l * sum_r / nf;
    let var_l = sum_ll - sum_l * sum_l / nf;
    let var_r = sum_rr - sum_r * sum_r / nf;
    let corr = cov / (var_l * var_r).sqrt().max(1e-12);
    let side_to_mid = side / mid.max(1.0);

    // Bounds from a real mono LP (corr 0.960, side/mid 0.020); stereo rips measured 0.57-0.70.
    let mode = if corr > 0.93 && side_to_mid < 0.04 { ChannelMode::Merge } else { ChannelMode::Stereo };
    (mode, corr, side_to_mid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dcl_mapping_hits_calibration_anchors() {
        assert_eq!(dcl_for_audible_rate(0.0), 10);
        assert_eq!(dcl_for_audible_rate(0.2), 10); // 12 rounds to 10
        assert_eq!(dcl_for_audible_rate(2.4), 30);
        assert_eq!(dcl_for_audible_rate(8.1), 40);
        assert_eq!(dcl_for_audible_rate(17.0), 45);
        assert_eq!(dcl_for_audible_rate(1000.0), 50);
    }

    #[test]
    fn test_grade_bands() {
        assert_eq!(RecordCondition::from_audible_rate(0.2), RecordCondition::NearMint);
        assert_eq!(RecordCondition::from_audible_rate(2.4), RecordCondition::VeryGoodPlus);
        assert_eq!(RecordCondition::from_audible_rate(8.1), RecordCondition::VeryGood);
        assert_eq!(RecordCondition::from_audible_rate(17.0), RecordCondition::GoodPlus);
    }

    #[test]
    fn test_choose_dcl_caps_only_gapless_records() {
        use RecordCondition::{GoodPlus, VeryGood};
        // With gaps: the condition's dcl, whatever the pickup.
        assert_eq!(choose_dcl(VeryGood, 35, Some(15), false), 35);
        assert_eq!(choose_dcl(GoodPlus, 45, None, false), 45);
        // Without: capped at the pickup crossover, never raised by it.
        assert_eq!(choose_dcl(VeryGood, 35, Some(15), true), 15);
        assert_eq!(choose_dcl(VeryGood, 30, Some(40), true), 30);
        assert_eq!(choose_dcl(GoodPlus, 45, Some(45), true), 45);
        assert_eq!(choose_dcl(VeryGood, 35, None, true), MIN_DCL);
        // A heavy-wear gapless record keeps a raised floor.
        assert_eq!(choose_dcl(GoodPlus, 45, Some(10), true), ESTIMATED_HEAVY_FLOOR);
        assert_eq!(choose_dcl(GoodPlus, 45, None, true), ESTIMATED_HEAVY_FLOOR);
        assert_eq!(choose_dcl(GoodPlus, 50, Some(40), true), 40);
    }

    #[test]
    fn test_ab_choice_thresholds() {
        // Pitch Protection rule. Saxophone record: cut music repairs 26%, gap clicks unchanged -> on.
        assert!(ab_choice(1000, 740, Some((100, 100))).1);
        // Only a 10% cut -> off.
        assert!(!ab_choice(1000, 900, Some((100, 100))).1);
        // Big cut but it also loses 10% of the real clicks in the gaps -> off.
        assert!(!ab_choice(1000, 700, Some((100, 90))).1);
        // More repairs with the option -> off, reported as a negative cut.
        let (cut, on) = ab_choice(1000, 1200, Some((100, 100)));
        assert!(!on && cut < 0.0);
        // Nothing repaired in the music -> nothing to judge.
        assert_eq!(ab_choice(0, 0, Some((0, 0))), (0.0, false));
        // No gaps to guard (gapless record): the music cut alone decides.
        assert!(ab_choice(1000, 820, None).1);
        assert!(!ab_choice(1000, 900, None).1);
    }

    #[test]
    fn test_reverse_choice_defaults_on() {
        use ReverseVerdict::*;
        // Any cut, however small, with the gap clicks kept -> on (no minimum cut).
        assert_eq!(reverse_choice(1000, 900, Some((100, 100))).1, On);
        assert_eq!(reverse_choice(1000, 1000, Some((100, 98))).1, On);
        // Nothing repaired in the music -> on.
        assert_eq!(reverse_choice(0, 0, Some((0, 0))), (0.0, On));
        // More repairs in the music -> off, reported as a negative cut.
        let (cut, v) = reverse_choice(1000, 1100, Some((100, 100)));
        assert!(cut < 0.0 && v == MoreMusicRepairs);
        // Loses more than 5% of the gap clicks -> off, whatever the music cut.
        assert_eq!(reverse_choice(1000, 700, Some((100, 90))).1, LosesGapClicks);
        // No gaps to guard (gapless record): only the music count can turn it off.
        assert_eq!(reverse_choice(1000, 950, None).1, On);
        assert_eq!(reverse_choice(1000, 1050, None).1, MoreMusicRepairs);
    }

    #[test]
    fn test_diff_bursts_bridges_single_zero() {
        let d = [0, 5, 0, 7, 0, 0, 9, 0];
        assert_eq!(diff_bursts(&d), vec![(1, 4), (6, 7)]);
    }
}
