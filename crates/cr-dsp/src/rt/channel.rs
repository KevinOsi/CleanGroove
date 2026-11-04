//! One channel of the real-time engine: takes in a block, scans it backward for clicks, repairs
//! them, and copies each repair out to the processor's buffer as the output reaches it.
//!
//! See docs/engine/REALTIME_ENGINE.md, sections 3, 5 and 6.

use crate::burg::update_excitation_rt;
use crate::detector::BandState;
use crate::flag;
use crate::params::{DerivedRtParams, RtParameters};
use crate::reconstruct::{fill_gap_bounded, ScratchSpace};
use crate::rt::detector::{
    find_defect_edges_rt, CHANNEL_BUFFER_LEN, CHANNEL_BUFFER_MASK, MAX_PRED, MAX_REPAIR,
    SUPER_BUFFER_MASK,
};
use crate::rt::pitch::protect_pitched_repairs_rt;
use crate::wavelet::{band_spans_to_time_span, time_span_to_band_span, D4Analyzer};

/// Band samples taken in per call to `process_block`.
const BLOCK: isize = 512;

// Fixed points behind the newest sample (`frame_count`), in band samples.
/// The output: a sample leaves the channel this long after it entered.
const OUTPUT_DELAY: isize = CHANNEL_BUFFER_LEN as isize;
/// The prediction errors of a 256-sample block are computed once its last sample is this old.
const EXCITATION_DELAY: isize = 384;
/// Repairs are counted as they pass this point...
const CHECK_IN_DELAY: isize = 2304;
/// ...and pitch protection looks at them here.
const PITCH_DELAY: isize = 3328;

// The backward scan, as distances behind the newest sample.
/// Where each scan starts...
const SCAN_START: isize = 512;
/// ...the least it covers...
const SCAN_MIN_REACH: isize = SCAN_START + BLOCK + EXCITATION_DELAY;
/// ...and the furthest it goes.
const SCAN_LIMIT: isize = 2048;
/// A click still open this many samples after its trailing edge is closed anyway.
const MAX_CLICK_SCAN: isize = 256;

/// Notices when a run of marked samples reaches a fixed point: true on its first sample.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunStart {
    inside: bool,
}

impl RunStart {
    fn reached(&mut self, marked: bool) -> bool {
        let first = marked && !self.inside;
        self.inside = marked;
        first
    }
}

/// A repair being copied out as it passes the output.
#[derive(Debug, Clone)]
pub struct RepairExport {
    /// Output position of its first sample, while one is passing.
    pub start: Option<isize>,
    /// Longer than `MAX_REPAIR`: it is dropped rather than copied out.
    pub too_long: bool,
    pub samples: [i32; MAX_REPAIR],
}

impl RepairExport {
    fn push(&mut self, pos: isize, values: &[i32]) {
        let start = *self.start.get_or_insert(pos);
        let offset = (pos - start) as usize;
        if offset + values.len() <= MAX_REPAIR {
            self.samples[offset..offset + values.len()].copy_from_slice(values);
        } else {
            self.too_long = true;
        }
    }
}

/// Where the backward scan stands in one band.
#[derive(Debug, Clone, Copy)]
enum BandScan {
    Idle,
    /// Inside a click whose trailing edge is at `right`.
    InClick { right: isize },
    /// The click ended at `left`; its leading edge is still to be refined against `pre_ref`,
    /// the pre-shoulder sum there.
    Ended { left: isize, right: isize, pre_ref: f64 },
}

pub struct RtChannel {
    pub params: RtParameters,
    pub derived: DerivedRtParams,
    pub is_wavelet: bool,
    /// Wavelet bands above 48 kHz: the processor's buffer has its own high band to repair.
    pub decimated: bool,

    pub min_pitch: usize,
    pub curr_pitch: usize,

    /// Band samples taken in (the newest is at `frame_count - 1`).
    pub frame_count: isize,
    /// `frame_count` at the end of the current block.
    pub block_end: isize,
    /// Next position to read in the processor's buffer.
    pub read_pos: isize,
    /// The earliest repair made in the first block of the last scan; the next scan reaches
    /// back to it.
    pub reach_back_to: Option<isize>,
    /// Repairs between the check-in point and the output.
    pub repairs_pending: usize,

    at_check_in: RunStart,
    at_pitch: RunStart,
    at_output: RunStart,
    pub export: RepairExport,

    /// The window sums in `lo`/`hi` are current for the scan position.
    pub sums_valid: bool,

    pub lo: BandState,
    pub hi: BandState,
    pub flags: Vec<u8>,

    pub d4: D4Analyzer,
    pub scratch: ScratchSpace,
}

impl RtChannel {
    pub fn new(params: RtParameters, channel_rate: u32, decimated: bool) -> Self {
        let is_wavelet = params.method.is_wavelet();
        let derived = params.compute_derived();
        let min_pitch = if is_wavelet { channel_rate / 2940 } else { channel_rate / 1470 } as usize;

        Self {
            params,
            derived,
            is_wavelet,
            decimated,
            min_pitch,
            curr_pitch: 0,
            frame_count: 0,
            block_end: 0,
            read_pos: 0,
            reach_back_to: None,
            repairs_pending: 0,
            at_check_in: RunStart::default(),
            at_pitch: RunStart::default(),
            at_output: RunStart::default(),
            export: RepairExport { start: None, too_long: false, samples: [0; MAX_REPAIR] },
            sums_valid: false,
            lo: BandState::new(CHANNEL_BUFFER_LEN, 0),
            hi: BandState::new(CHANNEL_BUFFER_LEN, 0),
            flags: vec![0; CHANNEL_BUFFER_LEN],
            d4: D4Analyzer::new(),
            scratch: ScratchSpace::new(),
        }
    }

    pub fn set_parameters(&mut self, params: RtParameters) {
        self.derived = params.compute_derived();
        self.params = params;
    }

    fn flag_at(&self, pos: isize) -> u8 {
        self.flags[(pos as usize) & CHANNEL_BUFFER_MASK]
    }

    /// Takes in one block, then scans it for clicks.
    pub fn process_block(
        &mut self,
        ext_samples: &mut [i32],
        mut ext_hi_samples: Option<&mut [i32]>,
        ext_flags: &mut [u8],
    ) {
        self.block_end += BLOCK;

        while self.frame_count < self.block_end || self.export.start.is_some() {
            if self.frame_count >= OUTPUT_DELAY {
                self.pass_output(ext_samples, ext_hi_samples.as_deref_mut(), ext_flags);
            }
            self.take_input(ext_samples);

            let fit_end = self.frame_count - EXCITATION_DELAY;
            if fit_end >= 0 && (fit_end & 0xFF) == 0 {
                let pos = self.frame_count - 256;
                self.update_excitations(pos, pos);
            }

            let pitch_pos = self.frame_count - PITCH_DELAY;
            if self.params.detect_pitch && pitch_pos >= 0 {
                let marked = self.flag_at(pitch_pos) & flag::REPAIRED_OR_PROTECTED != 0;
                if self.at_pitch.reached(marked) && self.repairs_pending > 4 {
                    protect_pitched_repairs_rt(
                        &mut self.lo.samples,
                        &self.lo.original,
                        &mut self.hi.samples,
                        &self.hi.original,
                        &mut self.flags,
                        CHANNEL_BUFFER_MASK,
                        pitch_pos,
                        self.min_pitch,
                        &mut self.curr_pitch,
                        self.is_wavelet,
                        &mut self.scratch,
                    );
                }
            }

            let check_in = self.frame_count - CHECK_IN_DELAY;
            if check_in >= 0 && self.at_check_in.reached(self.flag_at(check_in) & flag::REPAIRED != 0) {
                self.repairs_pending += 1;
            }
        }

        if self.params.sensitivity.is_enabled() {
            self.scan_for_clicks();
        }
    }

    /// The sample at the output: copies repairs out, marks protected samples.
    fn pass_output(
        &mut self,
        ext_samples: &mut [i32],
        mut ext_hi_samples: Option<&mut [i32]>,
        ext_flags: &mut [u8],
    ) {
        let out = self.frame_count - OUTPUT_DELAY;
        if self.frame_count > PITCH_DELAY {
            let marked = self.flag_at(out + 2) & flag::REPAIRED_OR_PROTECTED != 0;
            if self.at_output.reached(marked) {
                self.repairs_pending = self.repairs_pending.saturating_sub(1);
            }
        }

        // In Wavelet mode one band sample is two output samples.
        let (marks, pos) = if self.is_wavelet {
            (self.flag_at(out) | self.flag_at(out + 1), 2 * out)
        } else {
            (self.flag_at(out), out)
        };

        if marks & flag::PROTECTED != 0 {
            self.finish_export(pos, ext_samples, ext_hi_samples.as_deref_mut(), ext_flags);
            ext_flags[(pos as usize) & SUPER_BUFFER_MASK] |= flag::PROTECTED;
            if self.is_wavelet {
                ext_flags[((pos + 1) as usize) & SUPER_BUFFER_MASK] |= flag::PROTECTED;
            }
        } else if marks & flag::REPAIRED != 0 {
            let i0 = (out as usize) & CHANNEL_BUFFER_MASK;
            if self.is_wavelet {
                let i1 = (i0 + 1) & CHANNEL_BUFFER_MASK;
                let (even, odd) = crate::wavelet::synthesize_zero_up(
                    self.lo.samples[i0],
                    self.hi.samples[i0],
                    self.lo.samples[i1],
                    self.hi.samples[i1],
                );
                self.export.push(pos, &[even, odd]);
            } else {
                self.export.push(pos, &[self.lo.samples[i0]]);
            }
        } else {
            self.finish_export(pos, ext_samples, ext_hi_samples, ext_flags);
        }
    }

    /// Writes the passing repair, which ends at `end`, into the processor's buffer (unless it
    /// was too long), and fills the high band there over the same span.
    fn finish_export(
        &mut self,
        end: isize,
        ext_samples: &mut [i32],
        ext_hi_samples: Option<&mut [i32]>,
        ext_flags: &mut [u8],
    ) {
        let Some(start) = self.export.start.take() else { return };
        if std::mem::take(&mut self.export.too_long) {
            return;
        }

        let count = ((end - start) as usize).min(MAX_REPAIR);
        for i in 0..count {
            let pos = ((start + i as isize) as usize) & SUPER_BUFFER_MASK;
            ext_samples[pos] = self.export.samples[i];
            ext_flags[pos] |= flag::REPAIRED;
        }

        if let (true, Some(hi_buf)) = (self.decimated, ext_hi_samples) {
            fill_gap_bounded(
                hi_buf,
                SUPER_BUFFER_MASK,
                [start, start + count as isize],
                MAX_REPAIR,
                MAX_PRED,
                true,
                &mut self.export.samples,
                &mut self.scratch,
            );
            for i in 0..count {
                hi_buf[((start + i as isize) as usize) & SUPER_BUFFER_MASK] = self.export.samples[i];
            }
        }
    }

    /// Takes the next sample (Simple) or pair (Wavelet) from the processor's buffer.
    fn take_input(&mut self, ext_samples: &[i32]) {
        let i = (self.frame_count as usize) & CHANNEL_BUFFER_MASK;
        let read = |pos: isize| ext_samples[(pos as usize) & SUPER_BUFFER_MASK];
        if self.is_wavelet {
            let (lo, hi) = self.d4.analyze(read(self.read_pos), read(self.read_pos + 1));
            self.read_pos += 2;
            self.lo.samples[i] = lo;
            self.lo.original[i] = lo;
            self.hi.samples[i] = hi;
            self.hi.original[i] = hi;
        } else {
            let s = read(self.read_pos);
            self.read_pos += 1;
            self.lo.samples[i] = s;
            self.lo.original[i] = s;
        }
        self.flags[i] = 0;
        self.frame_count += 1;
    }

    /// Prediction errors over `[start, end]` from a fresh fit, in each band.
    fn update_excitations(&mut self, start: isize, end: isize) {
        update_band_excitation(&mut self.lo, self.frame_count, start, end, &mut self.scratch);
        if self.is_wavelet {
            update_band_excitation(&mut self.hi, self.frame_count, start, end, &mut self.scratch);
        }
    }

    /// The backward scan over the newest samples (realtime engine doc, section 5).
    pub fn scan_for_clicks(&mut self) {
        let mask = CHANNEL_BUFFER_MASK;
        let peak_len = self.derived.windows.peak_len as isize;
        let shoulder_len = self.derived.windows.shoulder_len as isize;
        let detect_factor = self.derived.sensitivity.detect_factor;
        let onset_factor = self.derived.sensitivity.onset_factor;
        let edge: isize = if self.is_wavelet { 64 } else { 128 };

        let reach = match self.reach_back_to.take() {
            Some(p) => SCAN_MIN_REACH.max(self.frame_count + EXCITATION_DELAY - p),
            None => SCAN_MIN_REACH,
        }
        .min(SCAN_LIMIT);

        let mut lo_scan = BandScan::Idle;
        let mut hi_scan = BandScan::Idle;
        // A band is loud outside a click: keep going past `reach` until it is resolved.
        let mut busy = false;
        self.sums_valid = false;
        let mut offset = SCAN_START;
        let mut scan_pos = self.frame_count - offset;

        while (offset < reach || busy) && scan_pos > edge {
            offset += 1;
            scan_pos -= 1;

            let peak_start = scan_pos;
            let peak_end = peak_start + peak_len;
            let windows = [peak_start - shoulder_len, peak_start, peak_end, peak_end + shoulder_len];
            if self.sums_valid {
                slide_sums(&mut self.lo, windows, mask);
                if self.is_wavelet {
                    slide_sums(&mut self.hi, windows, mask);
                }
            } else {
                self.sums_valid = true;
                fill_sums(&mut self.lo, windows, mask);
                if self.is_wavelet {
                    fill_sums(&mut self.hi, windows, mask);
                }
            }

            let lo_quiet = quiet(&self.lo, detect_factor);
            let hi_quiet = !self.is_wavelet || quiet(&self.hi, detect_factor);
            busy |= track_click(&mut lo_scan, &self.lo, lo_quiet, scan_pos, peak_len, onset_factor, mask);
            busy |= track_click(&mut hi_scan, &self.hi, hi_quiet, scan_pos, peak_len, onset_factor, mask);

            // Go on once no band is inside a click and at least one has ended one.
            let in_click = |s: &BandScan| matches!(s, BandScan::InClick { .. });
            let idle = |s: &BandScan| matches!(s, BandScan::Idle);
            if in_click(&lo_scan) || in_click(&hi_scan) || (idle(&lo_scan) && idle(&hi_scan)) {
                continue;
            }

            let mut lo_found = click_span(lo_scan, &self.lo, peak_len, onset_factor, mask);
            let mut hi_found = click_span(hi_scan, &self.hi, peak_len, onset_factor, mask);

            // Wavelet: a click found in one band is looked for in the other too.
            if self.is_wavelet && lo_found.is_none() {
                lo_found = hi_found;
            }
            match lo_found {
                Some((left, right)) => self.refine_click(false, left, right),
                None => self.lo.gap = [scan_pos, scan_pos],
            }
            if self.is_wavelet && self.lo.gap[1] > self.lo.gap[0] && hi_found.is_none() {
                hi_found = Some((self.lo.gap[0], self.lo.gap[1] - 1));
            }
            match hi_found {
                Some((left, right)) => self.refine_click(true, left, right),
                None => self.hi.gap = [scan_pos, scan_pos],
            }

            if self.is_wavelet {
                self.align_band_gaps();
            }

            lo_scan = BandScan::Idle;
            hi_scan = BandScan::Idle;
            busy = false;

            let [start, end] = self.lo.gap;
            if start >= edge && end > start && ((end - start) as usize) <= MAX_REPAIR {
                self.apply_repair_rt(offset < SCAN_START + BLOCK);
            }
        }
    }

    /// Pins down the defect between `left` and `right` in one band and its repair, joined to any
    /// repair it touches.
    fn refine_click(&mut self, is_hi: bool, left: isize, right: isize) {
        let mask = CHANNEL_BUFFER_MASK;
        let is_wavelet = self.is_wavelet;
        let flags = &self.flags;
        let band = if is_hi { &mut self.hi } else { &mut self.lo };

        find_defect_edges_rt(
            &band.samples,
            mask,
            left,
            right + 1,
            self.derived.windows.peak_len as isize,
            self.derived.sensitivity.threshold,
            MAX_REPAIR,
            &mut band.gap,
            &mut band.fixed,
            &mut self.scratch,
        );
        if band.gap[1] <= band.gap[0] {
            return;
        }

        // Repaired samples next to the gap (in Wavelet mode, also one sample further out) join it.
        let repaired = |pos: isize| flags[(pos as usize) & mask] & flag::REPAIRED != 0;
        let mut merged = false;
        if repaired(band.gap[0] - 1) || (is_wavelet && repaired(band.gap[0] - 2)) {
            merged = true;
            if !repaired(band.gap[0] - 1) {
                band.gap[0] -= 1;
            }
            while repaired(band.gap[0] - 1) {
                band.gap[0] -= 1;
            }
        }
        if repaired(band.gap[1]) || (is_wavelet && repaired(band.gap[1] + 1)) {
            merged = true;
            if !repaired(band.gap[1]) {
                band.gap[1] += 1;
            }
            while repaired(band.gap[1]) {
                band.gap[1] += 1;
            }
        }

        if ((band.gap[1] - band.gap[0]) as usize) > MAX_REPAIR {
            band.gap = [left, left];
        } else if merged {
            fill_gap_bounded(
                &band.samples,
                mask,
                band.gap,
                MAX_REPAIR,
                MAX_PRED,
                true,
                &mut band.fixed,
                &mut self.scratch,
            );
        }
    }

    /// Wavelet mode: widens both bands' gaps to the one time span covering both, and refits a
    /// band whose gap changed.
    fn align_band_gaps(&mut self) {
        let (lo, hi) = time_span_to_band_span(band_spans_to_time_span(self.lo.gap, self.hi.gap, true), true);
        for (band, gap) in [(&mut self.lo, lo), (&mut self.hi, hi)] {
            let changed = band.gap != gap;
            band.gap = gap;
            if changed && ((gap[1] - gap[0]) as usize) <= MAX_REPAIR {
                fill_gap_bounded(
                    &band.samples,
                    CHANNEL_BUFFER_MASK,
                    gap,
                    MAX_REPAIR,
                    MAX_PRED,
                    true,
                    &mut band.fixed,
                    &mut self.scratch,
                );
            }
        }
    }

    /// Writes the repair over `lo.gap` (both bands in Wavelet mode) and refits the prediction
    /// errors there. `first_block`: the scan is still in its first block.
    fn apply_repair_rt(&mut self, first_block: bool) {
        let [start, end] = self.lo.gap;
        let len = ((end - start) as usize).min(MAX_REPAIR);

        if first_block && self.reach_back_to.map_or(true, |p| start < p) {
            self.reach_back_to = Some(start);
        }

        for i in 0..len {
            let idx = ((start + i as isize) as usize) & CHANNEL_BUFFER_MASK;
            self.lo.samples[idx] = self.lo.fixed[i];
            if self.is_wavelet {
                self.hi.samples[idx] = self.hi.fixed[i];
            }
            self.flags[idx] = (self.flags[idx] | flag::REPAIRED) & !flag::PROTECTED;
        }

        self.update_excitations(start, end);
        self.sums_valid = false;
    }

    /// Moves every position back by `n` output samples (the processor's counters wrap).
    pub fn shift_positions(&mut self, n: isize) {
        let band_n = if self.is_wavelet { n / 2 } else { n };
        self.read_pos -= n;
        if let Some(start) = self.export.start.as_mut() {
            *start -= n;
        }
        self.frame_count -= band_n;
        self.block_end -= band_n;
        if let Some(p) = self.reach_back_to.as_mut() {
            *p -= band_n;
        }
    }
}

fn update_band_excitation(band: &mut BandState, frame_count: isize, start: isize, end: isize, scratch: &mut ScratchSpace) {
    update_excitation_rt(
        &band.samples,
        frame_count,
        CHANNEL_BUFFER_LEN as isize,
        CHANNEL_BUFFER_MASK,
        start,
        end,
        &mut band.fwd_excit,
        &mut band.bck_excit,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl,
    );
}

/// Window sums at a scan position from scratch: forward errors over the pre-shoulder, squared
/// backward errors over the peak, backward errors over the post-shoulder.
fn fill_sums(band: &mut BandState, [pre_start, peak_start, peak_end, post_end]: [isize; 4], mask: usize) {
    band.pre_sum = (pre_start..peak_start).map(|p| band.fwd_excit[(p as usize) & mask]).sum();
    band.peak = (peak_start..peak_end).map(|p| band.bck_excit[(p as usize) & mask].powi(2)).sum();
    band.post_sum = (peak_end..post_end).map(|p| band.bck_excit[(p as usize) & mask]).sum();
}

/// Window sums after the windows moved back one sample: a sample enters each window at its
/// start and one leaves at its end.
fn slide_sums(band: &mut BandState, windows: [isize; 4], mask: usize) {
    let [pre_start, peak_start, peak_end, post_end] = windows.map(|p| (p as usize) & mask);
    band.pre_sum += band.fwd_excit[pre_start] - band.fwd_excit[peak_start];
    band.peak += band.bck_excit[peak_start].powi(2) - band.bck_excit[peak_end].powi(2);
    band.post_sum += band.bck_excit[peak_end] - band.bck_excit[post_end];
}

/// The peak window does not stand out from the post-shoulder.
fn quiet(band: &BandState, detect_factor: f64) -> bool {
    band.peak <= detect_factor * band.post_sum * band.post_sum
}

/// Advances one band's scan state at `scan_pos`. Returns whether the band is loud outside a
/// click (the scan must not stop here).
fn track_click(
    scan: &mut BandScan,
    band: &BandState,
    quiet: bool,
    scan_pos: isize,
    peak_len: isize,
    onset_factor: f64,
    mask: usize,
) -> bool {
    let mut busy = false;
    if !quiet && !matches!(scan, BandScan::InClick { .. }) {
        busy = true;
        if matches!(scan, BandScan::Idle) {
            // Trailing edge: back from the peak window's end to the first sample above the
            // onset level, at most twice the peak window.
            let threshold = onset_factor * band.post_sum;
            let mut right = scan_pos + peak_len;
            let stop = right - 2 * peak_len;
            while right > stop {
                if band.bck_excit[((right - 1) as usize) & mask] > threshold {
                    break;
                }
                right -= 1;
            }
            *scan = BandScan::InClick { right };
        }
    }
    if let BandScan::InClick { right } = *scan {
        if quiet || scan_pos < right - MAX_CLICK_SCAN {
            *scan = BandScan::Ended { left: scan_pos, right, pre_ref: band.pre_sum };
        }
    }
    busy
}

/// The span of a click that ended: its leading edge refined to the first sample above the onset
/// level, searched forward from one peak window before where it ended.
fn click_span(scan: BandScan, band: &BandState, peak_len: isize, onset_factor: f64, mask: usize) -> Option<(isize, isize)> {
    let BandScan::Ended { left, right, pre_ref } = scan else { return None };
    if right <= 0 {
        return None;
    }
    let threshold = onset_factor * pre_ref;
    let search_start = (right - 1).min(left) - peak_len;
    let search_end = (right - 1).min(search_start + 2 * peak_len);
    let mut left = search_start;
    while left < search_end {
        if band.fwd_excit[((left + 1) as usize) & mask] > threshold {
            break;
        }
        left += 1;
    }
    Some((left, right))
}
