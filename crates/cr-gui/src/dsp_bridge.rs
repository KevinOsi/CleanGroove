//! The one place the GUI turns its settings into DSP work.
//!
//! Render, waveform preview and audition all go through `cr_params`, `thump_spans` and
//! `post_settings`: the offline engine (`cr_dsp::process::Processor`), the needle-thump repair
//! (`cr_dsp::thump`) when switched on, and then the post-declick chain (`cr_dsp::post`), so
//! what you see and hear is what gets rendered. Before this, preview and
//! audition ran the real-time engine, which has no DeCrackle and agreed with the render on
//! only 70-79% of repaired samples.

use std::collections::VecDeque;
use std::sync::Arc;

use cr_dsp::params::{ChannelMode, CrParameters, Method};
use cr_dsp::post::{context_frames, process_block, PostSettings};
use cr_dsp::process::Processor;

use crate::state::{ClickMarker, DeclickMethod, DspTuning};

/// Internal full scale (24-bit PCM << 1).
pub const FULL_SCALE: f32 = 16_777_216.0;

/// Differences at or below this are wavelet round-off, not repairs.
const ROUNDOFF: i32 = 2;

/// Engine parameters for the current GUI settings. Batch processing always repairs
/// automatically (auto_repair 100).
pub fn cr_params(t: &DspTuning) -> CrParameters {
    let mut p = CrParameters::preset_stereo_lp();
    p.dcl = t.declick.min(100) as i32;
    p.dcr = t.decrackle.min(100) as i32;
    p.method = match t.method {
        DeclickMethod::Simple => Method::Simple,
        DeclickMethod::Wavelet => Method::Wavelet,
        DeclickMethod::WaveletX2 => Method::WaveletX2,
    };
    p.detect_pitch = t.pitch_protection;
    p.reverse = t.reverse;
    // Always declick both channels separately: a mono record is summed afterwards by the post
    // chain, once its channels are aligned (see `post_settings`).
    p.mode = ChannelMode::Stereo;
    p
}

/// Post-declick chain for the current GUI settings (identity when the chain is switched off).
pub fn post_settings(t: &DspTuning) -> PostSettings {
    if !t.post_enabled {
        return PostSettings::default();
    }
    PostSettings {
        flip_right_polarity: t.flip_polarity,
        balance_db: if t.balance_on { t.balance_db } else { 0.0 },
        azimuth_us: if t.azimuth_on { t.azimuth_us } else { 0.0 },
        subsonic_hz: t.subsonic_on.then_some(cr_dsp::analysis::SUBSONIC_HZ),
        elliptical_hz: (t.elliptical_on && !t.stereo_to_mono).then_some(t.elliptical_hz),
        mono_sum: t.stereo_to_mono,
        gain_db: 0.0,
    }
}

/// Needle-thump spans to repair: the analysis' spans when the repair is switched on, else none.
pub fn thump_spans(t: &DspTuning) -> Arc<Vec<(usize, usize)>> {
    if t.dethump { Arc::clone(&t.thump_spans) } else { Arc::new(Vec::new()) }
}

/// Everything a render, preview or audition depends on: engine, thump spans, post chain.
pub type RenderSettings = (CrParameters, Arc<Vec<(usize, usize)>>, PostSettings);

pub fn render_settings(t: &DspTuning) -> RenderSettings {
    (cr_params(t), thump_spans(t), post_settings(t))
}

/// Context (frames) needed either side of a block for the thump repair and the post chain.
pub fn block_context(sample_rate: u32, thumps: &[(usize, usize)], post: &PostSettings) -> usize {
    let mut ctx = if post.is_identity() { 0 } else { context_frames(sample_rate) };
    if !thumps.is_empty() {
        ctx = ctx.max(cr_dsp::thump::REPAIR_CONTEXT);
    }
    ctx
}

/// Warm-up audio fed before the first frame of interest, so the detector's running statistics
/// have settled (same pad the auto-tuner uses). Scaled with the processor's decimation.
pub fn warm_up_frames(sample_rate: u32) -> usize {
    let decimation = if sample_rate > 96_000 { 4 } else if sample_rate > 48_000 { 2 } else { 1 };
    8192 * decimation
}

/// Streams repaired audio from any position in a track through the offline engine, one frame
/// per source frame. Starting mid-track it pre-rolls `warm_up_frames` of history.
pub struct AuditionEngine {
    processor: Processor,
    frames: Arc<Vec<(i32, i32)>>,
    next_in: usize,
    next_out: usize,
}

impl AuditionEngine {
    pub fn new(params: CrParameters, sample_rate: u32, frames: Arc<Vec<(i32, i32)>>, start: usize) -> Self {
        let start = start.min(frames.len());
        let first = start.saturating_sub(warm_up_frames(sample_rate));
        let mut engine = Self {
            processor: Processor::new(params, sample_rate, None),
            frames,
            next_in: first,
            next_out: first,
        };
        while engine.next_out < start {
            if engine.next().is_none() {
                break;
            }
        }
        engine
    }

    /// Next frame as (frame index, original, repaired), or None past the end of the track.
    pub fn next(&mut self) -> Option<(usize, (i32, i32), (i32, i32))> {
        let total = self.frames.len();
        if self.next_out >= total {
            return None;
        }
        loop {
            // Past the end, zeros push the last real frames out of the pipeline.
            let input = self.frames.get(self.next_in).copied().unwrap_or((0, 0));
            self.next_in += 1;
            if let Some(repaired) = self.processor.process_frame(input.0, input.1) {
                let idx = self.next_out;
                self.next_out += 1;
                return Some((idx, self.frames[idx], repaired));
            }
        }
    }
}

/// Audition through the whole render path: the declick engine, the thump repair, then the post
/// chain in 1 s blocks with context each side (`block_context`: zero-phase filters and the
/// thump repair's AR models need samples ahead). Yields (frame index, original, declicked and
/// dethumped, final output).
pub struct AuditionStream {
    engine: AuditionEngine,
    thumps: Arc<Vec<(usize, usize)>>,
    post: PostSettings,
    sample_rate: u32,
    /// Declicked frames from `buf_start` on, kept for the post chain's context.
    buf: VecDeque<((i32, i32), (i32, i32))>,
    buf_start: usize,
    /// First frame not yet emitted.
    next_emit: usize,
    ready: VecDeque<(usize, (i32, i32), (i32, i32), (f64, f64))>,
    ended: bool,
}

impl AuditionStream {
    pub fn new(settings: RenderSettings, sample_rate: u32, frames: Arc<Vec<(i32, i32)>>, start: usize) -> Self {
        let (params, thumps, post) = settings;
        let start = start.min(frames.len());
        let first = start.saturating_sub(block_context(sample_rate, &thumps, &post));
        Self {
            engine: AuditionEngine::new(params, sample_rate, frames, first),
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

    /// Next frame as (frame index, original, declicked, output), or None past the end.
    pub fn next(&mut self) -> Option<(usize, (i32, i32), (i32, i32), (f64, f64))> {
        if self.ready.is_empty() {
            self.fill();
        }
        self.ready.pop_front()
    }

    fn pull(&mut self) -> bool {
        match self.engine.next() {
            Some((_, orig, rep)) => {
                self.buf.push_back((orig, rep));
                true
            }
            None => {
                self.ended = true;
                false
            }
        }
    }

    fn fill(&mut self) {
        if self.post.is_identity() && self.thumps.is_empty() {
            while self.buf.len() < 1024 && !self.ended {
                self.pull();
            }
            for (orig, rep) in self.buf.drain(..) {
                let idx = self.next_emit;
                self.next_emit += 1;
                self.ready.push_back((idx, orig, rep, (rep.0 as f64, rep.1 as f64)));
            }
            self.buf_start = self.next_emit;
            return;
        }
        let ctx = block_context(self.sample_rate, &self.thumps, &self.post);
        let block = self.sample_rate as usize; // 1 s
        // Declicked frames up to the end of this block plus the right-hand context.
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
            self.ready.push_back((self.buf_start + k, self.buf[k].0, input[k], o));
        }
        self.next_emit = emit_end;
        // Keep `ctx` frames before the next block as its left-hand context.
        let drop_to = self.next_emit.saturating_sub(ctx).max(self.buf_start);
        for _ in self.buf_start..drop_to {
            self.buf.pop_front();
        }
        self.buf_start = drop_to;
    }
}

/// Linear-interpolating sample-rate converter from the file rate to the output device rate.
/// The DSP runs once per source frame at the file rate; only its output is resampled.
pub struct LinearResampler {
    step: f64,
    t: f64,
    a: Option<(usize, f32, f32)>,
    b: Option<(usize, f32, f32)>,
}

impl LinearResampler {
    pub fn new(source_rate: u32, device_rate: u32) -> Self {
        Self { step: source_rate.max(1) as f64 / device_rate.max(1) as f64, t: 0.0, a: None, b: None }
    }

    /// Forget buffered frames (after a seek or a settings change).
    pub fn reset(&mut self) {
        self.t = 0.0;
        self.a = None;
        self.b = None;
    }

    /// Next output frame as (source frame index, left, right), pulling source frames as needed.
    pub fn next(&mut self, mut pull: impl FnMut() -> Option<(usize, f32, f32)>) -> Option<(usize, f32, f32)> {
        if self.a.is_none() {
            self.a = pull();
            self.b = pull();
            self.t = 0.0;
        }
        let a = self.a?;
        let out = match self.b {
            Some(b) => {
                let t = self.t as f32;
                (a.0, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
            }
            None => a,
        };
        self.t += self.step;
        while self.t >= 1.0 {
            self.t -= 1.0;
            self.a = self.b;
            self.b = if self.a.is_some() { pull() } else { None };
        }
        Some(out)
    }
}

/// Groups changed samples of one channel into repair markers, bridging single unchanged
/// samples (zero crossings inside one repair).
#[derive(Default)]
pub struct RepairRuns {
    channel: usize,
    open: Option<(usize, usize, i32, f32)>, // (start, last changed frame, peak diff, peak original level)
    pub markers: Vec<ClickMarker>,
}

impl RepairRuns {
    pub fn new(channel: usize) -> Self {
        Self { channel, ..Default::default() }
    }

    pub fn push(&mut self, frame: usize, original: i32, repaired: i32) {
        let diff = (original - repaired).abs();
        if diff <= ROUNDOFF {
            if let Some((_, last, _, _)) = self.open {
                if frame > last + 1 {
                    self.close();
                }
            }
            return;
        }
        let level = (original as f32 / FULL_SCALE).abs();
        self.open = Some(match self.open {
            Some((start, last, peak, amp)) if frame <= last + 2 => (start, frame, peak.max(diff), amp.max(level)),
            Some(_) => {
                self.close();
                (frame, frame, diff, level)
            }
            None => (frame, frame, diff, level),
        });
    }

    pub fn finish(mut self) -> Vec<ClickMarker> {
        self.close();
        self.markers
    }

    fn close(&mut self) {
        if let Some((start, last, _, amp)) = self.open.take() {
            self.markers.push(ClickMarker { frame: start, channel: self.channel, length: last + 1 - start, amplitude: amp });
        }
    }
}

/// Merges per-channel markers into one list sorted by frame.
pub fn merge_markers(left: RepairRuns, right: RepairRuns) -> Vec<ClickMarker> {
    let mut all = left.finish();
    all.extend(right.finish());
    all.sort_by_key(|c| c.frame);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning() -> DspTuning {
        DspTuning::default()
    }

    #[test]
    fn test_cr_params_follows_every_setting() {
        let mut t = tuning();
        t.declick = 45;
        t.decrackle = 20;
        t.method = DeclickMethod::Simple;
        t.pitch_protection = false;
        t.reverse = true;
        t.stereo_to_mono = true;
        let p = cr_params(&t);
        assert_eq!(p.dcl, 45);
        assert_eq!(p.dcr, 20);
        assert_eq!(p.method, Method::Simple);
        assert!(!p.detect_pitch);
        assert!(p.reverse);
        // Mono records are summed by the post chain, so the engine always runs in stereo.
        assert_eq!(p.mode, ChannelMode::Stereo);
        assert_eq!(p.auto_repair, 100);
    }

    #[test]
    fn test_post_settings_follow_the_corrections() {
        let mut t = tuning();
        t.flip_polarity = true;
        t.balance_on = true;
        t.balance_db = -0.3;
        t.azimuth_on = true;
        t.azimuth_us = -5.8;
        t.elliptical_hz = 150.0;
        let p = post_settings(&t);
        assert!(p.flip_right_polarity);
        assert_eq!((p.balance_db, p.azimuth_us), (-0.3, -5.8));
        assert_eq!(p.subsonic_hz, Some(18.0));
        assert_eq!(p.elliptical_hz, Some(150.0));
        assert!(!p.mono_sum);

        t.stereo_to_mono = true;
        let p = post_settings(&t);
        assert!(p.mono_sum);
        assert_eq!(p.elliptical_hz, None, "elliptical is moot on a mono sum");

        t.post_enabled = false;
        assert!(post_settings(&t).is_identity());
    }

    #[test]
    fn test_audition_stream_matches_the_render_path() {
        let frames = Arc::new(clicky_track());
        let mut t = tuning();
        t.azimuth_on = true;
        t.azimuth_us = 4.0;
        let (params, post) = (cr_params(&t), post_settings(&t));
        let (declicked, _) = Processor::process_slice_aligned(params.clone(), 44_100, &frames);
        let mut expected = Vec::new();
        cr_dsp::post::run_post(&declicked, 44_100, &post, |b| expected.extend_from_slice(b));

        let mut stream = AuditionStream::new((params, Arc::new(Vec::new()), post), 44_100, Arc::clone(&frames), 0);
        for (i, want) in expected.iter().enumerate().take(80_000) {
            let (idx, orig, _, out) = stream.next().unwrap();
            assert_eq!((idx, orig), (i, frames[i]));
            // Within a few internal units (-130 dBFS): near the end the render's engine passes
            // the last unaligned frames through while the stream flushes with zeros, and the
            // zero-phase filter carries that back through its 1 s of context.
            assert!((out.0 - want.0).abs() < 4.0 && (out.1 - want.1).abs() < 4.0, "frame {i}: {out:?} vs {want:?}");
        }
    }

    #[test]
    fn test_audition_stream_repairs_thumps_like_the_render() {
        // With the thump repair on, the stream's declicked output and final output match the
        // render: declick, repair over the whole file, then the post chain.
        let frames = Arc::new(clicky_track());
        let mut t = tuning();
        t.dethump = true;
        t.thump_spans = Arc::new(vec![(20_000, 20_700), (44_000, 44_900), (70_000, 70_500)]);
        for post_on in [false, true] {
            t.post_enabled = post_on;
            let settings = render_settings(&t);
            let (mut declicked, _) = Processor::process_slice_aligned(settings.0.clone(), 44_100, &frames);
            assert_eq!(cr_dsp::thump::repair_spans(&mut declicked, 0, &settings.1).len(), 3);
            let mut expected = Vec::new();
            cr_dsp::post::run_post(&declicked, 44_100, &settings.2, |b| expected.extend_from_slice(b));

            let mut stream = AuditionStream::new(settings, 44_100, Arc::clone(&frames), 0);
            for (i, want) in expected.iter().enumerate().take(80_000) {
                let (idx, _, rep, out) = stream.next().unwrap();
                assert_eq!((idx, rep), (i, declicked[i]), "post {post_on}");
                assert!((out.0 - want.0).abs() < 4.0 && (out.1 - want.1).abs() < 4.0, "post {post_on}, frame {i}: {out:?} vs {want:?}");
            }
        }
    }

    #[test]
    fn test_audition_stream_seek_lands_on_the_frame() {
        let frames = Arc::new(clicky_track());
        let mut stream = AuditionStream::new(render_settings(&tuning()), 44_100, Arc::clone(&frames), 50_000);
        let got: Vec<usize> = (0..100).map(|_| stream.next().unwrap().0).collect();
        assert_eq!(got, (50_000..50_100).collect::<Vec<_>>());
    }

    /// Two seconds of tones with clicks every 100 ms on alternating channels.
    fn clicky_track() -> Vec<(i32, i32)> {
        (0..88_200)
            .map(|i| {
                let t = i as f64 / 44_100.0;
                let mut l = (2_000_000.0 * (2.0 * std::f64::consts::PI * 330.0 * t).sin()) as i32;
                let mut r = (2_000_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i32;
                if i % 4410 == 2000 {
                    if (i / 4410) % 2 == 0 { l += 6_000_000 } else { r -= 6_000_000 }
                }
                (l, r)
            })
            .collect()
    }

    #[test]
    fn test_audition_engine_matches_render_from_any_start() {
        let frames = Arc::new(clicky_track());
        let params = cr_params(&tuning());
        let (render, _) = Processor::process_slice_aligned(params.clone(), 44_100, &frames);

        // From the start the stream is the render, sample for sample.
        let mut engine = AuditionEngine::new(params.clone(), 44_100, Arc::clone(&frames), 0);
        for (i, expected) in render.iter().enumerate().take(80_000) {
            let (idx, orig, rep) = engine.next().unwrap();
            assert_eq!((idx, orig), (i, frames[i]));
            assert_eq!(rep, *expected, "frame {i}");
        }

        // Seeking to 1 s gives the same frame indices and the same clicks repaired.
        let mut engine = AuditionEngine::new(params, 44_100, Arc::clone(&frames), 44_100);
        let (mut changed, mut agree) = (0, 0);
        for i in 44_100..80_000 {
            let (idx, orig, rep) = engine.next().unwrap();
            assert_eq!((idx, orig), (i, frames[i]));
            let r_changed = rep != orig;
            let g_changed = render[i] != frames[i];
            changed += g_changed as usize;
            agree += (r_changed && g_changed) as usize;
        }
        assert!(changed > 0);
        assert!(agree as f64 >= 0.9 * changed as f64, "seeked stream repaired {agree} of {changed} render repairs");
    }

    #[test]
    fn test_audition_engine_ends_at_track_end() {
        let frames = Arc::new(clicky_track());
        let mut engine = AuditionEngine::new(cr_params(&tuning()), 44_100, Arc::clone(&frames), frames.len() - 10);
        let got: Vec<usize> = std::iter::from_fn(|| engine.next().map(|f| f.0)).collect();
        assert_eq!(got, ((frames.len() - 10)..frames.len()).collect::<Vec<_>>());
    }

    #[test]
    fn test_resampler_consumes_each_source_frame_once() {
        // 44.1 kHz file on a 48 kHz device: every source frame is pulled exactly once, in order.
        let mut r = LinearResampler::new(44_100, 48_000);
        let mut pulled = Vec::new();
        let mut src = 0usize;
        let mut outputs = 0usize;
        while r
            .next(|| {
                if src < 4410 {
                    pulled.push(src);
                    src += 1;
                    Some((src - 1, src as f32, 0.0))
                } else {
                    None
                }
            })
            .is_some()
        {
            outputs += 1;
        }
        assert_eq!(pulled, (0..4410).collect::<Vec<_>>());
        assert!((outputs as i64 - 4800).abs() <= 2, "{outputs} device frames for 0.1 s");
    }

    #[test]
    fn test_resampler_is_identity_at_equal_rates() {
        let mut r = LinearResampler::new(48_000, 48_000);
        let mut src = 0usize;
        for i in 0..100 {
            let (idx, l, _) = r
                .next(|| {
                    src += 1;
                    Some((src - 1, (src - 1) as f32, 0.0))
                })
                .unwrap();
            assert_eq!((idx, l), (i, i as f32));
        }
    }

    #[test]
    fn test_repair_runs_group_and_bridge() {
        let mut runs = RepairRuns::new(1);
        let diffs = [0, 5, 0, 7, 0, 0, 0, 9, 0];
        for (f, d) in diffs.iter().enumerate() {
            runs.push(f, 100 + d, 100);
        }
        let m = runs.finish();
        assert_eq!(m.iter().map(|c| (c.frame, c.length, c.channel)).collect::<Vec<_>>(), vec![(1, 3, 1), (7, 1, 1)]);
    }
}
