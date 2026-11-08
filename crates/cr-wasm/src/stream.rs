//! Repaired audio from any point of a file, frame by frame: the web page's live preview.
//!
//! The same approach as the desktop's audition (cr-gui `dsp_bridge`): the declick engine
//! pre-rolls `warm_up_frames` of history so its running statistics have settled, then the
//! needle-thump repair and the corrections run in one-second blocks with context either side,
//! because their filters need samples ahead.

use std::collections::VecDeque;
use std::sync::Arc;

use cr_dsp::params::CrParameters;
use cr_dsp::post::{context_frames, process_block, PostSettings};
use cr_dsp::process::Processor;

/// Warm-up audio fed before the first frame of interest (the auto-tuner's pad), scaled with the
/// processor's decimation.
pub fn warm_up_frames(sample_rate: u32) -> usize {
    let decimation = if sample_rate > 96_000 { 4 } else if sample_rate > 48_000 { 2 } else { 1 };
    8192 * decimation
}

/// Context needed either side of a block by the thump repair and the corrections.
pub fn block_context(sample_rate: u32, thumps: &[(usize, usize)], post: &PostSettings) -> usize {
    let mut ctx = if post.is_identity() { 0 } else { context_frames(sample_rate) };
    if !thumps.is_empty() {
        ctx = ctx.max(cr_dsp::thump::REPAIR_CONTEXT);
    }
    ctx
}

/// The declick engine from any start frame, one output frame per input frame.
struct Engine {
    processor: Processor,
    frames: Arc<Vec<(i32, i32)>>,
    next_in: usize,
    next_out: usize,
}

impl Engine {
    fn new(params: CrParameters, sample_rate: u32, frames: Arc<Vec<(i32, i32)>>, start: usize) -> Self {
        let start = start.min(frames.len());
        let first = start.saturating_sub(warm_up_frames(sample_rate));
        let mut e = Self { processor: Processor::new(params, sample_rate, None), frames, next_in: first, next_out: first };
        while e.next_out < start {
            if e.next().is_none() {
                break;
            }
        }
        e
    }

    /// (original, declicked) for the next frame, or None past the end.
    fn next(&mut self) -> Option<((i32, i32), (i32, i32))> {
        if self.next_out >= self.frames.len() {
            return None;
        }
        loop {
            // Past the end, zeros push the last real frames out of the pipeline.
            let input = self.frames.get(self.next_in).copied().unwrap_or((0, 0));
            self.next_in += 1;
            if let Some(rep) = self.processor.process_frame(input.0, input.1) {
                let idx = self.next_out;
                self.next_out += 1;
                return Some((self.frames[idx], rep));
            }
        }
    }
}

/// One frame of the stream: original, declicked (and dethumped), and the final output after the
/// corrections (f64, internal scale).
pub type StreamFrame = ((i32, i32), (i32, i32), (f64, f64));

/// Declick, thump repair and corrections from any start frame.
pub struct Stream {
    engine: Engine,
    thumps: Arc<Vec<(usize, usize)>>,
    post: PostSettings,
    sample_rate: u32,
    buf: VecDeque<((i32, i32), (i32, i32))>,
    buf_start: usize,
    next_emit: usize,
    ready: VecDeque<StreamFrame>,
    ended: bool,
}

impl Stream {
    pub fn new(
        params: CrParameters,
        thumps: Arc<Vec<(usize, usize)>>,
        post: PostSettings,
        sample_rate: u32,
        frames: Arc<Vec<(i32, i32)>>,
        start: usize,
    ) -> Self {
        let start = start.min(frames.len());
        let first = start.saturating_sub(block_context(sample_rate, &thumps, &post));
        Self {
            engine: Engine::new(params, sample_rate, frames, first),
            thumps,
            post,
            sample_rate,
            buf: VecDeque::new(),
            buf_start: first,
            next_emit: start,
            ready: VecDeque::new(),
            ended: false,
        }
    }

    pub fn next(&mut self) -> Option<StreamFrame> {
        if self.ready.is_empty() {
            self.fill();
        }
        self.ready.pop_front()
    }

    fn pull(&mut self) {
        match self.engine.next() {
            Some(f) => self.buf.push_back(f),
            None => self.ended = true,
        }
    }

    fn fill(&mut self) {
        if self.post.is_identity() && self.thumps.is_empty() {
            while self.buf.len() < 1024 && !self.ended {
                self.pull();
            }
            for (orig, rep) in self.buf.drain(..) {
                self.ready.push_back((orig, rep, (rep.0 as f64, rep.1 as f64)));
            }
            self.next_emit += self.ready.len();
            self.buf_start = self.next_emit;
            return;
        }
        let ctx = block_context(self.sample_rate, &self.thumps, &self.post);
        let block = self.sample_rate as usize;
        let want_end = self.next_emit + block + ctx;
        while self.buf_start + self.buf.len() < want_end && !self.ended {
            self.pull();
        }
        let have_end = self.buf_start + self.buf.len();
        let emit_end = (self.next_emit + block).min(have_end);
        if emit_end <= self.next_emit {
            return;
        }
        let mut input: Vec<(i32, i32)> = self.buf.iter().map(|f| f.1).collect();
        cr_dsp::thump::repair_spans(&mut input, self.buf_start, &self.thumps);
        let keep = (self.next_emit - self.buf_start)..(emit_end - self.buf_start);
        let out = process_block(&input, self.sample_rate, &self.post, keep.clone());
        for (k, o) in keep.zip(out) {
            self.ready.push_back((self.buf[k].0, input[k], o));
        }
        self.next_emit = emit_end;
        let drop_to = self.next_emit.saturating_sub(ctx).max(self.buf_start);
        for _ in self.buf_start..drop_to {
            self.buf.pop_front();
        }
        self.buf_start = drop_to;
    }
}
