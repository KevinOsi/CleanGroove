//! Primary Full-Rate Channel Pipeline & Ring Buffers
//!
//! See docs/engine/OFFLINE_ENGINE.md, sections 3, 5, 6, 9 and 10.

use crate::burg::{reflection_to_predictor, update_excitation};
use crate::flag;
use crate::detector::{refine_click, BandState};
use crate::kalman::{block_refl, process_kalman_frame, process_kalman_wavelet_frame, KalmanBandState};
use crate::params::{CrParameters, DerivedChannelParams, Engine};
use crate::pitch::protect_pitched_repairs;
use crate::reconstruct::{fill_gap, ScratchSpace};
use crate::subchannel::SubChannel;
use crate::wavelet::{time_span_to_band_span, band_spans_to_time_span, synthesize_zero_up, D4Analyzer};

/// Classic's click window: the scanner abandons a click once it has lasted this many samples
/// (band samples in Wavelet mode).
const CLICK_WINDOW: isize = 256;

/// Enhanced reach (engine doc 13).
///
/// Classic's detector is a transient detector: its reference level (the shoulder before the
/// window) fills with a sustained defect within a few samples, so a gouge of several ms is seen
/// only at its onset, the click "ends" inside it and the edge search finds no repair. Enhanced
/// leaves that state machine alone. When a click ends, it checks whether the excitation stays
/// above `LONG_SUSTAIN` times the detection level, measured against the level frozen at the
/// click's onset; if so, the whole sustained stretch (up to the long click window) is analysed
/// with the long repair limit, and the repair is made only if it passes the music gate below.
/// Otherwise the click goes down Classic's path unchanged.
///
/// In Wavelet mode a band sample is two output samples, so at 44.1 kHz the long click window is
/// 29 ms and the longest automatic repair 23 ms.
const ENH_CLICK_WINDOW_WAVELET: isize = 640;
const ENH_MAX_AUTO_REPAIR_WAVELET: usize = 512;
const ENH_CLICK_WINDOW_SIMPLE: isize = 1280;
const ENH_MAX_AUTO_REPAIR_SIMPLE: usize = 1024;
/// Span (samples) of the running mean that tracks a sustained defect, and the fraction of the
/// detection level it must hold.
const SUSTAIN_SPAN: isize = 16;
const LONG_SUSTAIN: f64 = 0.5;

/// Enhanced long-repair gate: a repair from the sustained-defect path is made only where the
/// defect stands clear of the music on BOTH sides of it, measured over `LONG_SIDE` samples each
/// side (about 12 ms at 44.1 kHz), both bands together (the wavelet split keeps energy):
/// - signal: what the repair removes (RMS) is at least `LONG_GATE_SIGNAL_DB` above the audio;
/// - excitation (the order-10 prediction error the detector works on): the defect's is at least
///   `LONG_GATE_EXCIT_DB` above the audio's. Music is predictable either side of a gouge; a
///   drum hit or a new note keeps the audio after it unpredictable as well.
///
/// This is the analysis' exposed-click idea applied to the repair itself. Set from the long
/// candidates on 600 s of the clean reference album with gapbench defects added and on the
/// whole album (20,500 music candidates, 2026-10-29): no music candidate reaches both. The
/// nearest are isolated loud hits in quiet passages (excitation 17.4 dB with signal 16.4 dB)
/// and onsets standing 20-25 dB in excitation but under 1.3 dB in signal.
const LONG_GATE_SIGNAL_DB: f64 = 6.0;
const LONG_GATE_EXCIT_DB: f64 = 20.0;
const LONG_SIDE_WAVELET: isize = 256;
const LONG_SIDE_SIMPLE: isize = 512;

/// Primary Channel audio restoration pipeline.
#[derive(Debug, Clone)]
pub struct Channel {
    pub params: CrParameters,
    pub derived: DerivedChannelParams,
    pub buffer_length: usize,
    pub buffer_mask: usize,
    pub coeff_length: usize,
    pub pipeline_delay: usize,
    pub min_offset: isize,
    pub max_offset: isize,
    pub curr_offset: isize,
    pub max_auto_repair: usize,
    /// Classic's repair limit (Enhanced's click path keeps it; see `try_long_repair`).
    pub classic_max_auto_repair: usize,
    pub click_window: isize,
    pub long_side: isize,
    pub enhanced: bool,
    pub edge: isize,
    pub lo: BandState,
    pub hi: BandState,
    pub flags: Vec<u8>,
    pub kalman_lo: KalmanBandState,
    pub kalman_hi: KalmanBandState,
    pub subchannel: Option<Box<SubChannel>>,
    pub d4: D4Analyzer,
    pub scratch: ScratchSpace,

    /// Position of the newest sample (-1 before the first).
    pub frame_count: isize,
    // How far each stage runs behind the samples taken in (`frame_count + 1`): a stage works on
    // position `frame_count + 1 - lag`.
    /// Prediction errors: a 256-sample block is fitted when this position reaches its end.
    pub excitation_lag: isize,
    /// DeCrackle.
    pub decrackle_lag: isize,
    /// DeCrackle stops at this position.
    pub decrackle_end: isize,
    /// The click scan runs every 128 samples of this position.
    pub scan_lag: isize,
    /// The output.
    pub output_lag: isize,
    /// Where pitch protection looks next (it jumps ahead over the repairs it has examined).
    pub pitch_count: isize,

    pub repairs_pending: usize,
    pub protected_total: usize,

    // Declick detection state machine fields
    pub sums_valid: bool,
    pub last_repair: isize,
    pub loop_total: isize,
    pub number_of_frames: isize,
    pub loop_limit: isize,
    pub detect_factor: f64,
    pub onset_factor: f64,

    // Wavelet sample pair buffering
    pub is_wavelet: bool,
    pub has_buffered_even: bool,
    pub buffered_even_in: i32,
    pub buffered_odd_out: Option<i32>,

    // Telemetry counters
    pub declick_events: usize,
    pub declick_samples: usize,
    pub protected_samples: usize,
    pub subchannel_hint_events: usize,
    /// Enhanced only: repairs made by the sustained-defect path (see `try_long_repair`; most
    /// are longer than Classic can make, some merge a long defect with short clicks), their
    /// total length, the candidates its music gate refused, and where each repair is
    /// ([start, end) in this channel's samples).
    pub long_repairs: usize,
    pub long_repair_samples: usize,
    pub long_gated: usize,
    pub long_repair_log: Vec<[isize; 2]>,
}

impl Channel {
    pub fn new(params: CrParameters, number_of_frames: isize) -> Self {
        let is_wavelet = params.method.is_wavelet();
        let is_reverse = params.reverse;
        let num_sub = params.method.num_subchannels();

        let derived = DerivedChannelParams::compute(
            params.dcl,
            params.dcr,
            is_wavelet,
            is_reverse,
            num_sub,
        );

        let enhanced = params.engine == Engine::Enhanced;
        let classic_max_auto_repair = if is_wavelet { 128 } else { 256 };
        let (click_window, max_auto_repair) = match (enhanced, is_wavelet) {
            (false, _) => (CLICK_WINDOW, classic_max_auto_repair),
            (true, true) => (ENH_CLICK_WINDOW_WAVELET, ENH_MAX_AUTO_REPAIR_WAVELET),
            (true, false) => (ENH_CLICK_WINDOW_SIMPLE, ENH_MAX_AUTO_REPAIR_SIMPLE),
        };
        let max_repair = 2048;
        // Look-ahead. A click can start just before the scan reaches `min_offset` and run on for
        // a whole click window, after which the edge search and the repair's backward AR model
        // (up to 2 * 512 + 256 samples) read the audio after it. Classic's 1088 covers its own
        // window and repair limit; Enhanced adds the longer window and the full model length.
        let min_offset: isize = if enhanced { 1088 + (click_window - CLICK_WINDOW) + 768 } else { 1088 };
        let max_offset: isize = min_offset + 4 * (max_auto_repair as isize) + 128 + 384;
        let mut pipeline_delay = (max_offset + 2 * (max_auto_repair as isize) + 256 + 384) as usize;

        let channel_frames = if number_of_frames > 0 {
            number_of_frames
        } else {
            isize::MAX / 2
        };

        if params.dcr > 0 {
            pipeline_delay += 192;
        }

        let subchannel = if num_sub > 0 {
            let sub = SubChannel::new(0, num_sub - 1, params.dcl, is_reverse, is_wavelet, channel_frames);
            let sub_delay = sub.pipeline_delay();
            if params.compat_byte_delay {
                let sub_delay_byte = (sub_delay as i8) as isize;
                pipeline_delay = ((pipeline_delay as isize) + sub_delay_byte) as usize;
            } else {
                pipeline_delay += 4 * sub_delay;
            }
            Some(Box::new(sub))
        } else {
            None
        };

        let mut buffer_length = 4096;
        while buffer_length <= pipeline_delay + 2 * max_repair {
            buffer_length <<= 1;
        }
        let buffer_mask = buffer_length - 1;
        let coeff_length = 10 * (buffer_length / 256);

        let edge = if is_wavelet { 64 } else { 128 };

        let frame_count: isize = -1;
        // The excitation stage starts 512 samples back with DeCrackle, else 320. With -512 every 512-sample
        // Burg window [base - 128, base + 384) is fully written when its block is generated, so the
        // reflection coefficients the Kalman filter loads are final (with -320 the newest block's
        // window still contained 64 stale ring-buffer samples when the Kalman stage loaded it).
        let excitation_lag: isize = if params.dcr > 0 { 512 } else { 320 };
        // The Kalman stage lags the excitation stage by one frame so the 3-point detector's
        // look-ahead read (fwd_excit[k + 1]) never touches a block that has not been generated
        // yet. (A start of -192 would be inconsistent with the excitation look-ahead.)
        // Without DeCrackle the stage never runs: its end is 0 and its position is past it.
        let decrackle_lag: isize = if params.dcr > 0 {
            513
        } else {
            -(pipeline_delay as isize)
        };
        let decrackle_end: isize = if params.dcr > 0 {
            channel_frames.saturating_add(64)
        } else {
            0
        };
        let scan_lag: isize = pipeline_delay as isize - min_offset - 128;
        let pitch_count: isize = -(pipeline_delay as isize) + min_offset - 384;
        let output_lag: isize = pipeline_delay as isize;

        let loop_limit = channel_frames - min_offset;
        let loop_total = channel_frames + 100_000;

        Self {
            params,
            detect_factor: derived.normal.detect_factor,
            onset_factor: derived.normal.onset_factor,
            derived,
            buffer_length,
            buffer_mask,
            coeff_length,
            pipeline_delay,
            min_offset,
            max_offset,
            curr_offset: -edge - 1,
            max_auto_repair,
            classic_max_auto_repair,
            click_window,
            long_side: if is_wavelet { LONG_SIDE_WAVELET } else { LONG_SIDE_SIMPLE },
            enhanced,
            edge,
            lo: BandState::new(buffer_length, coeff_length),
            hi: BandState::new(buffer_length, coeff_length),
            flags: vec![0; buffer_length],
            kalman_lo: KalmanBandState::new(is_wavelet),
            kalman_hi: KalmanBandState::new(is_wavelet),
            subchannel,
            d4: D4Analyzer::new(),
            scratch: ScratchSpace::new(),
            frame_count,
            excitation_lag,
            decrackle_lag,
            decrackle_end,
            scan_lag,
            output_lag,
            pitch_count,
            repairs_pending: 0,
            protected_total: 0,
            sums_valid: false,
            last_repair: loop_total,
            loop_total,
            number_of_frames: channel_frames,
            loop_limit,
            is_wavelet,
            has_buffered_even: false,
            buffered_even_in: 0,
            buffered_odd_out: None,
            declick_events: 0,
            declick_samples: 0,
            protected_samples: 0,
            subchannel_hint_events: 0,
            long_repairs: 0,
            long_repair_samples: 0,
            long_gated: 0,
            long_repair_log: Vec::new(),
        }
    }

    /// Recursively counts all subchannel hint events across all decimation depths.
    /// Position of the sample at the output.
    pub fn output_pos(&self) -> isize {
        self.frame_count + 1 - self.output_lag
    }

    pub fn subchannel_hint_events(&self) -> usize {
        self.subchannel.as_ref().map(|s| s.total_subchannel_hints()).unwrap_or(0)
    }

    /// Feeds one input sample and retrieves one output sample (or None while pipeline is filling).
    pub fn process_sample(&mut self, sample_in: i32) -> Option<i32> {
        if self.is_wavelet {
            if !self.has_buffered_even {
                self.buffered_even_in = sample_in;
                self.has_buffered_even = true;
                self.buffered_odd_out.take()
            } else {
                self.has_buffered_even = false;
                let (out_even, out_odd) = self.process_wavelet_step(self.buffered_even_in, sample_in);
                self.buffered_odd_out = out_odd;
                out_even
            }
        } else {
            self.process_simple_step(sample_in)
        }
    }

    fn process_simple_step(&mut self, sample_in: i32) -> Option<i32> {
        self.frame_count += 1;
        self.pitch_count += 1;
        let excitation_pos = self.frame_count + 1 - self.excitation_lag;
        let decrackle_pos = self.frame_count + 1 - self.decrackle_lag;
        let scan_trigger = self.frame_count + 1 - self.scan_lag;
        let output_pos = self.output_pos();
        self.curr_offset = (self.curr_offset + 1).min(self.max_offset);

        let mask = self.buffer_mask;
        let idx = (self.frame_count as usize) & mask;
        self.lo.samples[idx] = sample_in;
        self.lo.original[idx] = sample_in;
        self.flags[idx] = 0;

        // SubChannel decimation cascade
        if let Some(ref mut sub) = self.subchannel {
            if (self.frame_count & 3) == 0 {
                let avg_lo = (self.lo.original[idx]
                    + self.lo.original[idx.wrapping_sub(1) & mask]
                    + self.lo.original[idx.wrapping_sub(2) & mask]
                    + self.lo.original[idx.wrapping_sub(3) & mask])
                    / 4;
                sub.take_pair(avg_lo, 0, &mut self.flags, mask);
                self.subchannel_hint_events = sub.total_subchannel_hints();
            }
        }

        // Block-256 Excitation Generation
        if (excitation_pos & 0xFF) == 0 && excitation_pos >= 0 {
            update_excitation(
                &self.lo.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                excitation_pos - 256,
                excitation_pos,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
        }

        // Kalman DeCrackle Filter
        let (_committed, needs_regen) = process_kalman_frame(
            &mut self.kalman_lo,
            decrackle_pos,
            self.decrackle_end,
            self.edge,
            mask,
            &mut self.lo.samples,
            &self.lo.fwd_excit,
            &self.lo.block_refls,
            &mut self.flags,
            self.derived.error_decay,
            self.derived.crackle_factor,
            self.derived.decrackle_enabled,
        );

        if needs_regen {
            update_excitation(
                &self.lo.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                decrackle_pos - 10,
                decrackle_pos,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
            // The model is refreshed from the new fit; the crackle score carries on.
            reflection_to_predictor(10, block_refl(&self.lo.block_refls, decrackle_pos), &mut self.kalman_lo.ar);
        }

        // Sliding-Window DeClick Scanner
        if self.derived.declick_enabled
            && (scan_trigger & 0x7F) == 0
            && scan_trigger >= 0
            && scan_trigger < self.loop_limit
        {
            if self.params.reverse {
                self.scan_backward();
            } else {
                self.scan_forward();
            }
        }

        // Harmonic Pitch Protection
        if self.params.detect_pitch
            && self.pitch_count >= 0
            && (self.flags[(self.pitch_count as usize) & mask] & flag::PITCH_CANDIDATE) != 0
            && self.repairs_pending > 4
        {
            protect_pitched_repairs(
                &mut self.lo.samples,
                &self.lo.original,
                &mut self.hi.samples,
                &self.hi.original,
                &mut self.flags,
                mask,
                self.buffer_length as isize,
                self.coeff_length,
                self.frame_count,
                &mut self.pitch_count,
                &mut self.repairs_pending,
                &mut self.protected_total,
                32,
                false,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.hi.block_refls,
                &mut self.scratch,
            );
        }

        if output_pos >= 0 {
            let out_idx = (output_pos as usize) & mask;
            let next_out_idx = ((output_pos + 1) as usize) & mask;
            let flag = self.flags[out_idx];
            let next_flag = self.flags[next_out_idx];

            if (flag & flag::REPAIRED) != 0 {
                self.declick_samples += 1;
            }
            if (flag & flag::PROTECTED) != 0 {
                self.protected_samples += 1;
            }
            if (flag & flag::REPAIRED) != 0 && (next_flag & flag::REPAIRED) == 0 {
                self.declick_events += 1;
            }

            // Unreverted DeClick repair interval passes output.
            // Only REPAIRED intervals are counted in apply_repair(), so only REPAIRED intervals are
            // checked out here (DeCrackle commits never incremented the counter).
            if (flag & flag::REPAIRED) != 0 && (next_flag & flag::REPAIRED) == 0 {
                self.repairs_pending = self.repairs_pending.saturating_sub(1);
            }
            Some(self.lo.samples[out_idx])
        } else {
            None
        }
    }

    fn process_wavelet_step(&mut self, even: i32, odd: i32) -> (Option<i32>, Option<i32>) {
        self.frame_count += 1;
        self.pitch_count += 1;
        let excitation_pos = self.frame_count + 1 - self.excitation_lag;
        let decrackle_pos = self.frame_count + 1 - self.decrackle_lag;
        let scan_trigger = self.frame_count + 1 - self.scan_lag;
        let output_pos = self.output_pos();
        self.curr_offset = (self.curr_offset + 1).min(self.max_offset);

        let (lo_val, hi_val) = self.d4.analyze(even, odd);
        let mask = self.buffer_mask;
        let idx = (self.frame_count as usize) & mask;
        self.lo.samples[idx] = lo_val;
        self.lo.original[idx] = lo_val;
        self.hi.samples[idx] = hi_val;
        self.hi.original[idx] = hi_val;
        self.flags[idx] = 0;

        // SubChannel decimation cascade
        if let Some(ref mut sub) = self.subchannel {
            if (self.frame_count & 3) == 0 {
                let avg_lo = (self.lo.original[idx]
                    + self.lo.original[idx.wrapping_sub(1) & mask]
                    + self.lo.original[idx.wrapping_sub(2) & mask]
                    + self.lo.original[idx.wrapping_sub(3) & mask])
                    / 4;
                let avg_hi = (self.hi.original[idx]
                    + self.hi.original[idx.wrapping_sub(1) & mask]
                    + self.hi.original[idx.wrapping_sub(2) & mask]
                    + self.hi.original[idx.wrapping_sub(3) & mask])
                    / 4;
                sub.take_pair(avg_lo, avg_hi, &mut self.flags, mask);
                self.subchannel_hint_events = sub.total_subchannel_hints();
            }
        }

        // Block-256 Excitation Generation for both Lo and Hi bands
        if (excitation_pos & 0xFF) == 0 && excitation_pos >= 0 {
            update_excitation(
                &self.lo.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                excitation_pos - 256,
                excitation_pos,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
            update_excitation(
                &self.hi.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                excitation_pos - 256,
                excitation_pos,
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.hi.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
        }

        // Coupled Kalman DeCrackle Filter for Wavelet mode
        let (needs_regen_lo, needs_regen_hi) = process_kalman_wavelet_frame(
            &mut self.kalman_lo,
            &mut self.kalman_hi,
            decrackle_pos,
            self.decrackle_end,
            self.edge,
            mask,
            &mut self.lo.samples,
            &mut self.hi.samples,
            &self.lo.fwd_excit,
            &self.hi.fwd_excit,
            &self.lo.block_refls,
            &self.hi.block_refls,
            &mut self.flags,
            self.derived.error_decay,
            self.derived.crackle_factor,
            self.derived.decrackle_enabled,
        );

        if needs_regen_lo || needs_regen_hi {
            update_excitation(
                &self.lo.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                decrackle_pos - 10,
                decrackle_pos,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
            update_excitation(
                &self.hi.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                decrackle_pos - 10,
                decrackle_pos,
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.hi.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
            // The model is refreshed from the new fit; the crackle score carries on.
            reflection_to_predictor(10, block_refl(&self.lo.block_refls, decrackle_pos), &mut self.kalman_lo.ar);
            reflection_to_predictor(10, block_refl(&self.hi.block_refls, decrackle_pos), &mut self.kalman_hi.ar);
        }

        // Sliding-Window DeClick Scanner
        if self.derived.declick_enabled
            && (scan_trigger & 0x7F) == 0
            && scan_trigger >= 0
            && scan_trigger < self.loop_limit
        {
            if self.params.reverse {
                self.scan_backward();
            } else {
                self.scan_forward();
            }
        }

        // Harmonic Pitch Protection
        if self.params.detect_pitch
            && self.pitch_count >= 0
            && (self.flags[(self.pitch_count as usize) & mask] & flag::PITCH_CANDIDATE) != 0
            && self.repairs_pending > 4
        {
            protect_pitched_repairs(
                &mut self.lo.samples,
                &self.lo.original,
                &mut self.hi.samples,
                &self.hi.original,
                &mut self.flags,
                mask,
                self.buffer_length as isize,
                self.coeff_length,
                self.frame_count,
                &mut self.pitch_count,
                &mut self.repairs_pending,
                &mut self.protected_total,
                16,
                true,
                &mut self.lo.fwd_excit,
                &mut self.lo.bck_excit,
                &mut self.lo.block_refls,
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.hi.block_refls,
                &mut self.scratch,
            );
        }

        if output_pos >= 0 {
            let out_idx = (output_pos as usize) & mask;
            let next_out_idx = ((output_pos + 1) as usize) & mask;
            let flag = self.flags[out_idx];
            let next_flag = self.flags[next_out_idx];

            if (flag & flag::REPAIRED) != 0 {
                self.declick_samples += 2;
            }
            if (flag & flag::PROTECTED) != 0 {
                self.protected_samples += 2;
            }
            if (flag & flag::REPAIRED) != 0 && (next_flag & flag::REPAIRED) == 0 {
                self.declick_events += 1;
            }

            // Unreverted DeClick repair interval passes output.
            // Only REPAIRED intervals are counted in apply_repair(), so only REPAIRED intervals are
            // checked out here (DeCrackle commits never incremented the counter).
            if (flag & flag::REPAIRED) != 0 && (next_flag & flag::REPAIRED) == 0 {
                self.repairs_pending = self.repairs_pending.saturating_sub(1);
            }

            let idx0 = out_idx;
            let idx1 = next_out_idx;
            let (out_e, out_o) = synthesize_zero_up(
                self.lo.samples[idx0],
                self.hi.samples[idx0],
                self.lo.samples[idx1],
                self.hi.samples[idx1],
            );
            (Some(out_e), Some(out_o))
        } else {
            (None, None)
        }
    }

    /// Forward sliding-window declick scanner (engine doc 5)
    pub fn scan_forward(&mut self) {
        let mask = self.buffer_mask;
        let mut lo_onset: isize = 0;
        let mut lo_end: isize = 0;
        let mut hi_onset: isize = 0;
        let mut hi_end: isize = 0;
        let mut lo_post_ref: f64 = 0.0;
        let mut hi_post_ref: f64 = 0.0;
        let mut lo_onset_ref: f64 = 0.0;
        let mut hi_onset_ref: f64 = 0.0;
        let mut lo_in_click = false;
        let mut hi_in_click = false;
        let mut idle = true;

        while self.curr_offset > self.min_offset || !idle {
            let mut pos = self.frame_count - self.curr_offset;
            if self.last_repair < self.loop_total {
                let back = pos - self.last_repair + 384;
                self.curr_offset = (self.curr_offset + back).min(self.max_offset);
                self.last_repair = self.loop_total;
                self.sums_valid = false;
            }
            pos = self.frame_count - self.curr_offset;
            let peak_end = pos;
            let post_end = peak_end + (self.derived.windows.shoulder_len as isize);
            let peak_start = peak_end - (self.derived.windows.peak_len as isize);
            let pre_start = peak_start - (self.derived.windows.shoulder_len as isize);

            // 1. Update or initialize O(1) sliding window accumulators
            if self.sums_valid {
                let idx_post_end = (post_end as usize) & mask;
                let idx_peak_end = (peak_end as usize) & mask;
                let idx_peak_start = (peak_start as usize) & mask;
                let idx_pre_start = (pre_start as usize) & mask;

                self.lo.pre_sum += self.lo.fwd_excit[idx_peak_start] - self.lo.fwd_excit[idx_pre_start];
                self.lo.post_sum += self.lo.bck_excit[idx_post_end] - self.lo.bck_excit[idx_peak_end];
                self.lo.peak += self.lo.fwd_excit[idx_peak_end].powi(2) - self.lo.fwd_excit[idx_peak_start].powi(2);

                if self.is_wavelet {
                    self.hi.pre_sum += self.hi.fwd_excit[idx_peak_start] - self.hi.fwd_excit[idx_pre_start];
                    self.hi.post_sum += self.hi.bck_excit[idx_post_end] - self.hi.bck_excit[idx_peak_end];
                    self.hi.peak += self.hi.fwd_excit[idx_peak_end].powi(2) - self.hi.fwd_excit[idx_peak_start].powi(2);
                }
            } else {
                self.lo.pre_sum = 0.0;
                self.lo.post_sum = 0.0;
                self.lo.peak = 0.0;
                let mut p_pre = pre_start;
                let mut p_post = peak_end;
                for _ in 0..self.derived.windows.shoulder_len {
                    p_pre += 1;
                    p_post += 1;
                    self.lo.pre_sum += self.lo.fwd_excit[(p_pre as usize) & mask];
                    self.lo.post_sum += self.lo.bck_excit[(p_post as usize) & mask];
                }
                let mut p_pk = peak_start;
                for _ in 0..self.derived.windows.peak_len {
                    p_pk += 1;
                    let v = self.lo.fwd_excit[(p_pk as usize) & mask];
                    self.lo.peak += v * v;
                }
                if self.is_wavelet {
                    self.hi.pre_sum = 0.0;
                    self.hi.post_sum = 0.0;
                    self.hi.peak = 0.0;
                    let mut hp_pre = pre_start;
                    let mut hp_post = peak_end;
                    for _ in 0..self.derived.windows.shoulder_len {
                        hp_pre += 1;
                        hp_post += 1;
                        self.hi.pre_sum += self.hi.fwd_excit[(hp_pre as usize) & mask];
                        self.hi.post_sum += self.hi.bck_excit[(hp_post as usize) & mask];
                    }
                    let mut hp_pk = peak_start;
                    for _ in 0..self.derived.windows.peak_len {
                        hp_pk += 1;
                        let v = self.hi.fwd_excit[(hp_pk as usize) & mask];
                        self.hi.peak += v * v;
                    }
                }
                self.sums_valid = true;
            }

            // 2. Select threshold sensitivity based on SubChannel hint flag
            if idle {
                if (self.flags[(pos as usize) & mask] & flag::HINT) != 0 {
                    self.detect_factor = self.derived.lower.detect_factor;
                    self.onset_factor = self.derived.lower.onset_factor;
                } else {
                    self.detect_factor = self.derived.normal.detect_factor;
                    self.onset_factor = self.derived.normal.onset_factor;
                }
            }

            let lo_quiet = self.lo.peak <= self.detect_factor * self.lo.pre_sum * self.lo.pre_sum;
            let hi_quiet = !self.is_wavelet || (self.hi.peak <= self.detect_factor * self.hi.pre_sum * self.hi.pre_sum);

            // 3. Lo-Band Onset Search
            // A band whose click has already terminated (lo_end set, analysis pending until the
            // other band also exits) must not re-seed: re-seeding overwrote the pending onset, the
            // analysis then saw only the click's tail, and the click was never repaired. This is
            // the same per-band guard the real-time scanner keeps: a band whose click has ended
            // stays `Ended` until both bands are done.
            // Applied to all four offline scanners (Channel/SubChannel x fwd/bck).
            if !lo_quiet && !lo_in_click && lo_end == 0 {
                lo_in_click = true;
                idle = false;
                lo_onset_ref = self.lo.pre_sum;
                let search_start = pos - (self.derived.windows.peak_len as isize);
                let search_end = search_start + 2 * (self.derived.windows.peak_len as isize);
                lo_onset = search_start;
                while lo_onset < search_end {
                    if self.lo.fwd_excit[(lo_onset as usize) & mask] > self.onset_factor * self.lo.pre_sum {
                        break;
                    }
                    lo_onset += 1;
                }
            }

            // 4. Hi-Band Onset Search (if Wavelet enabled)
            if self.is_wavelet && !hi_quiet && !hi_in_click && hi_end == 0 {
                hi_in_click = true;
                idle = false;
                hi_onset_ref = self.hi.pre_sum;
                let search_start = pos - (self.derived.windows.peak_len as isize);
                let search_end = search_start + 2 * (self.derived.windows.peak_len as isize);
                hi_onset = search_start;
                while hi_onset < search_end {
                    if self.hi.fwd_excit[(hi_onset as usize) & mask] > self.onset_factor * self.hi.pre_sum {
                        break;
                    }
                    hi_onset += 1;
                }
            }

            // 5. Check for Click Termination (or 256-sample max window timeout)
            if lo_in_click && (lo_quiet || pos > lo_onset + CLICK_WINDOW) {
                lo_in_click = false;
                lo_post_ref = self.lo.post_sum;
                lo_end = pos;
            }
            if hi_in_click && (hi_quiet || pos > hi_onset + CLICK_WINDOW) {
                hi_in_click = false;
                hi_post_ref = self.hi.post_sum;
                hi_end = pos;
            }

            // 6. When both bands have exited the click transient, refine trailing edge & repair
            if !idle && !lo_in_click && !hi_in_click {
                idle = true;
                // Enhanced: a sustained defect (gouge) gets a long repair, or Classic's path.
                if self.enhanced {
                    let lo_long = (lo_end > 0).then(|| self.sustained_extent(false, [lo_onset, lo_end], lo_onset_ref, false));
                    let hi_long = (self.is_wavelet && hi_end > 0)
                        .then(|| self.sustained_extent(true, [hi_onset, hi_end], hi_onset_ref, false));
                    if self.try_long_repair(lo_long.flatten(), hi_long.flatten()) {
                        lo_end = 0;
                        hi_end = 0;
                    }
                }
                self.lo.gap[0] = 0;
                self.lo.gap[1] = 0;
                self.hi.gap[0] = 0;
                self.hi.gap[1] = 0;

                if lo_end > 0 {
                    let search_hi = lo_onset.max(lo_end - 1) + 1 + (self.derived.windows.peak_len as isize);
                    let search_lo = (lo_onset + 1).max(search_hi - 2 * (self.derived.windows.peak_len as isize));
                    lo_end = search_hi;
                    while lo_end > search_lo {
                        if self.lo.bck_excit[(lo_end as usize) & mask] > self.onset_factor * lo_post_ref {
                            break;
                        }
                        lo_end -= 1;
                    }
                    refine_click(
                        &mut self.lo,
                        &self.flags,
                        mask,
                        lo_onset,
                        lo_end,
                        self.derived.windows.peak_len as isize,
                        self.derived.normal.threshold,
                        self.derived.lower.threshold,
                        self.is_wavelet,
                        self.params.reverse,
                        self.classic_max_auto_repair,
                        &mut self.scratch,
                    );
                    lo_end = 0;
                }

                if self.is_wavelet && hi_end > 0 {
                    let search_hi = hi_onset.max(hi_end - 1) + 1 + (self.derived.windows.peak_len as isize);
                    let search_lo = (hi_onset + 1).max(search_hi - 2 * (self.derived.windows.peak_len as isize));
                    hi_end = search_hi;
                    while hi_end > search_lo {
                        if self.hi.bck_excit[(hi_end as usize) & mask] > self.onset_factor * hi_post_ref {
                            break;
                        }
                        hi_end -= 1;
                    }
                    refine_click(
                        &mut self.hi,
                        &self.flags,
                        mask,
                        hi_onset,
                        hi_end,
                        self.derived.windows.peak_len as isize,
                        self.derived.normal.threshold,
                        self.derived.lower.threshold,
                        self.is_wavelet,
                        self.params.reverse,
                        self.classic_max_auto_repair,
                        &mut self.scratch,
                    );
                    hi_end = 0;
                }

                // Synchronize Lo and Hi intervals in Wavelet mode
                if self.is_wavelet {
                    let prev_lo_len = self.lo.gap[1] - self.lo.gap[0];
                    let prev_hi_len = self.hi.gap[1] - self.hi.gap[0];
                    let unified = band_spans_to_time_span(self.lo.gap, self.hi.gap, true);
                    let (sync_lo, sync_hi) = time_span_to_band_span(unified, true);
                    self.lo.gap = sync_lo;
                    self.hi.gap = sync_hi;
                    if self.lo.gap[1] - self.lo.gap[0] != prev_lo_len && self.lo.gap[1] > self.lo.gap[0] {
                        fill_gap(
                            &self.lo.samples,
                            mask,
                            self.lo.gap,
                            &mut self.lo.fixed,
                            &mut self.scratch,
                        );
                    }
                    if self.hi.gap[1] - self.hi.gap[0] != prev_hi_len && self.hi.gap[1] > self.hi.gap[0] {
                        fill_gap(
                            &self.hi.samples,
                            mask,
                            self.hi.gap,
                            &mut self.hi.fixed,
                            &mut self.scratch,
                        );
                    }
                }

                // Boundary guard & apply repair
                if self.lo.gap[1] > self.lo.gap[0]
                    && self.lo.gap[0] > self.edge
                    && self.lo.gap[1] < self.number_of_frames - self.edge
                {
                    let adj_mask = flag::repaired_mask(self.is_wavelet);
                    let mut any_new = false;
                    for p in self.lo.gap[0]..self.lo.gap[1] {
                        if (self.flags[(p as usize) & mask] & adj_mask) == 0 {
                            any_new = true;
                            break;
                        }
                    }
                    if any_new {
                        self.apply_repair();
                    }
                }
            }

            self.curr_offset -= 1;
            if self.curr_offset == 0 {
                break;
            }
        }
    }

    /// Backward sliding-window declick scanner (engine doc 5)
    pub fn scan_backward(&mut self) {
        let mask = self.buffer_mask;
        let mut lo_onset: isize = 0;
        let mut lo_end: isize = 0;
        let mut hi_onset: isize = 0;
        let mut hi_end: isize = 0;
        let mut lo_pre_ref: f64 = 0.0;
        let mut hi_pre_ref: f64 = 0.0;
        let mut lo_onset_ref: f64 = 0.0;
        let mut hi_onset_ref: f64 = 0.0;
        let mut lo_in_click = false;
        let mut hi_in_click = false;
        let mut idle = true;

        self.curr_offset = self.min_offset;
        let pos_init = self.frame_count - self.curr_offset;
        let mut max_scan_offset = (self.min_offset + 576 + 384 + (pos_init - 576 - self.last_repair).max(0))
            .min(self.max_offset);
        self.last_repair = self.loop_total;
        self.sums_valid = false;

        while self.curr_offset < max_scan_offset || !idle {
            let pos = self.frame_count - self.curr_offset;
            let peak_start = pos;
            let peak_end = peak_start + (self.derived.windows.peak_len as isize);
            let post_end = peak_end + (self.derived.windows.shoulder_len as isize);
            let pre_start = peak_start - (self.derived.windows.shoulder_len as isize);

            // Window geometry (engine doc 5) (half-open, scanning backward in time):
            //   pre-shoulder [pre_start, peak_start) over fwd_excit, peak [peak_start, peak_end) over bck_excit^2,
            //   post-shoulder [peak_end, post_end) over bck_excit.
            // Moving one sample earlier ADDS the new low edge and REMOVES the old high edge.
            // (Audit fix: the previous update used the forward-scan formula, which adds a sample
            // already inside the window and removes one outside it, so the cached sums drifted
            // away from the true window sums on ~99.8% of reverse-mode scan steps.)
            if self.sums_valid {
                let idx_post_end = (post_end as usize) & mask;
                let idx_peak_end = (peak_end as usize) & mask;
                let idx_peak_start = (peak_start as usize) & mask;
                let idx_pre_start = (pre_start as usize) & mask;

                self.lo.pre_sum += self.lo.fwd_excit[idx_pre_start] - self.lo.fwd_excit[idx_peak_start];
                self.lo.post_sum += self.lo.bck_excit[idx_peak_end] - self.lo.bck_excit[idx_post_end];
                self.lo.peak += self.lo.bck_excit[idx_peak_start].powi(2) - self.lo.bck_excit[idx_peak_end].powi(2);

                if self.is_wavelet {
                    self.hi.pre_sum += self.hi.fwd_excit[idx_pre_start] - self.hi.fwd_excit[idx_peak_start];
                    self.hi.post_sum += self.hi.bck_excit[idx_peak_end] - self.hi.bck_excit[idx_post_end];
                    self.hi.peak += self.hi.bck_excit[idx_peak_start].powi(2) - self.hi.bck_excit[idx_peak_end].powi(2);
                }
            } else {
                self.lo.pre_sum = 0.0;
                self.lo.post_sum = 0.0;
                self.lo.peak = 0.0;
                for p in pre_start..peak_start {
                    self.lo.pre_sum += self.lo.fwd_excit[(p as usize) & mask];
                }
                for p in peak_end..post_end {
                    self.lo.post_sum += self.lo.bck_excit[(p as usize) & mask];
                }
                for p in peak_start..peak_end {
                    let v = self.lo.bck_excit[(p as usize) & mask];
                    self.lo.peak += v * v;
                }
                if self.is_wavelet {
                    self.hi.pre_sum = 0.0;
                    self.hi.post_sum = 0.0;
                    self.hi.peak = 0.0;
                    for p in pre_start..peak_start {
                        self.hi.pre_sum += self.hi.fwd_excit[(p as usize) & mask];
                    }
                    for p in peak_end..post_end {
                        self.hi.post_sum += self.hi.bck_excit[(p as usize) & mask];
                    }
                    for p in peak_start..peak_end {
                        let v = self.hi.bck_excit[(p as usize) & mask];
                        self.hi.peak += v * v;
                    }
                }
                self.sums_valid = true;
            }

            if idle {
                if (self.flags[(pos as usize) & mask] & flag::HINT) != 0 {
                    self.detect_factor = self.derived.lower.detect_factor;
                    self.onset_factor = self.derived.lower.onset_factor;
                } else {
                    self.detect_factor = self.derived.normal.detect_factor;
                    self.onset_factor = self.derived.normal.onset_factor;
                }
            }

            let lo_quiet = self.lo.peak <= self.detect_factor * self.lo.post_sum * self.lo.post_sum;
            let hi_quiet = !self.is_wavelet || (self.hi.peak <= self.detect_factor * self.hi.post_sum * self.hi.post_sum);

            // Onset search backward
            if !lo_quiet && !lo_in_click && lo_onset == 0 {
                lo_in_click = true;
                idle = false;
                lo_onset_ref = self.lo.post_sum;
                let search_start = pos + (self.derived.windows.peak_len as isize);
                let search_end = search_start - 2 * (self.derived.windows.peak_len as isize);
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
                hi_onset_ref = self.hi.post_sum;
                let search_start = pos + (self.derived.windows.peak_len as isize);
                let search_end = search_start - 2 * (self.derived.windows.peak_len as isize);
                hi_end = search_start;
                while hi_end > search_end {
                    if self.hi.bck_excit[(hi_end as usize) & mask] > self.onset_factor * self.hi.post_sum {
                        break;
                    }
                    hi_end -= 1;
                }
            }

            if lo_in_click && (lo_quiet || pos < lo_end - CLICK_WINDOW) {
                lo_in_click = false;
                lo_pre_ref = self.lo.pre_sum;
                lo_onset = pos;
            }
            if hi_in_click && (hi_quiet || pos < hi_end - CLICK_WINDOW) {
                hi_in_click = false;
                hi_pre_ref = self.hi.pre_sum;
                hi_onset = pos;
            }

            if !idle && !lo_in_click && !hi_in_click {
                idle = true;
                // Enhanced: a sustained defect (gouge) gets a long repair, or Classic's path.
                if self.enhanced {
                    let lo_long = (lo_onset > 0).then(|| self.sustained_extent(false, [lo_onset, lo_end], lo_onset_ref, true));
                    let hi_long = (self.is_wavelet && hi_onset > 0)
                        .then(|| self.sustained_extent(true, [hi_onset, hi_end], hi_onset_ref, true));
                    if self.try_long_repair(lo_long.flatten(), hi_long.flatten()) {
                        lo_onset = 0;
                        hi_onset = 0;
                        max_scan_offset = (self.curr_offset + 384).min(self.max_offset);
                        self.sums_valid = false;
                    }
                }
                self.lo.gap[0] = 0;
                self.lo.gap[1] = 0;
                self.hi.gap[0] = 0;
                self.hi.gap[1] = 0;

                if lo_onset > 0 {
                    let search_lo = lo_onset.min(lo_end + 1) - 1 - (self.derived.windows.peak_len as isize);
                    let search_hi = (lo_end - 1).min(search_lo + 2 * (self.derived.windows.peak_len as isize));
                    lo_onset = search_lo;
                    while lo_onset < search_hi {
                        if self.lo.fwd_excit[(lo_onset as usize) & mask] > self.onset_factor * lo_pre_ref {
                            break;
                        }
                        lo_onset += 1;
                    }
                    refine_click(
                        &mut self.lo,
                        &self.flags,
                        mask,
                        lo_onset,
                        lo_end,
                        self.derived.windows.peak_len as isize,
                        self.derived.normal.threshold,
                        self.derived.lower.threshold,
                        self.is_wavelet,
                        self.params.reverse,
                        self.classic_max_auto_repair,
                        &mut self.scratch,
                    );
                    lo_onset = 0;
                }

                if self.is_wavelet && hi_onset > 0 {
                    let search_lo = hi_onset.min(hi_end + 1) - 1 - (self.derived.windows.peak_len as isize);
                    let search_hi = (hi_end - 1).min(search_lo + 2 * (self.derived.windows.peak_len as isize));
                    hi_onset = search_lo;
                    while hi_onset < search_hi {
                        if self.hi.fwd_excit[(hi_onset as usize) & mask] > self.onset_factor * hi_pre_ref {
                            break;
                        }
                        hi_onset += 1;
                    }
                    refine_click(
                        &mut self.hi,
                        &self.flags,
                        mask,
                        hi_onset,
                        hi_end,
                        self.derived.windows.peak_len as isize,
                        self.derived.normal.threshold,
                        self.derived.lower.threshold,
                        self.is_wavelet,
                        self.params.reverse,
                        self.classic_max_auto_repair,
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
                        fill_gap(
                            &self.lo.samples,
                            mask,
                            self.lo.gap,
                            &mut self.lo.fixed,
                            &mut self.scratch,
                        );
                    }
                    if self.hi.gap[1] - self.hi.gap[0] != prev_hi_len && self.hi.gap[1] > self.hi.gap[0] {
                        fill_gap(
                            &self.hi.samples,
                            mask,
                            self.hi.gap,
                            &mut self.hi.fixed,
                            &mut self.scratch,
                        );
                    }
                }

                if self.lo.gap[1] > self.lo.gap[0]
                    && self.lo.gap[0] > self.edge
                    && self.lo.gap[1] < self.number_of_frames - self.edge
                {
                    let adj_mask = flag::repaired_mask(self.is_wavelet);
                    let mut any_new = false;
                    for p in self.lo.gap[0]..self.lo.gap[1] {
                        if (self.flags[(p as usize) & mask] & adj_mask) == 0 {
                            any_new = true;
                            break;
                        }
                    }
                    if any_new {
                        self.apply_repair();
                        max_scan_offset = (self.curr_offset + 384).min(self.max_offset);
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

    /// Enhanced: the extent of a sustained defect whose click Classic's detector has just closed
    /// at `click` = [onset, end). `onset_ref` is the shoulder sum the click was detected against
    /// (the audio before it; after it when scanning in reverse). Walks on from the end (from the
    /// onset when `reverse`) while the running mean of the excitation stays above `LONG_SUSTAIN`
    /// times the detection level, up to the long click window and never past the look-ahead the
    /// edge search and repair need. Returns the extended [onset, end) when the defect runs on at
    /// least two spans beyond where Classic closed it.
    fn sustained_extent(&self, hi_band: bool, click: [isize; 2], onset_ref: f64, reverse: bool) -> Option<[isize; 2]> {
        let band = if hi_band { &self.hi } else { &self.lo };
        let mask = self.buffer_mask;
        let level = LONG_SUSTAIN * self.onset_factor * onset_ref * SUSTAIN_SPAN as f64;
        let span = SUSTAIN_SPAN;
        if !reverse {
            let excit = &band.fwd_excit;
            // Room after the defect: edge margin plus the backward AR model (2 * 512 + 256).
            let limit = (click[0] + self.click_window).min(self.frame_count - 1280 - 64);
            let mut p = click[1];
            let mut sum: f64 = (p - span..p).map(|i| excit[(i as usize) & mask]).sum();
            while p < limit && sum > level {
                sum += excit[(p as usize) & mask] - excit[((p - span) as usize) & mask];
                p += 1;
            }
            (p >= click[1] + 2 * span).then_some([click[0], p])
        } else {
            let excit = &band.bck_excit;
            let limit = click[1] - self.click_window;
            let mut p = click[0];
            let mut sum: f64 = (p..p + span).map(|i| excit[(i as usize) & mask]).sum();
            while p > limit && sum > level {
                p -= 1;
                sum += excit[(p as usize) & mask] - excit[((p + span) as usize) & mask];
            }
            (p <= click[0] - 2 * span).then_some([p, click[1]])
        }
    }

    /// Enhanced: analyses sustained defects (per band) with the long repair limit and makes the
    /// repair if it passes the music gate. Returns false, with nothing changed but the bands'
    /// work buffers, when there is no sustained defect or no repair passes; the click then goes
    /// down Classic's path.
    fn try_long_repair(&mut self, lo: Option<[isize; 2]>, hi: Option<[isize; 2]>) -> bool {
        if lo.is_none() && hi.is_none() {
            return false;
        }
        let mask = self.buffer_mask;
        self.lo.gap = [0, 0];
        self.hi.gap = [0, 0];
        for (hi_band, extent) in [(false, lo), (true, hi)] {
            let Some([onset, end]) = extent else { continue };
            let band = if hi_band { &mut self.hi } else { &mut self.lo };
            refine_click(
                band,
                &self.flags,
                mask,
                onset,
                end,
                self.derived.windows.peak_len as isize,
                self.derived.normal.threshold,
                self.derived.lower.threshold,
                self.is_wavelet,
                self.params.reverse,
                self.max_auto_repair,
                &mut self.scratch,
            );
        }
        if self.is_wavelet {
            let prev_lo_len = self.lo.gap[1] - self.lo.gap[0];
            let prev_hi_len = self.hi.gap[1] - self.hi.gap[0];
            let unified = band_spans_to_time_span(self.lo.gap, self.hi.gap, true);
            let (sync_lo, sync_hi) = time_span_to_band_span(unified, true);
            self.lo.gap = sync_lo;
            self.hi.gap = sync_hi;
            if self.lo.gap[1] - self.lo.gap[0] != prev_lo_len && self.lo.gap[1] > self.lo.gap[0] {
                fill_gap(&self.lo.samples, mask, self.lo.gap, &mut self.lo.fixed, &mut self.scratch);
            }
            if self.hi.gap[1] - self.hi.gap[0] != prev_hi_len && self.hi.gap[1] > self.hi.gap[0] {
                fill_gap(&self.hi.samples, mask, self.hi.gap, &mut self.hi.fixed, &mut self.scratch);
            }
        }
        if self.lo.gap[1] <= self.lo.gap[0]
            || self.lo.gap[0] <= self.edge
            || self.lo.gap[1] >= self.number_of_frames - self.edge
        {
            return false;
        }
        let adj_mask = flag::repaired_mask(self.is_wavelet);
        if (self.lo.gap[0]..self.lo.gap[1]).all(|p| (self.flags[(p as usize) & mask] & adj_mask) != 0) {
            return false;
        }
        let (signal_db, excit_db) = self.long_repair_exposure_db();
        if signal_db < LONG_GATE_SIGNAL_DB || excit_db < LONG_GATE_EXCIT_DB {
            self.long_gated += 1;
            return false;
        }
        self.long_repairs += 1;
        self.long_repair_samples += (self.lo.gap[1] - self.lo.gap[0]) as usize;
        self.long_repair_log.push(self.lo.gap);
        self.apply_repair();
        true
    }

    /// The two gate measures (see `LONG_GATE_SIGNAL_DB`) for the repair in `lo.gap`/`fixed`,
    /// in dB above the louder side.
    fn long_repair_exposure_db(&self) -> (f64, f64) {
        let mask = self.buffer_mask;
        let [start, end] = self.lo.gap;
        let side = self.long_side;
        let both = |lo: f64, hi: f64| lo * lo + if self.is_wavelet { hi * hi } else { 0.0 };
        let signal = |p: isize| {
            let i = (p as usize) & mask;
            both(self.lo.samples[i] as f64, self.hi.samples[i] as f64)
        };
        let removed = (start..end)
            .enumerate()
            .map(|(k, p)| {
                let i = (p as usize) & mask;
                both((self.lo.samples[i] - self.lo.fixed[k]) as f64, (self.hi.samples[i] - self.hi.fixed[k]) as f64)
            })
            .sum::<f64>()
            / (end - start) as f64;
        let before = (start - side..start).map(signal).sum::<f64>() / side as f64;
        let after = (end..end + side).map(signal).sum::<f64>() / side as f64;
        // Excitation: forward before the defect, backward after it, their mean inside it.
        let fwd = |p: isize| both(self.lo.fwd_excit[(p as usize) & mask], self.hi.fwd_excit[(p as usize) & mask]);
        let bck = |p: isize| both(self.lo.bck_excit[(p as usize) & mask], self.hi.bck_excit[(p as usize) & mask]);
        let ex_in = (start..end).map(|p| fwd(p) + bck(p)).sum::<f64>() / (2 * (end - start)) as f64;
        let ex_before = (start - side..start).map(fwd).sum::<f64>() / side as f64;
        let ex_after = (end..end + side).map(bck).sum::<f64>() / side as f64;
        let db = |a: f64, b: f64| 10.0 * (a.max(1.0) / b.max(1.0)).log10();
        (db(removed, before.max(after)), db(ex_in, ex_before.max(ex_after)))
    }

    fn apply_repair(&mut self) {
        let mask = self.buffer_mask;
        let gap_len = (self.lo.gap[1] - self.lo.gap[0]) as usize;
        let flag_bit = flag::repaired_mask(self.is_wavelet);

        // Count the distinct DeClick intervals this repair absorbs (overlapping or directly
        // adjacent existing REPAIRED runs). After the write they form ONE run that the output
        // check-out will decrement once, so the pending counter must net to +1 overall.
        let mut absorbed = 0usize;
        let mut prev_set = false;
        for p in (self.lo.gap[0] - 1)..=self.lo.gap[1] {
            let set = (self.flags[(p as usize) & mask] & flag::REPAIRED) != 0;
            if set && !prev_set {
                absorbed += 1;
            }
            prev_set = set;
        }

        for k in 0..gap_len {
            let ring_k = ((self.lo.gap[0] + (k as isize)) as usize) & mask;
            self.lo.samples[ring_k] = self.lo.fixed[k];
            if self.is_wavelet {
                self.hi.samples[ring_k] = self.hi.fixed[k];
            }
            self.flags[ring_k] |= flag_bit;
        }

        self.repairs_pending = (self.repairs_pending + 1).saturating_sub(absorbed);
        self.last_repair = self.lo.gap[0];
        self.sums_valid = false;

        update_excitation(
            &self.lo.samples,
            self.frame_count,
            self.buffer_length as isize,
            mask,
            self.coeff_length,
            self.lo.gap[0],
            self.lo.gap[1],
            &mut self.lo.fwd_excit,
            &mut self.lo.bck_excit,
            &mut self.lo.block_refls,
            &mut self.scratch.fwd_err,
            &mut self.scratch.bck_err,
            &mut self.scratch.refl,
        );

        if self.is_wavelet {
            update_excitation(
                &self.hi.samples,
                self.frame_count,
                self.buffer_length as isize,
                mask,
                self.coeff_length,
                self.hi.gap[0],
                self.hi.gap[1],
                &mut self.hi.fwd_excit,
                &mut self.hi.bck_excit,
                &mut self.hi.block_refls,
                &mut self.scratch.fwd_err,
                &mut self.scratch.bck_err,
                &mut self.scratch.refl,
            );
        }
    }

    /// Returns the Channel time-domain repair interval [start, end) if an unreverted repair
    /// is at or immediately ahead of the output (used for Stage 2 high-band repair at >48 kHz).
    pub fn upcoming_repair_span(&self) -> Option<[isize; 2]> {
        if self.output_pos() < 0 {
            return None;
        }
        let mask = self.buffer_mask;
        let check_pos = if self.is_wavelet {
            self.output_pos() + 1
        } else {
            self.output_pos()
        };
        if (self.flags[(check_pos as usize) & mask] & flag::ANY_CHANGE) == 0 {
            return None;
        }
        let mut start = check_pos;
        while start > 0 && (self.flags[((start - 1) as usize) & mask] & flag::ANY_CHANGE) != 0 {
            start -= 1;
        }
        let mut end = check_pos + 1;
        while (self.flags[(end as usize) & mask] & flag::ANY_CHANGE) != 0
            && (end - start) <= (self.max_auto_repair as isize)
        {
            end += 1;
        }
        if self.is_wavelet {
            Some(band_spans_to_time_span([start, end], [start, end], true))
        } else {
            Some([start, end])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_passthrough_identity_simple_mode() {
        let mut params = CrParameters::default();
        params.dcl = 0;
        params.dcr = 0;
        params.method = crate::params::Method::Simple;

        let mut channel = Channel::new(params, 4000);
        let n_samples = 4000;
        let mut input = Vec::new();
        for i in 0..n_samples {
            input.push(((i as f64 * 0.1).sin() * 1_000_000.0) as i32);
        }

        let mut output = Vec::new();
        for &s in &input {
            if let Some(out) = channel.process_sample(s) {
                output.push(out);
            }
        }
        // Flush remaining pipeline delay
        while output.len() < n_samples {
            if let Some(out) = channel.process_sample(0) {
                output.push(out);
            }
        }

        // In simple mode with dcl=0 and dcr=0, output is bit-identical to input
        for i in 0..1000 {
            assert_eq!(output[i], input[i], "Mismatch at sample {}", i);
        }
    }
}
