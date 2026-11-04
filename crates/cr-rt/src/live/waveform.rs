//! Waveform columns for the live view: the original input and what the declicker made of it,
//! time-aligned, with the samples it repaired and the ones pitch protection held back.
//!
//! Built on the DSP worker from each output frame paired with the input frame it came from
//! (the input delayed by the processor latency), so the view lines up with what is heard.

use std::collections::VecDeque;

use cr_dsp::params::RtSoundMode;
use cr_dsp::rt::{RtProcessor, SUPER_BUFFER_MASK};

/// Each column covers this much audio (10 ms).
pub const COLUMNS_PER_SEC: u32 = 100;

const FULL_SCALE: f32 = 16_777_216.0;

/// Repair marks the RT channel exports into the processor's super buffer.
use cr_dsp::flag::{PROTECTED as FLAG_PROTECTED, REPAIRED as FLAG_REPAIRED};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveColumn {
    /// Column number from the start of the session: input frame / frames per column.
    pub index: u64,
    /// Peak of the raw input, per channel, 1.0 = full scale.
    pub orig_peak: [f32; 2],
    /// Peak of what was played, per channel: repaired audio (Sound Out), the input again
    /// (Sound In), or the removed clicks (Noise). See `mode`.
    pub out_peak: [f32; 2],
    pub mode: RtSoundMode,
    /// Samples the declicker replaced in this column.
    pub repaired: [u16; 2],
    /// Samples it found but left alone because pitch protection held them back.
    pub protected: [u16; 2],
    /// Input samples at the clip level.
    pub clipped: [u16; 2],
}

/// Repair flags per output frame of an `RtProcessor`, in output order.
///
/// The processor emits output in steps of `decimation_factor` frames, one step per super
/// buffer index; the step's flags sit at that index. After each `process_frame` this reads
/// the flags of any step just taken and hands back the flags of the frame just returned.
pub struct RepairTap {
    pending: VecDeque<[u8; 2]>,
    last_out_count: isize,
}

impl RepairTap {
    pub fn new(p: &RtProcessor) -> Self {
        Self {
            pending: VecDeque::with_capacity(8),
            last_out_count: p.out_count,
        }
    }

    /// Flags (L, R) of the frame `process_frame` just returned.
    pub fn after_frame(&mut self, p: &RtProcessor) -> [u8; 2] {
        if p.out_count != self.last_out_count {
            let idx = p.out_count - 1;
            let flags = if idx >= 0 {
                let i = (idx as usize) & SUPER_BUFFER_MASK;
                [p.l_flags[i], p.r_flags[i]]
            } else {
                [0, 0]
            };
            for _ in 0..p.decimation_factor {
                self.pending.push_back(flags);
            }
            self.last_out_count = p.out_count;
        }
        self.pending.pop_front().unwrap_or([0, 0])
    }
}

/// Pairs output frames with the input they came from and collects 10 ms columns.
pub struct WaveBuilder {
    frames_per_column: u64,
    clip_level: i32,
    /// Output frames before this many are pipeline warm-up (zeros).
    latency: usize,
    /// Input frames not yet matched by an output frame.
    delay: VecDeque<(i32, i32)>,
    /// Absolute input index of `delay.front()`.
    delay_pos: u64,
    current: Option<WaveColumn>,
    peaks: [i32; 4],
    done: Vec<WaveColumn>,
}

impl WaveBuilder {
    pub fn new(sample_rate: u32, clip_level: i32, latency: usize) -> Self {
        Self {
            frames_per_column: (sample_rate / COLUMNS_PER_SEC).max(1) as u64,
            clip_level,
            latency,
            delay: VecDeque::with_capacity(latency + 2048),
            delay_pos: 0,
            current: None,
            peaks: [0; 4],
            done: Vec::new(),
        }
    }

    /// The processor restarted (method change): its pipeline is empty again. Input still in
    /// the delay line will never come out; the next output matches the next input.
    pub fn restart(&mut self, latency: usize) {
        self.delay_pos += self.delay.len() as u64;
        self.delay.clear();
        self.latency = latency;
    }

    /// One processed frame: the input that went in, and the output (with its flags) that came out.
    pub fn push(&mut self, input: (i32, i32), output: (i32, i32), flags: [u8; 2], mode: RtSoundMode) {
        self.delay.push_back(input);
        if self.delay.len() <= self.latency {
            return;
        }
        let orig = self.delay.pop_front().unwrap();
        let pos = self.delay_pos;
        self.delay_pos += 1;

        let index = pos / self.frames_per_column;
        if self.current.map(|c| c.index) != Some(index) {
            self.finish_column();
            self.current = Some(WaveColumn {
                index,
                orig_peak: [0.0; 2],
                out_peak: [0.0; 2],
                mode,
                repaired: [0; 2],
                protected: [0; 2],
                clipped: [0; 2],
            });
        }
        let col = self.current.as_mut().unwrap();
        col.mode = mode;
        let o = [orig.0.saturating_abs(), orig.1.saturating_abs()];
        let q = [output.0.saturating_abs(), output.1.saturating_abs()];
        for c in 0..2 {
            self.peaks[c] = self.peaks[c].max(o[c]);
            self.peaks[2 + c] = self.peaks[2 + c].max(q[c]);
            col.clipped[c] += (o[c] >= self.clip_level) as u16;
            if flags[c] & FLAG_REPAIRED != 0 {
                col.repaired[c] += 1;
            } else if flags[c] & FLAG_PROTECTED != 0 {
                col.protected[c] += 1;
            }
        }
    }

    fn finish_column(&mut self) {
        if let Some(mut col) = self.current.take() {
            for c in 0..2 {
                col.orig_peak[c] = self.peaks[c] as f32 / FULL_SCALE;
                col.out_peak[c] = self.peaks[2 + c] as f32 / FULL_SCALE;
            }
            self.peaks = [0; 4];
            self.done.push(col);
        }
    }

    /// Columns finished since the last call.
    pub fn take(&mut self) -> Vec<WaveColumn> {
        std::mem::take(&mut self.done)
    }
}
