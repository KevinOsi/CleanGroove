//! Multi-Rate Decimated SubChannel State Machine
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 8.

use crate::burg::update_excitation_floored;
use crate::flag;
use crate::detector::{find_defect_edges_subchannel, BandState};
use crate::params::DerivedSubChannelParams;
use crate::reconstruct::{fill_gap_subchannel, lowpass_step, ScratchSpace};
use crate::wavelet::{time_span_to_band_span, band_spans_to_time_span};

/// Decimated multi-rate SubChannel processor (decimated by 4x per depth level).
#[derive(Debug, Clone)]
pub struct SubChannel {
    pub depth: usize,
    pub child: Option<Box<SubChannel>>,
    pub buffer_length: usize,
    pub buffer_mask: usize,
    pub pipeline_delay: usize,
    pub lo: BandState,
    pub hi: BandState,
    pub flags: Vec<u8>,
    pub params: DerivedSubChannelParams,
    pub scratch: ScratchSpace,
    /// Position of the newest sample (-1 before the first).
    pub frame_count: isize,
    // How far each stage runs behind the samples taken in, as in the main channel.
    pub scan_lag: isize,
    pub excitation_lag: isize,
    pub curr_offset: isize,
    pub min_offset: isize,
    pub max_offset: isize,
    pub max_auto_repair: usize,
    pub is_reverse: bool,
    pub is_wavelet: bool,

    // Detection state machine fields
    pub sums_valid: bool,
    pub last_repair: isize,
    pub loop_total: isize,
    pub number_of_frames: isize,
    pub loop_limit: isize,
    pub detect_factor: f64,
    pub onset_factor: f64,

    // Telemetry counters
    pub subchannel_hint_events: usize,
}

impl SubChannel {
    pub fn new(
        depth: usize,
        max_depth: usize,
        dcl: i32,
        is_reverse: bool,
        is_wavelet: bool,
        parent_frames: isize,
    ) -> Self {
        let remaining_subchannels = max_depth.saturating_sub(depth);
        let params = DerivedSubChannelParams::compute(dcl, is_reverse, remaining_subchannels);

        let max_auto_repair = 64 >> (2 * depth.min(2));
        let min_offset: isize = 256;
        let max_offset: isize = min_offset + 4 * (max_auto_repair as isize) + 128;
        let pipeline_delay = (max_offset + 2 * (max_auto_repair as isize) + 256) as usize;

        let mut buffer_length = 2048;
        while buffer_length <= pipeline_delay + 2 * max_auto_repair {
            buffer_length <<= 1;
        }
        let buffer_mask = buffer_length - 1;

        let sub_frames = if parent_frames > 0 && parent_frames < isize::MAX / 4 {
            parent_frames / 4
        } else {
            isize::MAX / 2
        };

        let child = if depth < max_depth {
            Some(Box::new(SubChannel::new(
                depth + 1,
                max_depth,
                dcl,
                is_reverse,
                is_wavelet,
                sub_frames,
            )))
        } else {
            None
        };

        let loop_limit = sub_frames - min_offset;
        let loop_total = sub_frames + 100_000;

        Self {
            depth,
            child,
            buffer_length,
            buffer_mask,
            pipeline_delay,
            lo: BandState::new(buffer_length, 0),
            hi: BandState::new(buffer_length, 0),
            flags: vec![0; buffer_length],
            detect_factor: params.normal.detect_factor,
            onset_factor: params.normal.onset_factor,
            params,
            scratch: ScratchSpace::new(),
            frame_count: -1,
            scan_lag: pipeline_delay as isize - min_offset - 64,
            excitation_lag: 320,
            curr_offset: -64 - 1,
            min_offset,
            max_offset,
            max_auto_repair,
            is_reverse,
            is_wavelet,
            sums_valid: false,
            last_repair: loop_total,
            loop_total,
            number_of_frames: sub_frames,
            loop_limit,
            subchannel_hint_events: 0,
        }
    }

    /// Recursively counts all subchannel hint events across all decimation depths.
    pub fn total_subchannel_hints(&self) -> usize {
        self.subchannel_hint_events
            + self.child.as_ref().map(|c| c.total_subchannel_hints()).unwrap_or(0)
    }

    pub fn pipeline_delay(&self) -> usize {
        self.pipeline_delay
    }

    /// Processes one pair of decimated subband samples (lo_in, hi_in) and propagates the hint flag to parent flags.
    pub fn take_pair(
        &mut self,
        lo_in: i32,
        hi_in: i32,
        parent_flags: &mut [u8],
        parent_mask: usize,
    ) {
        self.frame_count += 1;
        let excitation_pos = self.frame_count + 1 - self.excitation_lag;
        let scan_trigger = self.frame_count + 1 - self.scan_lag;
        self.curr_offset = (self.curr_offset + 1).min(self.max_offset);

        let idx = (self.frame_count as usize) & self.buffer_mask;
        self.lo.samples[idx] = lo_in;
        self.lo.original[idx] = lo_in;
        self.hi.samples[idx] = hi_in;
        self.hi.original[idx] = hi_in;
        self.flags[idx] = 0;

        // 2nd-order Butterworth filtering
        lowpass_step(idx, self.buffer_mask, &self.lo.samples, &mut self.lo.filtered);
        if self.is_wavelet {
            lowpass_step(idx, self.buffer_mask, &self.hi.samples, &mut self.hi.filtered);
        }

        // Decimation cascade to deeper child subchannel every 4 frames
        if let Some(ref mut child) = self.child {
            if (self.frame_count & 3) == 0 {
                let mask = self.buffer_mask;
                let avg_lo = (self.lo.original[idx]
                    + self.lo.original[idx.wrapping_sub(1) & mask]
                    + self.lo.original[idx.wrapping_sub(2) & mask]
                    + self.lo.original[idx.wrapping_sub(3) & mask])
                    / 4;
                let avg_hi = if self.is_wavelet {
                    (self.hi.original[idx]
                        + self.hi.original[idx.wrapping_sub(1) & mask]
                        + self.hi.original[idx.wrapping_sub(2) & mask]
                        + self.hi.original[idx.wrapping_sub(3) & mask])
                        / 4
                } else {
                    0
                };
                child.take_pair(avg_lo, avg_hi, &mut self.flags, self.buffer_mask);
            }
        }

        // Block-256 excitation generation
        if (excitation_pos & 0xFF) == 0 && excitation_pos >= 0 {
            update_excitation_floored(
                &self.lo.samples,
                self.frame_count,
                self.buffer_length as isize,
                self.buffer_mask,
                excitation_pos - 256,
                excitation_pos,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
            if self.is_wavelet {
                update_excitation_floored(
                    &self.hi.samples,
                    self.frame_count,
                    self.buffer_length as isize,
                    self.buffer_mask,
                    excitation_pos - 256,
                    excitation_pos,
                    &mut self.hi.fwd_excit,
                    &mut self.hi.bck_excit,
                    &mut self.scratch.fwd_err,
                    &mut self.scratch.bck_err,
                    &mut self.scratch.refl,
                );
            }
        }

        // Sliding-window declick scan every 128 frames
        if (scan_trigger & 0x7F) == 0 && scan_trigger >= 0 && scan_trigger < self.loop_limit {
            if self.is_reverse {
                self.scan_backward(parent_flags, parent_mask);
            } else {
                self.scan_forward(parent_flags, parent_mask);
            }
        }
    }

    /// Forward sliding-window SubChannel declicker (engine doc 8)
    pub fn scan_forward(&mut self, parent_flags: &mut [u8], parent_mask: usize) {
        let mask = self.buffer_mask;
        let mut lo_onset: isize = 0;
        let mut lo_end: isize = 0;
        let mut hi_onset: isize = 0;
        let mut hi_end: isize = 0;
        let mut lo_post_ref: f64 = 0.0;
        let mut hi_post_ref: f64 = 0.0;
        let mut lo_in_click = false;
        let mut hi_in_click = false;
        let mut idle = true;

        while self.curr_offset > self.min_offset || !idle {
            let mut pos = self.frame_count - self.curr_offset;
            if self.last_repair < self.loop_total {
                let rescan = pos - self.last_repair + 64;
                self.curr_offset = (self.curr_offset + rescan).min(self.max_offset);
                self.last_repair = self.loop_total;
                self.sums_valid = false;
            }
            pos = self.frame_count - self.curr_offset;

            let peak_end = pos + (self.params.windows.peak_len as isize) / 2;
            let post_end = peak_end + (self.params.windows.shoulder_len as isize);
            let peak_start = peak_end - (self.params.windows.peak_len as isize);
            let pre_start = peak_start - (self.params.windows.shoulder_len as isize);

            // O(1) sliding window shoulder sums
            if self.sums_valid {
                let idx_post_end = (post_end as usize) & mask;
                let idx_peak_end = (peak_end as usize) & mask;
                let idx_peak_start = (peak_start as usize) & mask;
                let idx_pre_start = (pre_start as usize) & mask;

                self.lo.pre_sum += self.lo.fwd_excit[idx_peak_start] - self.lo.fwd_excit[idx_pre_start];
                self.lo.post_sum += self.lo.bck_excit[idx_post_end] - self.lo.bck_excit[idx_peak_end];

                if self.is_wavelet {
                    self.hi.pre_sum += self.hi.fwd_excit[idx_peak_start] - self.hi.fwd_excit[idx_pre_start];
                    self.hi.post_sum += self.hi.bck_excit[idx_post_end] - self.hi.bck_excit[idx_peak_end];
                }
            } else {
                self.lo.pre_sum = 0.0;
                self.lo.post_sum = 0.0;
                let mut p_pre = pre_start;
                let mut p_post = peak_end;
                for _ in 0..self.params.windows.shoulder_len {
                    p_pre += 1;
                    p_post += 1;
                    self.lo.pre_sum += self.lo.fwd_excit[(p_pre as usize) & mask];
                    self.lo.post_sum += self.lo.bck_excit[(p_post as usize) & mask];
                }
                if self.is_wavelet {
                    self.hi.pre_sum = 0.0;
                    self.hi.post_sum = 0.0;
                    let mut hp_pre = pre_start;
                    let mut hp_post = peak_end;
                    for _ in 0..self.params.windows.shoulder_len {
                        hp_pre += 1;
                        hp_post += 1;
                        self.hi.pre_sum += self.hi.fwd_excit[(hp_pre as usize) & mask];
                        self.hi.post_sum += self.hi.bck_excit[(hp_post as usize) & mask];
                    }
                }
                self.sums_valid = true;
            }

            // Lower threshold switch when flagged as a hint
            if idle {
                if (self.flags[(pos as usize) & mask] & flag::HINT) != 0 {
                    self.onset_factor = self.params.lower.onset_factor;
                } else {
                    self.onset_factor = self.params.normal.onset_factor;
                }
            }

            let idx = (pos as usize) & mask;
            let lo_quiet = self.lo.fwd_excit[idx] <= self.onset_factor * self.lo.pre_sum;
            let hi_quiet = !self.is_wavelet || (self.hi.fwd_excit[idx] <= self.onset_factor * self.hi.pre_sum);

            // Onset Search
            if !lo_quiet && !lo_in_click && lo_end == 0 {
                lo_in_click = true;
                idle = false;
                let search_start = pos - (self.params.windows.peak_len as isize);
                let search_end = pos + (self.params.windows.peak_len as isize);
                lo_onset = search_start;
                while lo_onset < search_end {
                    if self.lo.fwd_excit[(lo_onset as usize) & mask] > self.onset_factor * self.lo.pre_sum {
                        break;
                    }
                    lo_onset += 1;
                }
            }

            if self.is_wavelet && !hi_quiet && !hi_in_click && hi_end == 0 {
                hi_in_click = true;
                idle = false;
                let search_start = pos - (self.params.windows.peak_len as isize);
                let search_end = pos + (self.params.windows.peak_len as isize);
                hi_onset = search_start;
                while hi_onset < search_end {
                    if self.hi.fwd_excit[(hi_onset as usize) & mask] > self.onset_factor * self.hi.pre_sum {
                        break;
                    }
                    hi_onset += 1;
                }
            }

            // Click termination with 64-subframe maximum timeout
            if lo_in_click && (lo_quiet || pos > lo_onset + 64) {
                lo_in_click = false;
                lo_post_ref = self.lo.post_sum;
                lo_end = pos;
            }
            if hi_in_click && (hi_quiet || pos > hi_onset + 64) {
                hi_in_click = false;
                hi_post_ref = self.hi.post_sum;
                hi_end = pos;
            }

            // When click has ended, refine edge and analyze
            if !idle && !lo_in_click && !hi_in_click {
                idle = true;
                self.lo.gap[0] = 0;
                self.lo.gap[1] = 0;
                self.hi.gap[0] = 0;
                self.hi.gap[1] = 0;

                let hint = (self.flags[(pos as usize) & mask] & flag::HINT) != 0;
                let th = if hint {
                    self.params.lower.threshold
                } else {
                    self.params.normal.threshold
                };

                if lo_end > 0 {
                    let search_hi = lo_onset.max(lo_end - 1) + 1 + (self.params.windows.peak_len as isize);
                    let search_lo = (lo_onset + 1).max(search_hi - 2 * (self.params.windows.peak_len as isize));
                    lo_end = search_hi;
                    while lo_end > search_lo {
                        if self.lo.bck_excit[(lo_end as usize) & mask] > self.onset_factor * lo_post_ref {
                            break;
                        }
                        lo_end -= 1;
                    }
                    find_defect_edges_subchannel(
                        &self.lo.samples,
                        mask,
                        lo_onset,
                        lo_end,
                        self.params.windows.peak_len as isize,
                        th,
                        self.is_reverse,
                        self.max_auto_repair,
                        &mut self.lo.gap,
                        &mut self.lo.fixed,
                        &mut self.scratch,
                    );
                    lo_end = 0;
                }

                if self.is_wavelet && hi_end > 0 {
                    let search_hi = hi_onset.max(hi_end - 1) + 1 + (self.params.windows.peak_len as isize);
                    let search_lo = (hi_onset + 1).max(search_hi - 2 * (self.params.windows.peak_len as isize));
                    hi_end = search_hi;
                    while hi_end > search_lo {
                        if self.hi.bck_excit[(hi_end as usize) & mask] > self.onset_factor * hi_post_ref {
                            break;
                        }
                        hi_end -= 1;
                    }
                    find_defect_edges_subchannel(
                        &self.hi.samples,
                        mask,
                        hi_onset,
                        hi_end,
                        self.params.windows.peak_len as isize,
                        th,
                        self.is_reverse,
                        self.max_auto_repair,
                        &mut self.hi.gap,
                        &mut self.hi.fixed,
                        &mut self.scratch,
                    );
                    hi_end = 0;
                }

                // Synchronize Wavelet intervals
                if self.is_wavelet {
                    let prev_lo_len = self.lo.gap[1] - self.lo.gap[0];
                    let prev_hi_len = self.hi.gap[1] - self.hi.gap[0];
                    let unified = band_spans_to_time_span(self.lo.gap, self.hi.gap, true);
                    let (sync_lo, sync_hi) = time_span_to_band_span(unified, true);
                    self.lo.gap = sync_lo;
                    self.hi.gap = sync_hi;
                    if self.lo.gap[1] - self.lo.gap[0] != prev_lo_len && self.lo.gap[1] > self.lo.gap[0] {
                        fill_gap_subchannel(
                            &self.lo.samples,
                            mask,
                            self.lo.gap,
                            &mut self.lo.fixed,
                            &mut self.scratch,
                        );
                    }
                    if self.hi.gap[1] - self.hi.gap[0] != prev_hi_len && self.hi.gap[1] > self.hi.gap[0] {
                        fill_gap_subchannel(
                            &self.hi.samples,
                            mask,
                            self.hi.gap,
                            &mut self.hi.fixed,
                            &mut self.scratch,
                        );
                    }
                }

                if self.lo.gap[1] > self.lo.gap[0]
                    && self.lo.gap[0] > 64
                    && self.lo.gap[1] < self.number_of_frames - 64
                    && ((self.lo.gap[1] - self.lo.gap[0]) as usize) <= self.max_auto_repair
                {
                    let mut any_new = false;
                    for p in self.lo.gap[0]..self.lo.gap[1] {
                        if (self.flags[(p as usize) & mask] & flag::SUB_REPAIRED) == 0 {
                            any_new = true;
                            break;
                        }
                    }
                    if any_new {
                        self.apply_repair(parent_flags, parent_mask);
                    }
                }
            }

            self.curr_offset -= 1;
            if self.curr_offset == 0 {
                break;
            }
        }
    }

    /// Backward sliding-window SubChannel declicker (engine doc 8)
    pub fn scan_backward(&mut self, parent_flags: &mut [u8], parent_mask: usize) {
        let mask = self.buffer_mask;
        let mut lo_onset: isize = 0;
        let mut lo_end: isize = 0;
        let mut hi_onset: isize = 0;
        let mut hi_end: isize = 0;
        let mut lo_pre_ref: f64 = 0.0;
        let mut hi_pre_ref: f64 = 0.0;
        let mut lo_in_click = false;
        let mut hi_in_click = false;
        let mut idle = true;

        self.curr_offset = self.min_offset;
        let mut max_scan_offset = (self.min_offset + 384).min(self.max_offset);
        self.last_repair = self.loop_total;
        self.sums_valid = false;

        while self.curr_offset < max_scan_offset || !idle {
            let pos = self.frame_count - self.curr_offset;
            let peak_end = pos + (self.params.windows.peak_len as isize) / 2;
            let post_end = peak_end + (self.params.windows.shoulder_len as isize);
            let peak_start = peak_end - (self.params.windows.peak_len as isize);
            let pre_start = peak_start - (self.params.windows.shoulder_len as isize);

            // Windows are (pre_start, peak_start] (pre, fwd_excit) and (peak_end, post_end] (post, bck_excit).
            // Scanning backward, each step ADDS the sample just below the old low edge
            // (new pre_start + 1 / new peak_end + 1) and REMOVES the old high edge (new peak_start + 1 / new post_end + 1).
            // (Audit fix: previously used the forward-scan update while moving backward,
            // corrupting the cached sums on nearly every reverse-mode step.)
            if self.sums_valid {
                let add_pre = ((pre_start + 1) as usize) & mask;
                let rem_pre = ((peak_start + 1) as usize) & mask;
                let add_post = ((peak_end + 1) as usize) & mask;
                let rem_post = ((post_end + 1) as usize) & mask;

                self.lo.pre_sum += self.lo.fwd_excit[add_pre] - self.lo.fwd_excit[rem_pre];
                self.lo.post_sum += self.lo.bck_excit[add_post] - self.lo.bck_excit[rem_post];

                if self.is_wavelet {
                    self.hi.pre_sum += self.hi.fwd_excit[add_pre] - self.hi.fwd_excit[rem_pre];
                    self.hi.post_sum += self.hi.bck_excit[add_post] - self.hi.bck_excit[rem_post];
                }
            } else {
                self.lo.pre_sum = 0.0;
                self.lo.post_sum = 0.0;
                let mut p_pre = pre_start;
                let mut p_post = peak_end;
                for _ in 0..self.params.windows.shoulder_len {
                    p_pre += 1;
                    p_post += 1;
                    self.lo.pre_sum += self.lo.fwd_excit[(p_pre as usize) & mask];
                    self.lo.post_sum += self.lo.bck_excit[(p_post as usize) & mask];
                }
                if self.is_wavelet {
                    self.hi.pre_sum = 0.0;
                    self.hi.post_sum = 0.0;
                    let mut hp_pre = pre_start;
                    let mut hp_post = peak_end;
                    for _ in 0..self.params.windows.shoulder_len {
                        hp_pre += 1;
                        hp_post += 1;
                        self.hi.pre_sum += self.hi.fwd_excit[(hp_pre as usize) & mask];
                        self.hi.post_sum += self.hi.bck_excit[(hp_post as usize) & mask];
                    }
                }
                self.sums_valid = true;
            }

            if idle {
                if (self.flags[(pos as usize) & mask] & flag::HINT) != 0 {
                    self.onset_factor = self.params.lower.onset_factor;
                } else {
                    self.onset_factor = self.params.normal.onset_factor;
                }
            }

            let idx = (pos as usize) & mask;
            let lo_quiet = self.lo.bck_excit[idx] <= self.onset_factor * self.lo.post_sum;
            let hi_quiet = !self.is_wavelet || (self.hi.bck_excit[idx] <= self.onset_factor * self.hi.post_sum);

            if !lo_quiet && !lo_in_click && lo_onset == 0 {
                lo_in_click = true;
                idle = false;
                let search_start = pos + (self.params.windows.peak_len as isize);
                let search_end = search_start - 2 * (self.params.windows.peak_len as isize);
                lo_end = search_start;
                while lo_end > search_end {
                    if self.lo.bck_excit[(lo_end as usize) & mask] > self.onset_factor * self.lo.post_sum {
                        break;
                    }
                    lo_end -= 1;
                }
            }

            if self.is_wavelet && !hi_quiet && !hi_in_click && hi_onset == 0 {
                hi_in_click = true;
                idle = false;
                let search_start = pos + (self.params.windows.peak_len as isize);
                let search_end = search_start - 2 * (self.params.windows.peak_len as isize);
                hi_end = search_start;
                while hi_end > search_end {
                    if self.hi.bck_excit[(hi_end as usize) & mask] > self.onset_factor * self.hi.post_sum {
                        break;
                    }
                    hi_end -= 1;
                }
            }

            if lo_in_click && (lo_quiet || pos < lo_end - 64) {
                lo_in_click = false;
                lo_pre_ref = self.lo.pre_sum;
                lo_onset = pos;
            }
            if hi_in_click && (hi_quiet || pos < hi_end - 64) {
                hi_in_click = false;
                hi_pre_ref = self.hi.pre_sum;
                hi_onset = pos;
            }

            if !idle && !lo_in_click && !hi_in_click {
                idle = true;
                self.lo.gap[0] = 0;
                self.lo.gap[1] = 0;
                self.hi.gap[0] = 0;
                self.hi.gap[1] = 0;

                let hint = (self.flags[(pos as usize) & mask] & flag::HINT) != 0;
                let th = if hint {
                    self.params.lower.threshold
                } else {
                    self.params.normal.threshold
                };

                if lo_onset > 0 {
                    let search_lo = lo_onset.min(lo_end + 1) - 1 - (self.params.windows.peak_len as isize);
                    let search_hi = (lo_end - 1).min(search_lo + 2 * (self.params.windows.peak_len as isize));
                    lo_onset = search_lo;
                    while lo_onset < search_hi {
                        if self.lo.fwd_excit[(lo_onset as usize) & mask] > self.onset_factor * lo_pre_ref {
                            break;
                        }
                        lo_onset += 1;
                    }
                    find_defect_edges_subchannel(
                        &self.lo.samples,
                        mask,
                        lo_onset,
                        lo_end,
                        self.params.windows.peak_len as isize,
                        th,
                        self.is_reverse,
                        self.max_auto_repair,
                        &mut self.lo.gap,
                        &mut self.lo.fixed,
                        &mut self.scratch,
                    );
                    lo_onset = 0;
                }

                if self.is_wavelet && hi_onset > 0 {
                    let search_lo = hi_onset.min(hi_end + 1) - 1 - (self.params.windows.peak_len as isize);
                    let search_hi = (hi_end - 1).min(search_lo + 2 * (self.params.windows.peak_len as isize));
                    hi_onset = search_lo;
                    while hi_onset < search_hi {
                        if self.hi.fwd_excit[(hi_onset as usize) & mask] > self.onset_factor * hi_pre_ref {
                            break;
                        }
                        hi_onset += 1;
                    }
                    find_defect_edges_subchannel(
                        &self.hi.samples,
                        mask,
                        hi_onset,
                        hi_end,
                        self.params.windows.peak_len as isize,
                        th,
                        self.is_reverse,
                        self.max_auto_repair,
                        &mut self.hi.gap,
                        &mut self.hi.fixed,
                        &mut self.scratch,
                    );
                    hi_onset = 0;
                }

                if self.is_wavelet {
                    let prev_lo_len = self.lo.gap[1] - self.lo.gap[0];
                    let prev_hi_len = self.hi.gap[1] - self.hi.gap[0];
                    let unified = band_spans_to_time_span(self.lo.gap, self.hi.gap, true);
                    let (sync_lo, sync_hi) = time_span_to_band_span(unified, true);
                    self.lo.gap = sync_lo;
                    self.hi.gap = sync_hi;
                    if self.lo.gap[1] - self.lo.gap[0] != prev_lo_len && self.lo.gap[1] > self.lo.gap[0] {
                        fill_gap_subchannel(
                            &self.lo.samples,
                            mask,
                            self.lo.gap,
                            &mut self.lo.fixed,
                            &mut self.scratch,
                        );
                    }
                    if self.hi.gap[1] - self.hi.gap[0] != prev_hi_len && self.hi.gap[1] > self.hi.gap[0] {
                        fill_gap_subchannel(
                            &self.hi.samples,
                            mask,
                            self.hi.gap,
                            &mut self.hi.fixed,
                            &mut self.scratch,
                        );
                    }
                }

                if self.lo.gap[1] > self.lo.gap[0]
                    && self.lo.gap[0] > 64
                    && self.lo.gap[1] < self.number_of_frames - 64
                    && ((self.lo.gap[1] - self.lo.gap[0]) as usize) <= self.max_auto_repair
                {
                    let mut any_new = false;
                    for p in self.lo.gap[0]..self.lo.gap[1] {
                        if (self.flags[(p as usize) & mask] & flag::SUB_REPAIRED) == 0 {
                            any_new = true;
                            break;
                        }
                    }
                    if any_new {
                        self.apply_repair(parent_flags, parent_mask);
                        max_scan_offset = (self.curr_offset + 64).min(self.max_offset);
                        self.sums_valid = false;
                    }
                }
            }

            self.curr_offset += 1;
            if self.curr_offset >= self.max_offset {
                break;
            }
        }
        self.curr_offset = self.min_offset;
    }

    /// Commits repair to SubChannel buffer and executes the Nyquist Gate before setting the parent's hint flag.
    pub fn apply_repair(&mut self, parent_flags: &mut [u8], parent_mask: usize) {
        let mask = self.buffer_mask;
        let gap_len = (self.lo.gap[1] - self.lo.gap[0]) as usize;

        // 1. Commit fixed samples to subchannel buffers
        for k in 0..gap_len {
            let ring_k = ((self.lo.gap[0] + (k as isize)) as usize) & mask;
            self.lo.samples[ring_k] = self.lo.fixed[k];
            if self.is_wavelet {
                self.hi.samples[ring_k] = self.hi.fixed[k];
            }
            self.flags[ring_k] |= flag::SUB_REPAIRED;
        }

        self.last_repair = self.lo.gap[0];
        self.sums_valid = false;

        // 2. Regenerate subchannel excitation
        update_excitation_floored(
            &self.lo.samples,
            self.frame_count,
            self.buffer_length as isize,
            mask,
            self.lo.gap[0],
            self.lo.gap[1],
            &mut self.lo.fwd_excit,
            &mut self.lo.bck_excit,
            &mut self.scratch.fwd_err,
            &mut self.scratch.bck_err,
            &mut self.scratch.refl,
        );

        if self.is_wavelet {
            update_excitation_floored(
                &self.hi.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.hi.gap[0],
                self.hi.gap[1],
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
        }

        // 3. Low-frequency gate (engine doc 8)
        let center = (self.lo.gap[0] + self.lo.gap[1]) / 2;
        let start_64 = center - 32;

        let mut sum_lo = 0.0;
        for k in 0..64 {
            let rk = ((start_64 + k) as usize) & mask;
            sum_lo += self.lo.samples[rk] as f64;
        }
        let mean_lo = sum_lo / 64.0;

        let mut signal_var = 0.0; // 64-sample local signal variance
        let mut filter_var = 0.0; // 64-sample mean-squared difference between lo.samples and lo.filtered
        for k in 0..64 {
            let rk = ((start_64 + k) as usize) & mask;
            let diff_mean = (self.lo.samples[rk] as f64) - mean_lo;
            signal_var += diff_mean * diff_mean;
            let diff_filt = (self.lo.samples[rk] - self.lo.filtered[rk]) as f64;
            filter_var += diff_filt * diff_filt;
        }
        signal_var /= 64.0;
        filter_var = (filter_var / 64.0).max(1e-12);

        // Subband signal is genuinely low-frequency (signal_var > 10.0 * filter_var) and not upper-band aliasing:
        if signal_var > 10.0 * filter_var {
            let min_diff = self.params.lower.threshold * filter_var.sqrt();
            let mut significant = false;
            for k in 0..gap_len {
                let rk = ((self.lo.gap[0] + (k as isize)) as usize) & mask;
                if ((self.lo.samples[rk] - self.lo.original[rk]) as f64).abs() > min_diff {
                    significant = true;
                    break;
                }
            }

            if significant {
                self.subchannel_hint_events += 1;
                let parent_start = (4 * self.lo.gap[0] - 8).max(0);
                let parent_end = 4 * self.lo.gap[1];
                for p in parent_start..parent_end {
                    let p_idx = (p as usize) & parent_mask;
                    parent_flags[p_idx] |= flag::HINT;
                }
            }
        }
    }
}
