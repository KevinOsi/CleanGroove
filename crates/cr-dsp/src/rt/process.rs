//! Real-Time Multi-Rate Orchestration & Telemetry
//!
//! See docs/engine/REALTIME_ENGINE.md, sections 3, 4, 7 and 8.

use std::collections::VecDeque;

use crate::params::{RtParameters, RtSoundMode};
use crate::flag;
use crate::rt::channel::RtChannel;
use crate::rt::detector::{SUPER_BUFFER_LEN, SUPER_BUFFER_MASK};
use crate::wavelet::{synthesize_positive_up, D4Analyzer};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RtTelemetry {
    pub left_peak_linear: f64,
    pub right_peak_linear: f64,
    pub active_l_fixes: usize,
    pub active_r_fixes: usize,
    pub click_activity: f64,
}

pub struct RtProcessor {
    pub params: RtParameters,
    pub sample_rate: u32,
    pub stage1_resample: bool,
    pub stage2_decimated: bool,
    pub decimation_factor: usize,
    pub channel_rate: u32,

    pub left_channel: RtChannel,
    pub right_channel: RtChannel,

    pub in_count: isize,
    pub out_count: isize,
    pub proc_delay: usize,
    pub process_block_len: usize,
    pub filter_len: usize,

    // Primary 32,768-frame SuperBuffer
    pub l_samples: Vec<i32>,
    pub l_original: Vec<i32>,
    pub l_flags: Vec<u8>,
    pub r_samples: Vec<i32>,
    pub r_original: Vec<i32>,
    pub r_flags: Vec<u8>,

    // High-frequency subband buffers for >48 kHz Stage-2 D4 decimation
    pub l_hi_samples: Option<Vec<i32>>,
    pub l_hi_original: Option<Vec<i32>>,
    pub r_hi_samples: Option<Vec<i32>>,
    pub r_hi_original: Option<Vec<i32>>,
    pub d4_stage2_l: D4Analyzer,
    pub d4_stage2_r: D4Analyzer,

    // Stage 1 (>96 kHz) Haar decimation
    pub stage1_in_buf: Vec<(i32, i32)>,
    pub stage1_hi_orig: Vec<(i32, i32)>,
    pub stage1_hi_rep: Vec<(i32, i32)>,
    pub stage1_frame_in: usize,
    pub stage1_frame_out: usize,

    // Stage 2 input buffer and cadence queue
    pub stage2_in_buf: Vec<(i32, i32)>,
    pub output_queue: VecDeque<(i32, i32)>,

    // Telemetry running counters and accumulators
    pub active_l_fixes: usize,
    pub active_r_fixes: usize,
    pub left_peak_raw: i32,
    pub right_peak_raw: i32,
    pub telemetry: RtTelemetry,
}

impl RtProcessor {
    pub fn new(params: RtParameters, sample_rate: u32) -> Self {
        let stage1_resample = sample_rate > 96_000;
        let rate_stage1 = if stage1_resample {
            sample_rate / 2
        } else {
            sample_rate
        };
        let stage2_decimated = rate_stage1 > 48_000;
        let decimation_factor =
            (if stage1_resample { 2 } else { 1 }) * (if stage2_decimated { 2 } else { 1 });
        let channel_rate = sample_rate / (decimation_factor as u32);

        let is_wavelet = params.method.is_wavelet();
        let process_block_len = if is_wavelet { 1024 } else { 512 };
        let filter_len = if is_wavelet { 8192 } else { 4096 };
        let proc_delay = filter_len + 2 * process_block_len;

        let left_channel = RtChannel::new(params.clone(), channel_rate, stage2_decimated);
        let right_channel = RtChannel::new(params.clone(), channel_rate, stage2_decimated);

        let (l_hi_samples, l_hi_original, r_hi_samples, r_hi_original) = if stage2_decimated {
            (
                Some(vec![0; SUPER_BUFFER_LEN]),
                Some(vec![0; SUPER_BUFFER_LEN]),
                Some(vec![0; SUPER_BUFFER_LEN]),
                Some(vec![0; SUPER_BUFFER_LEN]),
            )
        } else {
            (None, None, None, None)
        };

        Self {
            params,
            sample_rate,
            stage1_resample,
            stage2_decimated,
            decimation_factor,
            channel_rate,
            left_channel,
            right_channel,
            in_count: 0,
            out_count: -(proc_delay as isize),
            proc_delay,
            process_block_len,
            filter_len,
            l_samples: vec![0; SUPER_BUFFER_LEN],
            l_original: vec![0; SUPER_BUFFER_LEN],
            l_flags: vec![0; SUPER_BUFFER_LEN],
            r_samples: vec![0; SUPER_BUFFER_LEN],
            r_original: vec![0; SUPER_BUFFER_LEN],
            r_flags: vec![0; SUPER_BUFFER_LEN],
            l_hi_samples,
            l_hi_original,
            r_hi_samples,
            r_hi_original,
            d4_stage2_l: D4Analyzer::new(),
            d4_stage2_r: D4Analyzer::new(),
            stage1_in_buf: Vec::with_capacity(2),
            stage1_hi_orig: vec![(0, 0); SUPER_BUFFER_LEN],
            stage1_hi_rep: vec![(0, 0); SUPER_BUFFER_LEN],
            stage1_frame_in: 0,
            stage1_frame_out: 0,
            stage2_in_buf: Vec::with_capacity(2),
            output_queue: VecDeque::with_capacity(8),
            active_l_fixes: 0,
            active_r_fixes: 0,
            left_peak_raw: 0,
            right_peak_raw: 0,
            telemetry: RtTelemetry::default(),
        }
    }

    pub fn set_parameters(&mut self, params: RtParameters) {
        self.left_channel.set_parameters(params.clone());
        self.right_channel.set_parameters(params.clone());
        self.params = params;
    }

    pub fn set_mode(&mut self, mode: RtSoundMode) {
        self.params.mode = mode;
        self.left_channel.params.mode = mode;
        self.right_channel.params.mode = mode;
    }

    /// Feeds one stereo input frame (left_in, right_in) and retrieves one restored/delayed stereo frame.
    pub fn process_frame(&mut self, left_in: i32, right_in: i32) -> (i32, i32) {
        // Track raw input peak levels
        self.left_peak_raw = self.left_peak_raw.max(left_in.abs());
        self.right_peak_raw = self.right_peak_raw.max(right_in.abs());

        if self.stage1_resample {
            // >96 kHz Stage 1 Haar split
            self.stage1_in_buf.push((left_in, right_in));
            if self.stage1_in_buf.len() == 2 {
                let f0 = self.stage1_in_buf[0];
                let f1 = self.stage1_in_buf[1];
                self.stage1_in_buf.clear();

                let l_lo = (f0.0 + f1.0) >> 1;
                let l_hi = (f0.0 - f1.0) >> 1;
                let r_lo = (f0.1 + f1.1) >> 1;
                let r_hi = (f0.1 - f1.1) >> 1;

                let h_idx = self.stage1_frame_in & SUPER_BUFFER_MASK;
                self.stage1_hi_orig[h_idx] = (l_hi, r_hi);
                self.stage1_hi_rep[h_idx] = (l_hi, r_hi);
                self.stage1_frame_in = self.stage1_frame_in.wrapping_add(1);

                self.feed_stage2_frame(l_lo, r_lo);
            }
        } else {
            self.feed_stage2_frame(left_in, right_in);
        }

        // Return synthesized frame from queue or zero padding during initial pipeline latency
        if let Some(out) = self.output_queue.pop_front() {
            out
        } else {
            (0, 0)
        }
    }

    fn feed_stage2_frame(&mut self, left_in: i32, right_in: i32) {
        if self.stage2_decimated {
            // >48 kHz Stage 2 D4 QMF split
            self.stage2_in_buf.push((left_in, right_in));
            if self.stage2_in_buf.len() == 2 {
                let f0 = self.stage2_in_buf[0];
                let f1 = self.stage2_in_buf[1];
                self.stage2_in_buf.clear();

                let (l_lo, l_hi) = self.d4_stage2_l.analyze(f0.0, f1.0);
                let (r_lo, r_hi) = self.d4_stage2_r.analyze(f0.1, f1.1);

                self.step_super_buffer(l_lo, r_lo, Some(l_hi), Some(r_hi));
            }
        } else {
            self.step_super_buffer(left_in, right_in, None, None);
        }
    }

    fn step_super_buffer(
        &mut self,
        l_in: i32,
        r_in: i32,
        l_hi_in: Option<i32>,
        r_hi_in: Option<i32>,
    ) {
        let in_idx = (self.in_count as usize) & SUPER_BUFFER_MASK;
        self.l_samples[in_idx] = l_in;
        self.l_original[in_idx] = l_in;
        if (self.l_flags[in_idx] & flag::REPAIRED) != 0 {
            self.active_l_fixes = self.active_l_fixes.saturating_sub(1);
        }
        self.l_flags[in_idx] = 0;

        self.r_samples[in_idx] = r_in;
        self.r_original[in_idx] = r_in;
        if (self.r_flags[in_idx] & flag::REPAIRED) != 0 {
            self.active_r_fixes = self.active_r_fixes.saturating_sub(1);
        }
        self.r_flags[in_idx] = 0;

        if let (Some(l_hi), Some(r_hi)) = (l_hi_in, r_hi_in) {
            if let Some(ref mut l_hi_s) = self.l_hi_samples {
                l_hi_s[in_idx] = l_hi;
            }
            if let Some(ref mut l_hi_o) = self.l_hi_original {
                l_hi_o[in_idx] = l_hi;
            }
            if let Some(ref mut r_hi_s) = self.r_hi_samples {
                r_hi_s[in_idx] = r_hi;
            }
            if let Some(ref mut r_hi_o) = self.r_hi_original {
                r_hi_o[in_idx] = r_hi;
            }
        }

        self.in_count += 1;

        // Block execution boundary
        let p_len = self.process_block_len as isize;
        if (self.in_count & (p_len - 1)) == 0 && self.in_count >= 2 * p_len {
            self.execute_block();

            if self.in_count > 2 * 131_072 {
                self.shift_positions(131_072);
            }
        }

        // Produce output frames
        if self.out_count >= 0 {
            let out_idx = (self.out_count as usize) & SUPER_BUFFER_MASK;
            if (self.l_flags[out_idx] & flag::REPAIRED) != 0 {
                self.active_l_fixes += 1;
            }
            if (self.r_flags[out_idx] & flag::REPAIRED) != 0 {
                self.active_r_fixes += 1;
            }

            if self.stage2_decimated {
                let idx0 = out_idx;
                let idx1 = (idx0 + 1) & SUPER_BUFFER_MASK;

                let l_samples = &self.l_samples;
                let l_original = &self.l_original;
                let l_hi_s = self.l_hi_samples.as_ref().unwrap();
                let l_hi_o = self.l_hi_original.as_ref().unwrap();

                let r_samples = &self.r_samples;
                let r_original = &self.r_original;
                let r_hi_s = self.r_hi_samples.as_ref().unwrap();
                let r_hi_o = self.r_hi_original.as_ref().unwrap();

                // Left channel synthesis
                let (rep_l0, rep_l1) =
                    synthesize_positive_up(l_samples[idx0], l_hi_s[idx0], l_samples[idx1], l_hi_s[idx1]);
                let (orig_l0, orig_l1) =
                    synthesize_positive_up(l_original[idx0], l_hi_o[idx0], l_original[idx1], l_hi_o[idx1]);

                // Right channel synthesis (uses r_hi_s[idx0]; realtime engine doc 8)
                let (rep_r0, rep_r1) =
                    synthesize_positive_up(r_samples[idx0], r_hi_s[idx0], r_samples[idx1], r_hi_s[idx1]);
                let (orig_r0, orig_r1) =
                    synthesize_positive_up(r_original[idx0], r_hi_o[idx0], r_original[idx1], r_hi_o[idx1]);

                let (pair0, pair1) = match self.params.mode {
                    RtSoundMode::SoundOut => ((rep_l0, rep_r0), (rep_l1, rep_r1)),
                    RtSoundMode::SoundIn => ((orig_l0, orig_r0), (orig_l1, orig_r1)),
                    RtSoundMode::Noise => (
                        (rep_l0 - orig_l0, rep_r0 - orig_r0),
                        (rep_l1 - orig_l1, rep_r1 - orig_r1),
                    ),
                };

                if self.stage1_resample {
                    let l_repaired = (self.l_flags[idx0] & flag::REPAIRED) != 0 || (self.l_flags[idx1] & flag::REPAIRED) != 0;
                    let r_repaired = (self.r_flags[idx0] & flag::REPAIRED) != 0 || (self.r_flags[idx1] & flag::REPAIRED) != 0;
                    let h0 = self.stage1_frame_out & SUPER_BUFFER_MASK;
                    let h1 = self.stage1_frame_out.wrapping_add(1) & SUPER_BUFFER_MASK;
                    if l_repaired {
                        self.stage1_hi_rep[h0].0 = 0;
                        self.stage1_hi_rep[h1].0 = 0;
                    }
                    if r_repaired {
                        self.stage1_hi_rep[h0].1 = 0;
                        self.stage1_hi_rep[h1].1 = 0;
                    }
                    self.synthesize_stage1_pairs(pair0, pair1);
                } else {
                    self.output_queue.push_back(pair0);
                    self.output_queue.push_back(pair1);
                }
            } else {
                let out_frame = match self.params.mode {
                    RtSoundMode::SoundOut => (self.l_samples[out_idx], self.r_samples[out_idx]),
                    RtSoundMode::SoundIn => (self.l_original[out_idx], self.r_original[out_idx]),
                    RtSoundMode::Noise => (
                        self.l_samples[out_idx] - self.l_original[out_idx],
                        self.r_samples[out_idx] - self.r_original[out_idx],
                    ),
                };

                if self.stage1_resample {
                    self.synthesize_stage1_single(out_frame);
                } else {
                    self.output_queue.push_back(out_frame);
                }
            }
        } else {
            // Warm-up pipeline latency: emit zeros
            if self.stage2_decimated {
                if self.stage1_resample {
                    self.output_queue.push_back((0, 0));
                    self.output_queue.push_back((0, 0));
                    self.output_queue.push_back((0, 0));
                    self.output_queue.push_back((0, 0));
                } else {
                    self.output_queue.push_back((0, 0));
                    self.output_queue.push_back((0, 0));
                }
            } else {
                self.output_queue.push_back((0, 0));
            }
        }

        self.out_count += 1;
    }

    fn synthesize_stage1_single(&mut self, rep: (i32, i32)) {
        let h_idx = self.stage1_frame_out & SUPER_BUFFER_MASK;
        let (l_hi_orig, r_hi_orig) = self.stage1_hi_orig[h_idx];
        let (l_hi_rep, r_hi_rep) = self.stage1_hi_rep[h_idx];
        self.stage1_frame_out = self.stage1_frame_out.wrapping_add(1);

        let (l_hi, r_hi) = match self.params.mode {
            RtSoundMode::SoundOut => (l_hi_rep, r_hi_rep),
            RtSoundMode::SoundIn => (l_hi_orig, r_hi_orig),
            RtSoundMode::Noise => (l_hi_rep - l_hi_orig, r_hi_rep - r_hi_orig),
        };

        self.output_queue.push_back((rep.0 + l_hi, rep.1 + r_hi));
        self.output_queue.push_back((rep.0 - l_hi, rep.1 - r_hi));
    }

    fn synthesize_stage1_pairs(&mut self, p0: (i32, i32), p1: (i32, i32)) {
        self.synthesize_stage1_single(p0);
        self.synthesize_stage1_single(p1);
    }

    fn execute_block(&mut self) {
        let l_hi_ref = self.l_hi_samples.as_deref_mut();
        let r_hi_ref = self.r_hi_samples.as_deref_mut();

        #[cfg(all(feature = "parallel", not(target_arch = "wasm32")))]
        if self.params.parallel {
            let left_chan = &mut self.left_channel;
            let l_s = &mut self.l_samples;
            let l_f = &mut self.l_flags;

            let right_chan = &mut self.right_channel;
            let r_s = &mut self.r_samples;
            let r_f = &mut self.r_flags;

            rayon::join(
                || left_chan.process_block(l_s, l_hi_ref, l_f),
                || right_chan.process_block(r_s, r_hi_ref, r_f),
            );
            self.update_telemetry();
            return;
        }

        self.left_channel
            .process_block(&mut self.l_samples, l_hi_ref, &mut self.l_flags);
        self.right_channel
            .process_block(&mut self.r_samples, r_hi_ref, &mut self.r_flags);

        self.update_telemetry();
    }

    fn update_telemetry(&mut self) {
        let window_len = (SUPER_BUFFER_LEN - self.proc_delay).max(1);
        let l_fixes = self.active_l_fixes;
        let r_fixes = self.active_r_fixes;

        let l_level = self.left_peak_raw;
        let r_level = self.right_peak_raw;
        self.left_peak_raw = 0;
        self.right_peak_raw = 0;

        let raw_density = ((l_fixes + r_fixes) as f64) / (window_len as f64);
        let activity = if l_level + r_level < 10_000 {
            0.0
        } else {
            (0.16 * ((raw_density + 1.0e-4).ln() + 8.0)).clamp(0.0, 1.0)
        };

        self.telemetry = RtTelemetry {
            left_peak_linear: (l_level as f64) / 16_777_216.0,
            right_peak_linear: (r_level as f64) / 16_777_216.0,
            active_l_fixes: l_fixes,
            active_r_fixes: r_fixes,
            click_activity: activity,
        };
    }

    pub fn shift_positions(&mut self, n: isize) {
        self.in_count -= n;
        self.out_count -= n;
        self.left_channel.shift_positions(n);
        self.right_channel.shift_positions(n);
    }

    /// Processes a slice of input stereo frames in-place or into output slice.
    pub fn process_frames(&mut self, input: &[(i32, i32)], output: &mut [(i32, i32)]) {
        let count = input.len().min(output.len());
        for i in 0..count {
            output[i] = self.process_frame(input[i].0, input[i].1);
        }
    }
}
