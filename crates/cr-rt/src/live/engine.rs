//! Backend-free core of a live session: what runs in the input callback, the DSP worker and
//! the output callback. No cpal here, so tests drive it with in-memory buffers.

use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use cr_dsp::params::RtParameters;
use cr_dsp::rt::{RtProcessor, RtTelemetry};
use rtrb::{Consumer, Producer, RingBuffer};

use super::recorder::{Recorder, TapeReport, TapeStatus};
use super::waveform::{RepairTap, WaveBuilder, WaveColumn};

/// Lock-free queues between the callbacks and the DSP worker (~1.36 s at 48 kHz).
pub const RING_FRAMES: usize = 65_536;
/// Output waits for this many processed frames before it starts (~85 ms jitter cushion).
pub const PRE_ROLL_FRAMES: usize = 4096;
/// Above this many queued output frames, one frame per callback is dropped (clock drift).
pub const DRIFT_LIMIT_FRAMES: usize = 16_384;
/// Most frames the DSP worker processes before it checks commands and sends telemetry.
pub const DSP_BATCH_FRAMES: usize = 1024;
/// Raw input queue to the recorder (~11 s at 48 kHz), room for slow disk writes.
pub const CAPTURE_RING_FRAMES: usize = 524_288;
/// Most frames the capture side writes in one go.
const CAPTURE_CHUNK_FRAMES: usize = 65_536;

/// Internal-domain full scale (25-bit signed).
const FULL_SCALE: f64 = 16_777_216.0;

/// What was negotiated with the devices.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPlan {
    pub requested_rate: u32,
    pub input_rate: u32,
    pub output_rate: u32,
    pub input_channels: usize,
    pub output_channels: usize,
}

impl StreamPlan {
    /// The DSP and the tape run at the rate the audio actually arrives at.
    pub fn processing_rate(&self) -> u32 {
        self.input_rate
    }

    /// Things the user should know before trusting what they hear.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.input_rate != self.requested_rate {
            w.push(format!(
                "Input runs at {} Hz, not the requested {} Hz (device default). \
                 Recording uses {} Hz.",
                self.input_rate, self.requested_rate, self.input_rate
            ));
        }
        if self.input_rate != self.output_rate {
            w.push(format!(
                "Input is {} Hz but output is {} Hz: monitoring will play at the wrong speed \
                 and drop out. Pick a rate both devices support. Recording is not affected.",
                self.input_rate, self.output_rate
            ));
        }
        w
    }
}

/// Counters and meters shared by the callbacks, the worker and the session.
#[derive(Debug, Default)]
pub struct Shared {
    input_peak: [AtomicI32; 2],
    input_clips: [AtomicU32; 2],
    input_frames: AtomicU32,
    input_lr_differs: AtomicU32,
    output_peak: [AtomicI32; 2],
    dropped_input: AtomicU64,
    dropped_tape: AtomicU64,
    underruns: AtomicU64,
    drift_drops: AtomicU64,
    monitor_gain_bits: AtomicU32,
}

/// Levels since the previous snapshot, plus running totals of lost frames.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LevelSnapshot {
    /// Input peak per channel, linear, 1.0 = full scale. Raw input, before the DSP.
    pub input_peak: [f32; 2],
    /// Input samples at or above the clip level (same rule as the record analysis).
    pub input_clips: [u32; 2],
    /// Input frames in this period, and how many of them had L different from R. Two ADC
    /// channels always differ by their own noise, so none differing means the source is one
    /// channel copied to both (e.g. the Windows device format set to 1 channel).
    pub input_frames: u32,
    pub input_lr_differs: u32,
    /// Output peak per channel after the DSP and the monitor volume.
    pub output_peak: [f32; 2],
    /// Input frames lost because the DSP queue was full (total since start).
    pub dropped_input: u64,
    /// Input frames lost from the recorder's queue because it was full (total).
    pub dropped_tape: u64,
    /// Output callbacks that ran out of audio (total).
    pub underruns: u64,
    /// Output frames dropped to absorb clock drift (total).
    pub drift_drops: u64,
}

impl Shared {
    pub fn new(monitor_volume: f32) -> Self {
        let s = Self::default();
        s.set_monitor_volume(monitor_volume);
        s
    }

    pub fn set_monitor_volume(&self, gain: f32) {
        self.monitor_gain_bits
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn monitor_volume(&self) -> f32 {
        f32::from_bits(self.monitor_gain_bits.load(Ordering::Relaxed))
    }

    /// Reads the meters and clears them for the next period.
    pub fn take_levels(&self) -> LevelSnapshot {
        let lin = |a: &AtomicI32| (a.swap(0, Ordering::Relaxed) as f64 / FULL_SCALE) as f32;
        LevelSnapshot {
            input_peak: [lin(&self.input_peak[0]), lin(&self.input_peak[1])],
            input_clips: [
                self.input_clips[0].swap(0, Ordering::Relaxed),
                self.input_clips[1].swap(0, Ordering::Relaxed),
            ],
            input_frames: self.input_frames.swap(0, Ordering::Relaxed),
            input_lr_differs: self.input_lr_differs.swap(0, Ordering::Relaxed),
            output_peak: [lin(&self.output_peak[0]), lin(&self.output_peak[1])],
            dropped_input: self.dropped_input.load(Ordering::Relaxed),
            dropped_tape: self.dropped_tape.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            drift_drops: self.drift_drops.load(Ordering::Relaxed),
        }
    }
}

/// Runs in the input callback: raw frames to the DSP queue and the capture queue, input meters.
pub struct InputSide {
    to_dsp: Producer<(i32, i32)>,
    to_capture: Producer<(i32, i32)>,
    shared: Arc<Shared>,
    clip_level: i32,
}

impl InputSide {
    /// `data` is interleaved with `channels` channels; mono is copied to both sides, channels
    /// beyond two are ignored.
    pub fn push_interleaved<T: Copy>(
        &mut self,
        data: &[T],
        channels: usize,
        to_internal: impl Fn(T) -> i32,
    ) {
        let channels = channels.max(1);
        let mut peak = [0i32; 2];
        let mut clips = [0u32; 2];
        let mut dropped = 0u64;
        let mut tape_dropped = 0u64;
        let mut frames = 0u32;
        let mut differs = 0u32;

        for frame in data.chunks_exact(channels) {
            let l = to_internal(frame[0]);
            let r = if channels == 1 { l } else { to_internal(frame[1]) };
            frames += 1;
            differs += (l != r) as u32;
            for (c, v) in [l, r].into_iter().enumerate() {
                let a = v.saturating_abs();
                peak[c] = peak[c].max(a);
                clips[c] += (a >= self.clip_level) as u32;
            }
            if self.to_dsp.push((l, r)).is_err() {
                dropped += 1;
            }
            if self.to_capture.push((l, r)).is_err() {
                tape_dropped += 1;
            }
        }

        let s = &self.shared;
        s.input_frames.fetch_add(frames, Ordering::Relaxed);
        if differs > 0 {
            s.input_lr_differs.fetch_add(differs, Ordering::Relaxed);
        }
        for c in 0..2 {
            s.input_peak[c].fetch_max(peak[c], Ordering::Relaxed);
            if clips[c] > 0 {
                s.input_clips[c].fetch_add(clips[c], Ordering::Relaxed);
            }
        }
        if dropped > 0 {
            s.dropped_input.fetch_add(dropped, Ordering::Relaxed);
        }
        if tape_dropped > 0 {
            s.dropped_tape.fetch_add(tape_dropped, Ordering::Relaxed);
        }
    }
}

/// Runs in the output callback: pre-roll, drift and underrun handling, monitor volume.
pub struct OutputSide {
    from_dsp: Consumer<(i32, i32)>,
    shared: Arc<Shared>,
    primed: bool,
    last: (f32, f32),
}

impl OutputSide {
    /// Fills interleaved `data` with `channels` channels; mono gets the average of L and R,
    /// channels beyond two get silence.
    pub fn fill_interleaved<T: Copy>(
        &mut self,
        data: &mut [T],
        channels: usize,
        from_internal: impl Fn(i32) -> T,
    ) {
        let channels = channels.max(1);
        let silence = from_internal(0);

        if !self.primed {
            if self.from_dsp.slots() >= PRE_ROLL_FRAMES {
                self.primed = true;
            } else {
                data.fill(silence);
                return;
            }
        }

        if self.from_dsp.slots() > DRIFT_LIMIT_FRAMES {
            let _ = self.from_dsp.pop();
            self.shared.drift_drops.fetch_add(1, Ordering::Relaxed);
        }

        let gain = self.shared.monitor_volume();
        let mut peak = [0i32; 2];
        let mut underrun = false;

        for frame in data.chunks_exact_mut(channels) {
            let (l, r) = match self.from_dsp.pop() {
                Ok((l, r)) => {
                    self.last = (l as f32 * gain, r as f32 * gain);
                    self.last
                }
                Err(_) => {
                    // Decay to silence, then re-prime once audio is back.
                    self.last = (self.last.0 * 0.95, self.last.1 * 0.95);
                    underrun = true;
                    self.last
                }
            };
            let (l, r) = (l as i32, r as i32);
            peak[0] = peak[0].max(l.saturating_abs());
            peak[1] = peak[1].max(r.saturating_abs());
            if channels == 1 {
                frame[0] = from_internal(((l as i64 + r as i64) / 2) as i32);
            } else {
                frame[0] = from_internal(l);
                frame[1] = from_internal(r);
                for s in &mut frame[2..] {
                    *s = silence;
                }
            }
        }

        if underrun {
            self.primed = false;
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.output_peak[0].fetch_max(peak[0], Ordering::Relaxed);
        self.shared.output_peak[1].fetch_max(peak[1], Ordering::Relaxed);
    }
}

/// The RT declicker between the two queues.
pub struct DspWorker {
    processor: RtProcessor,
    sample_rate: u32,
    from_input: Consumer<(i32, i32)>,
    to_output: Producer<(i32, i32)>,
    tap: RepairTap,
    wave: WaveBuilder,
}

impl DspWorker {
    /// Processes up to `DSP_BATCH_FRAMES` queued frames; returns how many.
    pub fn step(&mut self) -> usize {
        let n = self
            .from_input
            .slots()
            .min(self.to_output.slots())
            .min(DSP_BATCH_FRAMES);
        for _ in 0..n {
            let Ok((l, r)) = self.from_input.pop() else { break };
            let out = self.processor.process_frame(l, r);
            let flags = self.tap.after_frame(&self.processor);
            self.wave.push((l, r), out, flags, self.processor.params.mode);
            let _ = self.to_output.push(out);
        }
        n
    }

    /// Waveform columns finished since the last call.
    pub fn take_wave(&mut self) -> Vec<WaveColumn> {
        self.wave.take()
    }

    /// Applies new settings. Sensitivity, pitch protection and monitor mode change in place.
    /// A method change (Simple / Wavelet) changes the block length and latency, which the
    /// processor cannot do while running, so it starts a fresh processor: the monitor goes
    /// quiet for one latency period, the recording is not touched. Returns true then.
    pub fn set_params(&mut self, params: RtParameters) -> bool {
        if params.method.is_wavelet() != self.processor.params.method.is_wavelet() {
            self.processor = RtProcessor::new(params, self.sample_rate);
            self.tap = RepairTap::new(&self.processor);
            self.wave.restart(self.latency_frames());
            true
        } else {
            self.processor.set_parameters(params);
            false
        }
    }

    pub fn params(&self) -> &RtParameters {
        &self.processor.params
    }

    pub fn telemetry(&self) -> RtTelemetry {
        self.processor.telemetry
    }

    /// DSP latency in hardware frames (each processor frame is `decimation_factor` frames).
    pub fn latency_frames(&self) -> usize {
        self.processor.proc_delay * self.processor.decimation_factor
    }
}

/// Reads the raw input queue and hands it to the recorder. Runs on its own thread in a
/// session; always drains, recording or not, so the queue never fills while idle.
pub struct Capture {
    from_input: Consumer<(i32, i32)>,
    recorder: Recorder,
    shared: Arc<Shared>,
    chunk: Vec<(i32, i32)>,
}

impl Capture {
    /// Moves queued frames to the recorder; returns how many were read.
    pub fn pump(&mut self) -> Result<usize> {
        self.pump_up_to(CAPTURE_CHUNK_FRAMES)
    }

    fn pump_up_to(&mut self, limit: usize) -> Result<usize> {
        let n = self.from_input.slots().min(limit).min(CAPTURE_CHUNK_FRAMES);
        if n == 0 {
            return Ok(0);
        }
        self.chunk.clear();
        if let Ok(read) = self.from_input.read_chunk(n) {
            self.chunk.extend(read);
        }
        self.recorder.write(&self.chunk)?;
        Ok(n)
    }

    fn dropped(&self) -> u64 {
        self.shared.dropped_tape.load(Ordering::Relaxed)
    }

    pub fn start(&mut self, path: &Path, params: RtParameters) -> Result<()> {
        // Frames queued before Rec was pressed are not part of the take.
        self.discard_queued();
        let dropped = self.dropped();
        self.recorder.start(path, params, dropped)
    }

    /// Writes everything queued at this moment (not what arrives meanwhile).
    fn write_queued(&mut self) -> Result<()> {
        let mut left = self.from_input.slots();
        while left > 0 {
            let n = self.pump_up_to(left)?;
            if n == 0 {
                break;
            }
            left -= n;
        }
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.write_queued()?;
        self.recorder.pause()
    }

    pub fn resume(&mut self) -> Result<()> {
        self.discard_queued();
        self.recorder.resume()
    }

    /// Writes what is queued and finishes the take. The take is closed (WAV finalised) even
    /// if that last write fails; the error is returned then.
    pub fn stop(&mut self) -> Result<TapeReport> {
        let written = self.write_queued();
        let report = self.close_after_error()?;
        written.map(|_| report)
    }

    /// Finishes the take without writing anything more (after a write error).
    pub fn close_after_error(&mut self) -> Result<TapeReport> {
        let dropped = self.dropped();
        self.recorder.stop(dropped)
    }

    /// At the end of a session (input stopped): finishes the take if one is open.
    pub fn finish(&mut self) -> Result<Option<TapeReport>> {
        if !self.recorder.is_active() {
            return Ok(None);
        }
        self.stop().map(Some)
    }

    pub fn is_active(&self) -> bool {
        self.recorder.is_active()
    }

    pub fn status(&self) -> TapeStatus {
        self.recorder.status(self.dropped())
    }

    fn discard_queued(&mut self) {
        let n = self.from_input.slots();
        if let Ok(chunk) = self.from_input.read_chunk(n) {
            chunk.commit_all();
        }
    }
}

/// The halves of a session: input callback, DSP worker, output callback, recorder.
pub struct Engine {
    pub input: InputSide,
    pub worker: DspWorker,
    pub output: OutputSide,
    pub capture: Capture,
    pub shared: Arc<Shared>,
}

impl Engine {
    pub fn build(
        plan: &StreamPlan,
        params: RtParameters,
        monitor_volume: f32,
        input_device: Option<String>,
    ) -> Result<Self> {
        let rate = plan.processing_rate();
        let shared = Arc::new(Shared::new(monitor_volume));
        let clip_level = (cr_dsp::analysis::CLIP_FRACTION * FULL_SCALE) as i32;

        let (in_prod, in_cons) = RingBuffer::<(i32, i32)>::new(RING_FRAMES);
        let (out_prod, out_cons) = RingBuffer::<(i32, i32)>::new(RING_FRAMES);
        let (cap_prod, cap_cons) = RingBuffer::<(i32, i32)>::new(CAPTURE_RING_FRAMES);

        Ok(Self {
            input: InputSide {
                to_dsp: in_prod,
                to_capture: cap_prod,
                shared: shared.clone(),
                clip_level,
            },
            worker: {
                let processor = RtProcessor::new(params, rate);
                let latency = processor.proc_delay * processor.decimation_factor;
                DspWorker {
                    tap: RepairTap::new(&processor),
                    wave: WaveBuilder::new(rate, clip_level, latency),
                    processor,
                    sample_rate: rate,
                    from_input: in_cons,
                    to_output: out_prod,
                }
            },
            output: OutputSide {
                from_dsp: out_cons,
                shared: shared.clone(),
                primed: false,
                last: (0.0, 0.0),
            },
            capture: Capture {
                from_input: cap_cons,
                // Audit fix: the tape used to be opened with the requested rate before the
                // devices were negotiated, so a device that fell back to its default rate
                // produced a WAV tagged at the wrong speed. It now uses the negotiated rate.
                recorder: Recorder::new(rate, input_device),
                shared: shared.clone(),
                chunk: Vec::with_capacity(CAPTURE_CHUNK_FRAMES),
            },
            shared,
        })
    }
}
