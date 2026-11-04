//! Multi-Rate Dyadic Processor & Stereo Merge Unmixing
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 3.

use std::collections::VecDeque;

use crate::channel::Channel;
use crate::params::{ChannelMode, CrParameters};
use crate::reconstruct::{fill_gap, ScratchSpace};
use crate::wavelet::{synthesize_positive_up, D4Analyzer};

/// Telemetry stats from a processing run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessorStats {
    pub declick_events: usize,
    pub declick_samples: usize,
    pub protected_samples: usize,
    pub subchannel_hint_events: usize,
    /// Defects left unrepaired because they are longer than the automatic repair limit (left
    /// alone: they would need a manual repair), counted on each
    /// channel's main band, and their total length in output samples.
    pub cancelled_long_repairs: usize,
    pub cancelled_long_samples: usize,
    /// Enhanced only: repairs made by the sustained-defect (long) path, their total length in
    /// output samples, and long candidates the music gate refused.
    pub long_repairs: usize,
    pub long_repair_samples: usize,
    pub long_gated: usize,
}

/// Master audio restoration processor handling multi-rate decimation, stereo channels, and mono LP merge mode.
#[derive(Debug, Clone)]
pub struct Processor {
    pub params: CrParameters,
    pub raw_sample_rate: u32,
    pub stage1_resample: bool,    // > 96 kHz Haar wavelet split
    pub stage2_decimated: bool,   // > 48 kHz Daubechies D4 wavelet split
    pub decimation_factor: usize,
    pub left_channel: Channel,
    pub right_channel: Option<Channel>,
    pub merge_mode: bool,
    pub channel_sample_latency: usize,
    pub total_pipeline_delay: usize,

    // Alignment and trailing extra samples (engine doc 3)
    pub total_frames: Option<usize>,
    pub aligned_frames: usize,
    pub extra_samples: usize,
    pub trailing_samples: Vec<(i32, i32)>,
    pub total_in_count: usize,
    pub total_out_count: usize,

    // Stereo merge 4000-sample sliding window balance tracker centered by 2000 samples
    pub level_left: i64,
    pub level_right: i64,
    pub balance_ring: Vec<i32>,
    pub orig_left_ring: Vec<i32>,
    pub orig_right_ring: Vec<i32>,
    pub balance_mask: usize,
    pub frame_in_count: isize,

    // Stage 1 (>96 kHz) Haar decimation state & ring buffers
    pub stage1_buffer: Vec<(i32, i32)>,
    pub stage1_hi_ring: Vec<(i32, i32)>,
    pub stage1_repaired_ring: Vec<(bool, bool)>,
    pub stage1_hi_mask: usize,
    pub stage1_frame_in: usize,
    pub stage1_frame_out: usize,

    // Stage 2 (>48 kHz) Daubechies D4 decimation state & ring buffers
    pub d4_stage2_left: D4Analyzer,
    pub d4_stage2_right: D4Analyzer,
    pub stage2_buffer: Vec<(i32, i32)>,
    pub stage2_hi_left_ring: Vec<i32>,
    pub stage2_hi_right_ring: Vec<i32>,
    pub stage2_lo_left_ring: Vec<i32>,
    pub stage2_lo_right_ring: Vec<i32>,
    pub stage2_hi_mask: usize,
    pub stage2_frame_in: usize,
    pub stage2_channel_out_count: usize,
    pub stage2_frame_out: usize,
    pub stage2_last_repaired_end_l: isize,
    pub stage2_last_repaired_end_r: isize,
    pub stage2_fixed: Vec<i32>,
    pub stage2_scratch: ScratchSpace,

    // Output queue for synthesized frames
    pub output_queue: VecDeque<(i32, i32)>,
}

impl Processor {
    pub fn new(params: CrParameters, sample_rate: u32, total_frames: Option<usize>) -> Self {
        let stage1_resample = sample_rate > 96_000;
        let rate_stage1 = if stage1_resample { sample_rate / 2 } else { sample_rate };
        let stage2_decimated = rate_stage1 > 48_000;
        let decimation_factor = (if stage1_resample { 2 } else { 1 }) * (if stage2_decimated { 2 } else { 1 });

        let merge_mode = params.mode == ChannelMode::Merge;
        let is_mono = params.mode == ChannelMode::Mono;

        // Dyadic alignment (engine doc 3)
        let divisor: usize = (1 << params.method.dyadic_level()) * decimation_factor;
        let (aligned_frames, extra_samples) = if let Some(tf) = total_frames {
            let rem = tf % divisor;
            (tf - rem, rem)
        } else {
            (0, 0)
        };

        let wavelet_factor = if params.method.is_wavelet() { 2 } else { 1 };
        let total_decimation = decimation_factor * wavelet_factor;
        let channel_frames = if aligned_frames > 0 {
            (aligned_frames / total_decimation) as isize
        } else {
            -1
        };

        let left_channel = Channel::new(params.clone(), channel_frames);
        let channel_pipeline_delay = left_channel.pipeline_delay;

        let right_channel = if !merge_mode && !is_mono {
            Some(Channel::new(params.clone(), channel_frames))
        } else {
            None
        };

        let channel_sample_latency = if params.method.is_wavelet() {
            2 * channel_pipeline_delay
        } else {
            channel_pipeline_delay
        };

        let total_pipeline_delay = if merge_mode {
            2000 + channel_sample_latency
        } else {
            channel_sample_latency * decimation_factor
        };

        let balance_buf_size = 65536;
        let balance_mask = balance_buf_size - 1;

        let stage1_hi_size = 65536;
        let stage1_hi_mask = stage1_hi_size - 1;

        let stage2_hi_size = 65536;
        let stage2_hi_mask = stage2_hi_size - 1;

        Self {
            params,
            raw_sample_rate: sample_rate,
            stage1_resample,
            stage2_decimated,
            decimation_factor,
            left_channel,
            right_channel,
            merge_mode,
            channel_sample_latency,
            total_pipeline_delay,
            total_frames,
            aligned_frames,
            extra_samples,
            trailing_samples: Vec::new(),
            total_in_count: 0,
            total_out_count: 0,
            level_left: 800_000_000,
            level_right: 800_000_000,
            balance_ring: vec![0x400000; balance_buf_size],
            orig_left_ring: vec![0; balance_buf_size],
            orig_right_ring: vec![0; balance_buf_size],
            balance_mask,
            frame_in_count: -1,
            stage1_buffer: Vec::with_capacity(2),
            stage1_hi_ring: vec![(0, 0); stage1_hi_size],
            stage1_repaired_ring: vec![(false, false); stage1_hi_size],
            stage1_hi_mask,
            stage1_frame_in: 0,
            stage1_frame_out: 0,
            d4_stage2_left: D4Analyzer::new(),
            d4_stage2_right: D4Analyzer::new(),
            stage2_buffer: Vec::with_capacity(2),
            stage2_hi_left_ring: vec![0; stage2_hi_size],
            stage2_hi_right_ring: vec![0; stage2_hi_size],
            stage2_lo_left_ring: vec![0; stage2_hi_size],
            stage2_lo_right_ring: vec![0; stage2_hi_size],
            stage2_hi_mask,
            stage2_frame_in: 0,
            stage2_channel_out_count: 0,
            stage2_frame_out: 0,
            stage2_last_repaired_end_l: -1,
            stage2_last_repaired_end_r: -1,
            stage2_fixed: vec![0; 2048],
            stage2_scratch: ScratchSpace::new(),
            output_queue: VecDeque::new(),
        }
    }

    /// Processes one incoming stereo frame (left_in, right_in) and returns one restored frame if available.
    pub fn process_frame(&mut self, left_in: i32, right_in: i32) -> Option<(i32, i32)> {
        self.total_in_count += 1;

        // If extra unaligned samples are detected at EOF, store them to copy unmodified
        if let Some(tf) = self.total_frames {
            if self.total_in_count > self.aligned_frames && self.total_in_count <= tf {
                self.trailing_samples.push((left_in, right_in));
                return self.output_queue.pop_front();
            }
        }

        if self.stage1_resample {
            // Stage 1 (>96 kHz) Haar split: pairs of raw frames
            self.stage1_buffer.push((left_in, right_in));
            if self.stage1_buffer.len() == 2 {
                let f0 = self.stage1_buffer[0];
                let f1 = self.stage1_buffer[1];
                self.stage1_buffer.clear();

                let l_lo = (f0.0 + f1.0) >> 1;
                let l_hi = (f0.0 - f1.0) >> 1;
                let r_lo = (f0.1 + f1.1) >> 1;
                let r_hi = (f0.1 - f1.1) >> 1;

                let h_idx = self.stage1_frame_in & self.stage1_hi_mask;
                self.stage1_hi_ring[h_idx] = (l_hi, r_hi);
                self.stage1_frame_in += 1;

                self.feed_stage2(l_lo, r_lo);
            }
        } else {
            self.feed_stage2(left_in, right_in);
        }

        if let Some(out) = self.output_queue.pop_front() {
            self.total_out_count += 1;
            Some(out)
        } else {
            None
        }
    }

    fn feed_stage2(&mut self, left_in: i32, right_in: i32) {
        if self.stage2_decimated {
            // Stage 2 (>48 kHz) Daubechies D4 split: pairs of frames entering Stage 2
            self.stage2_buffer.push((left_in, right_in));
            if self.stage2_buffer.len() == 2 {
                let f0 = self.stage2_buffer[0];
                let f1 = self.stage2_buffer[1];
                self.stage2_buffer.clear();

                let (l_lo, l_hi) = self.d4_stage2_left.analyze(f0.0, f1.0);
                let (r_lo, r_hi) = self.d4_stage2_right.analyze(f0.1, f1.1);

                let d4_idx = self.stage2_frame_in & self.stage2_hi_mask;
                self.stage2_hi_left_ring[d4_idx] = l_hi;
                self.stage2_hi_right_ring[d4_idx] = r_hi;
                self.stage2_frame_in += 1;

                self.feed_channel_or_merge(l_lo, r_lo);
            }
        } else {
            self.feed_channel_or_merge(left_in, right_in);
        }
    }

    fn feed_channel_or_merge(&mut self, left_in: i32, right_in: i32) {
        self.frame_in_count += 1;
        let in_idx = (self.frame_in_count as usize) & self.balance_mask;
        self.orig_left_ring[in_idx] = left_in;
        self.orig_right_ring[in_idx] = right_in;

        if self.merge_mode {
            // 4000-sample sliding balance sum
            self.level_left += (left_in as i64).abs();
            self.level_right += (right_in as i64).abs();

            // Merge mode (engine doc 3): 4000-sample sliding window initialised to 4000 * 200_000. The initial
            // value represents 4000 phantom samples of magnitude 200_000 that must slide out of
            // the window just like real samples. (Audit fix: they were never removed, leaving a
            // permanent 800M bias that pulled every balance toward 50/50.)
            if self.frame_in_count >= 4000 {
                let old_idx = ((self.frame_in_count - 4000) as usize) & self.balance_mask;
                self.level_left = (self.level_left - (self.orig_left_ring[old_idx] as i64).abs()).max(1);
                self.level_right = (self.level_right - (self.orig_right_ring[old_idx] as i64).abs()).max(1);
            } else {
                self.level_left = (self.level_left - 200_000).max(1);
                self.level_right = (self.level_right - 200_000).max(1);
            }

            let denom = (self.level_left + self.level_right).max(1);
            let balance = ((0x800000_i64 * self.level_left) / denom) as i32;

            // Centering: the mono pipeline is fed 2000 frames behind the read position so that the
            // 4000-sample level window [read - 3999, read] is centred on the merged sample. The
            // balance is therefore stored against the merged sample's index. (Audit fix: it was
            // stored against the read index, so the unmix used a trailing, un-centred window and
            // the 2000-frame look-ahead served no purpose.)
            let merge_in_count = self.frame_in_count - 2000;
            if merge_in_count >= 0 {
                let merge_idx = (merge_in_count as usize) & self.balance_mask;
                self.balance_ring[merge_idx] = balance;
                let mono_in = self.orig_left_ring[merge_idx] + self.orig_right_ring[merge_idx];

                if let Some(rep_mono) = self.left_channel.process_sample(mono_in) {
                    let out_frame = merge_in_count - (self.channel_sample_latency as isize) + 1;
                    if out_frame >= 0 {
                        let out_idx = (out_frame as usize) & self.balance_mask;
                        let orig_l = self.orig_left_ring[out_idx];
                        let orig_r = self.orig_right_ring[out_idx];
                        let orig_m = orig_l + orig_r;

                        let (out_l, out_r) = if rep_mono != orig_m {
                            let bal = self.balance_ring[out_idx] as f64;
                            let w_left = bal / (0x800000 as f64);
                            let w_right = 1.0 - w_left;
                            let delta = (rep_mono - orig_m) as f64;
                            (
                                orig_l + (w_left * delta) as i32,
                                orig_r + (w_right * delta) as i32,
                            )
                        } else {
                            (orig_l, orig_r)
                        };

                        if self.stage2_decimated {
                            if let Some(span) = self.left_channel.upcoming_repair_span() {
                                self.repair_stage2_high_band(span, true, true);
                            }
                        }
                        self.collect_channel_output(out_l, out_r);
                    }
                }
            }
        } else if self.right_channel.is_some() {
            let out_l = self.left_channel.process_sample(left_in);
            let span_l = if self.stage2_decimated {
                self.left_channel.upcoming_repair_span()
            } else {
                None
            };
            let (out_r, span_r) = {
                let right_chan = self.right_channel.as_mut().unwrap();
                let or = right_chan.process_sample(right_in);
                let sr = if self.stage2_decimated {
                    right_chan.upcoming_repair_span()
                } else {
                    None
                };
                (or, sr)
            };
            if let Some(span) = span_l {
                self.repair_stage2_high_band(span, true, false);
            }
            if let Some(span) = span_r {
                self.repair_stage2_high_band(span, false, true);
            }
            if let (Some(l), Some(r)) = (out_l, out_r) {
                self.collect_channel_output(l, r);
            }
        } else {
            let out_l = self.left_channel.process_sample(left_in);
            if self.stage2_decimated {
                if let Some(span) = self.left_channel.upcoming_repair_span() {
                    self.repair_stage2_high_band(span, true, true);
                }
            }
            if let Some(l) = out_l {
                self.collect_channel_output(l, l);
            }
        }
    }

    fn repair_stage2_high_band(&mut self, span: [isize; 2], do_left: bool, do_right: bool) {
        let gap_len = (span[1] - span[0]) as usize;
        if gap_len == 0 || gap_len > 1024 || span[0] <= 128 {
            return;
        }
        let ar_order = (2 * gap_len).clamp(50, 512);
        let burg_window = 2 * ar_order + 256;
        if (span[1] as usize) + burg_window > self.stage2_frame_in {
            return;
        }

        if do_left && span[1] > self.stage2_last_repaired_end_l {
            fill_gap(
                &self.stage2_hi_left_ring,
                self.stage2_hi_mask,
                span,
                &mut self.stage2_fixed,
                &mut self.stage2_scratch,
            );
            for k in 0..gap_len {
                let idx = ((span[0] + (k as isize)) as usize) & self.stage2_hi_mask;
                self.stage2_hi_left_ring[idx] = self.stage2_fixed[k];
                if self.stage1_resample {
                    let s1_idx0 = (2 * (span[0] + (k as isize)) as usize) & self.stage1_hi_mask;
                    let s1_idx1 = (2 * (span[0] + (k as isize)) as usize + 1) & self.stage1_hi_mask;
                    self.stage1_repaired_ring[s1_idx0].0 = true;
                    self.stage1_repaired_ring[s1_idx1].0 = true;
                }
            }
            self.stage2_last_repaired_end_l = span[1];
        }

        if do_right && span[1] > self.stage2_last_repaired_end_r {
            fill_gap(
                &self.stage2_hi_right_ring,
                self.stage2_hi_mask,
                span,
                &mut self.stage2_fixed,
                &mut self.stage2_scratch,
            );
            for k in 0..gap_len {
                let idx = ((span[0] + (k as isize)) as usize) & self.stage2_hi_mask;
                self.stage2_hi_right_ring[idx] = self.stage2_fixed[k];
                if self.stage1_resample {
                    let s1_idx0 = (2 * (span[0] + (k as isize)) as usize) & self.stage1_hi_mask;
                    let s1_idx1 = (2 * (span[0] + (k as isize)) as usize + 1) & self.stage1_hi_mask;
                    self.stage1_repaired_ring[s1_idx0].1 = true;
                    self.stage1_repaired_ring[s1_idx1].1 = true;
                }
            }
            self.stage2_last_repaired_end_r = span[1];
        }
    }

    fn collect_channel_output(&mut self, rep_left: i32, rep_right: i32) {
        if self.stage2_decimated {
            let lo_idx = self.stage2_channel_out_count & self.stage2_hi_mask;
            self.stage2_lo_left_ring[lo_idx] = rep_left;
            self.stage2_lo_right_ring[lo_idx] = rep_right;
            self.stage2_channel_out_count += 1;

            if self.stage2_channel_out_count > 1 {
                let out_k = self.stage2_channel_out_count - 2;
                let idx0 = out_k & self.stage2_hi_mask;
                let idx1 = (out_k + 1) & self.stage2_hi_mask;

                let lo0_l = self.stage2_lo_left_ring[idx0];
                let lo1_l = self.stage2_lo_left_ring[idx1];
                let hi0_l = self.stage2_hi_left_ring[idx0];
                let hi1_l = self.stage2_hi_left_ring[idx1];

                let lo0_r = self.stage2_lo_right_ring[idx0];
                let lo1_r = self.stage2_lo_right_ring[idx1];
                let hi0_r = self.stage2_hi_right_ring[idx0];
                let hi1_r = self.stage2_hi_right_ring[idx1];

                let (out0_l, out1_l) = synthesize_positive_up(lo0_l, hi0_l, lo1_l, hi1_l);
                let (out0_r, out1_r) = synthesize_positive_up(lo0_r, hi0_r, lo1_r, hi1_r);

                self.collect_stage1_output(out0_l, out0_r);
                self.collect_stage1_output(out1_l, out1_r);
            }
        } else {
            self.collect_stage1_output(rep_left, rep_right);
        }
    }

    fn collect_stage1_output(&mut self, rep_left: i32, rep_right: i32) {
        if self.stage1_resample {
            // Stage 1 Haar synthesis: given (L_rep, R_rep), reconstruct 2 raw frames with Haar high-band
            let h_idx = self.stage1_frame_out & self.stage1_hi_mask;
            let (mut l_hi, mut r_hi) = self.stage1_hi_ring[h_idx];
            let (rep_l, rep_r) = self.stage1_repaired_ring[h_idx];
            if rep_l {
                l_hi = 0;
            }
            if rep_r {
                r_hi = 0;
            }
            self.stage1_repaired_ring[h_idx] = (false, false);
            self.stage1_frame_out += 1;

            self.output_queue.push_back((rep_left + l_hi, rep_right + r_hi));
            self.output_queue.push_back((rep_left - l_hi, rep_right - r_hi));
        } else {
            self.output_queue.push_back((rep_left, rep_right));
        }
    }

    /// Processes a chunk of stereo frames, utilizing Rayon data-parallelism across Left and Right
    /// channels when in standard non-decimated Stereo mode.
    pub fn process_chunk(&mut self, chunk: &[(i32, i32)], out_frames: &mut Vec<(i32, i32)>) {
        out_frames.clear();
        #[cfg(feature = "parallel")]
        if !self.merge_mode
            && !self.stage1_resample
            && !self.stage2_decimated
            && self.right_channel.is_some()
        {
            let mut active_end = chunk.len();
            if let Some(tf) = self.total_frames {
                for (i, &(l, r)) in chunk.iter().enumerate() {
                    let frame_num = self.total_in_count + i + 1;
                    if frame_num > self.aligned_frames && frame_num <= tf {
                        self.trailing_samples.push((l, r));
                        if i < active_end {
                            active_end = i;
                        }
                    }
                }
            }
            self.total_in_count += chunk.len();
            let active_slice = &chunk[..active_end];

            let mut out_l = Vec::with_capacity(active_slice.len());
            let mut out_r = Vec::with_capacity(active_slice.len());
            let left_chan = &mut self.left_channel;
            let right_chan = self.right_channel.as_mut().unwrap();

            rayon::join(
                || {
                    for &(l, _) in active_slice {
                        if let Some(ol) = left_chan.process_sample(l) {
                            out_l.push(ol);
                        }
                    }
                },
                || {
                    for &(_, r) in active_slice {
                        if let Some(or) = right_chan.process_sample(r) {
                            out_r.push(or);
                        }
                    }
                },
            );

            let count = out_l.len().min(out_r.len());
            out_frames.reserve(count);
            for i in 0..count {
                out_frames.push((out_l[i], out_r[i]));
            }
            self.total_out_count += count;
            return;
        }

        out_frames.reserve(chunk.len());
        for &(l, r) in chunk {
            if let Some(out) = self.process_frame(l, r) {
                out_frames.push(out);
            }
        }
    }

    /// Rayon chunk parallelism across stereo channels when mode == ChannelMode::Stereo
    #[cfg(feature = "parallel")]
    pub fn process_stereo_chunk_parallel(
        &mut self,
        left_chunk: &[i32],
        right_chunk: &[i32],
        out_left: &mut Vec<i32>,
        out_right: &mut Vec<i32>,
    ) {
        if let Some(ref mut right_chan) = self.right_channel {
            let left_chan = &mut self.left_channel;
            rayon::join(
                || {
                    out_left.reserve(left_chunk.len());
                    for &s in left_chunk {
                        if let Some(out) = left_chan.process_sample(s) {
                            out_left.push(out);
                        }
                    }
                },
                || {
                    out_right.reserve(right_chunk.len());
                    for &s in right_chunk {
                        if let Some(out) = right_chan.process_sample(s) {
                            out_right.push(out);
                        }
                    }
                },
            );
        }
    }

    /// Flushes remaining delayed audio samples from the processing pipeline at end-of-track.
    pub fn flush(&mut self) -> Vec<(i32, i32)> {
        let mut flushed = Vec::new();

        // While there are pending outputs in queue
        while let Some(out) = self.output_queue.pop_front() {
            flushed.push(out);
            self.total_out_count += 1;
        }

        // Drain pipeline with zero padding
        let target_frames = self.total_frames.unwrap_or(self.total_in_count);
        let max_flush_steps = self.total_pipeline_delay + 8192;
        let mut steps = 0;

        while (self.total_out_count < self.aligned_frames || self.total_frames.is_none())
            && steps < max_flush_steps
        {
            if let Some(out) = self.process_frame(0, 0) {
                flushed.push(out);
            }
            steps += 1;
        }

        // Final Stage 2 synthesis flush if there is an un-synthesized trailing frame
        if self.stage2_decimated && self.stage2_channel_out_count > 0 {
            let out_k = self.stage2_channel_out_count - 1;
            let idx0 = out_k & self.stage2_hi_mask;

            let lo0_l = self.stage2_lo_left_ring[idx0];
            let hi0_l = self.stage2_hi_left_ring[idx0];
            let lo0_r = self.stage2_lo_right_ring[idx0];
            let hi0_r = self.stage2_hi_right_ring[idx0];

            let (out0_l, out1_l) = synthesize_positive_up(lo0_l, hi0_l, 0, 0);
            let (out0_r, out1_r) = synthesize_positive_up(lo0_r, hi0_r, 0, 0);

            if self.total_out_count < self.aligned_frames {
                flushed.push((out0_l, out0_r));
                self.total_out_count += 1;
            }
            if self.total_out_count < self.aligned_frames {
                flushed.push((out1_l, out1_r));
                self.total_out_count += 1;
            }
        }

        // Append unmodified trailing extra samples (engine doc 3)
        for &(l, r) in &self.trailing_samples {
            flushed.push((l, r));
            self.total_out_count += 1;
        }

        // Ensure total flushed matches target_frames if known
        if let Some(tf) = self.total_frames {
            if self.total_out_count > tf {
                flushed.truncate(flushed.len() - (self.total_out_count - tf));
            }
        }

        let _ = target_frames;
        flushed
    }

    /// Returns telemetry counters accumulated across processing channels.
    pub fn stats(&self) -> ProcessorStats {
        let left = &self.left_channel;
        let right = self.right_channel.as_ref();
        ProcessorStats {
            declick_events: left.declick_events + right.map(|r| r.declick_events).unwrap_or(0),
            declick_samples: left.declick_samples + right.map(|r| r.declick_samples).unwrap_or(0),
            protected_samples: left.protected_samples + right.map(|r| r.protected_samples).unwrap_or(0),
            subchannel_hint_events: left.subchannel_hint_events() + right.map(|r| r.subchannel_hint_events()).unwrap_or(0),
            cancelled_long_repairs: left.lo.cancelled_long + right.map(|r| r.lo.cancelled_long).unwrap_or(0),
            // Main-band samples are half-rate in Wavelet mode, and half again above 48 kHz.
            cancelled_long_samples: (left.lo.cancelled_long_len + right.map(|r| r.lo.cancelled_long_len).unwrap_or(0))
                * if left.is_wavelet { 2 } else { 1 }
                * self.decimation_factor,
            long_repairs: left.long_repairs + right.map(|r| r.long_repairs).unwrap_or(0),
            long_repair_samples: (left.long_repair_samples + right.map(|r| r.long_repair_samples).unwrap_or(0))
                * if left.is_wavelet { 2 } else { 1 }
                * self.decimation_factor,
            long_gated: left.long_gated + right.map(|r| r.long_gated).unwrap_or(0),
        }
    }

    /// Enhanced only: every repair the sustained-defect path made, as (channel, first frame,
    /// length in frames). Positions are accurate to a few samples (the wavelet split's delay).
    pub fn long_repair_log(&self) -> Vec<(usize, usize, usize)> {
        let scale = if self.left_channel.is_wavelet { 2 } else { 1 } * self.decimation_factor;
        let mut log: Vec<(usize, usize, usize)> = [Some(&self.left_channel), self.right_channel.as_ref()]
            .into_iter()
            .enumerate()
            .filter_map(|(ch, c)| c.map(|c| (ch, c)))
            .flat_map(|(ch, c)| {
                c.long_repair_log
                    .iter()
                    .map(move |&[s, e]| (ch, s.max(0) as usize * scale, (e - s).max(0) as usize * scale))
            })
            .collect();
        log.sort_by_key(|r| (r.1, r.0));
        log
    }

    /// Processes a slice of audio frames and returns exact sample-aligned output frames and telemetry statistics.
    pub fn process_slice_aligned(
        params: CrParameters,
        sample_rate: u32,
        frames: &[(i32, i32)],
    ) -> (Vec<(i32, i32)>, ProcessorStats) {
        let mut processor = Processor::new(params, sample_rate, Some(frames.len()));
        let mut output = Vec::with_capacity(frames.len());
        processor.process_chunk(frames, &mut output);
        output.extend(processor.flush());
        let stats = processor.stats();
        (output, stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_stereo_passthrough() {
        let mut params = CrParameters::default();
        params.dcl = 0;
        params.dcr = 0;
        params.method = crate::params::Method::Simple;
        params.mode = ChannelMode::Stereo;

        let n_frames = 2000;
        let mut processor = Processor::new(params, 44100, Some(n_frames));

        let mut input = Vec::new();
        for i in 0..n_frames {
            let s = ((i as f64 * 0.05).sin() * 1_000_000.0) as i32;
            input.push((s, s / 2));
        }

        let mut output = Vec::new();
        for &(l, r) in &input {
            if let Some(out) = processor.process_frame(l, r) {
                output.push(out);
            }
        }

        let flushed = processor.flush();
        output.extend(flushed);

        assert_eq!(output.len(), n_frames);
        for i in 0..500 {
            assert_eq!(output[i].0, input[i].0);
            assert_eq!(output[i].1, input[i].1);
        }
    }


    #[test]
    fn test_processor_dirac_alignment() {
        for mode in [ChannelMode::Stereo, ChannelMode::Merge] {
            for method in [
                crate::params::Method::Simple,
                crate::params::Method::Wavelet,
                crate::params::Method::WaveletX2,
            ] {
                let mut params = CrParameters::default();
                params.dcl = 0;
                params.dcr = 0;
                params.method = method;
                params.mode = mode;

                let n_frames = 6000;
                let mut input = vec![(0i32, 0i32); n_frames];
                input[3000] = (1_000_000, 500_000);

                let mut processor = Processor::new(params, 44100, Some(n_frames));
                let mut output = Vec::new();
                for &(l, r) in &input {
                    if let Some(out) = processor.process_frame(l, r) {
                        output.push(out);
                    }
                }
                output.extend(processor.flush());

                assert_eq!(output.len(), n_frames, "Failed length check for method {:?}, mode {:?}", method, mode);
                assert_eq!(
                    output[3000],
                    input[3000],
                    "Dirac pulse mismatch at index 3000 for method {:?}, mode {:?}",
                    method,
                    mode
                );
            }
        }
    }
}
