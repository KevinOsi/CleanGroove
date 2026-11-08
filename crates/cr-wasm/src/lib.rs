//! CleanGroove's offline engine for the browser.
//!
//! A plain C-style interface, so the page needs no generated glue code. The page decodes the
//! file (the browser does WAV, FLAC and more), copies the two channels into a `Session`, and then:
//! - analyses the record and gets the suggested settings as JSON (`cg_analyze`),
//! - streams repaired audio from any point for live preview (`cg_stream_*`),
//! - previews a stretch with its repairs for the waveform (`cg_range`),
//! - renders the whole file for download (`cg_render_*`).
//!
//! Samples cross the boundary as f32 in [-1, 1] and are converted with the `cr_dsp::params`
//! helpers. The engine is the same code the desktop programs use; nothing here changes what it
//! does. Settings follow the desktop app: both channels are always declicked separately, and a
//! mono record is summed afterwards by the corrections.

pub mod stream;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use cr_dsp::analysis::{AlbumAnalysis, ANALYSIS_DCL, SUBSONIC_HZ};
use cr_dsp::autotune::AutoTuneReport;
use cr_dsp::params::{internal_to_f64, scale_f64_to_internal, ChannelMode, CrParameters, Engine, Method};
use cr_dsp::post::PostSettings;
use cr_dsp::process::Processor;

use stream::Stream;

/// Differences at or below this are wavelet round-off, not repairs.
const ROUNDOFF: i32 = 2;
/// Internal full scale (24-bit PCM << 1).
const FULL_SCALE: f64 = 16_777_216.0;
/// Frames per block of render output handed to the page.
const EMIT_FRAMES: usize = 65_536;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// The page's progress callback: (stage, fraction). Stage 1 analysis, 2 render.
    fn cg_progress(stage: u32, fraction: f32);
    /// The page takes a block of render output: (pass, pointer to interleaved f32, samples).
    fn cg_emit(pass: u32, ptr: *const f32, len: u32);
}

fn progress(stage: u32, fraction: f32) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        cg_progress(stage, fraction)
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (stage, fraction);
}

/// The settings the page shows, as the desktop app holds them.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub declick: i32,
    pub decrackle: i32,
    /// 0 Simple, 1 Wavelet, 2 Wavelet x2.
    pub method: u32,
    pub pitch: bool,
    pub reverse: bool,
    pub enhanced: bool,
    /// Corrections, applied after declicking.
    pub post_on: bool,
    pub flip: bool,
    pub balance_on: bool,
    pub azimuth_on: bool,
    pub subsonic_on: bool,
    pub elliptical_on: bool,
    pub elliptical_hz: f64,
    pub mono_sum: bool,
    /// Needle-thump repair over the series the analysis found.
    pub dethump: bool,
    /// Measured balance and azimuth to correct (from the analysis; the page passes them so a
    /// session holding only a slice of the file corrects the same way).
    pub balance_db: f64,
    pub azimuth_us: f64,
}

/// A loaded file and everything done with it.
pub struct Session {
    sample_rate: u32,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    frames: Arc<Vec<(i32, i32)>>,
    settings: Settings,
    analysis: Option<AlbumAnalysis>,
    report: Option<AutoTuneReport>,
    /// Thump spans given by the page (a session holding a slice has no analysis of its own).
    thumps_given: Option<Vec<(usize, usize)>>,
    thump_buf: Vec<u32>,
    text: String,
    stream: Option<Stream>,
    /// Stream output: original, declicked, final, per channel.
    st: [Vec<f32>; 6],
    range_out: [Vec<f32>; 2],
    range_markers: Vec<u32>,
    render: Option<Render>,
    /// Last render: sample peak dBFS, true peak dBTP, loudness LUFS, album gain dB (NaN without
    /// corrections).
    levels: [f64; 4],
}

struct Render {
    processor: Processor,
    next_in: usize,
    declicked: Vec<(i32, i32)>,
    chunk: Vec<(i32, i32)>,
    out_chunk: Vec<(i32, i32)>,
}

impl Session {
    pub fn new(sample_rate: u32, total: usize) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            in_l: vec![0.0; total],
            in_r: vec![0.0; total],
            frames: Arc::new(Vec::new()),
            settings: Settings { declick: 30, method: 1, pitch: true, post_on: true, elliptical_hz: 100.0, ..Default::default() },
            analysis: None,
            report: None,
            thumps_given: None,
            thump_buf: Vec::new(),
            text: String::new(),
            stream: None,
            st: Default::default(),
            range_out: Default::default(),
            range_markers: Vec::new(),
            render: None,
            levels: [f64::NAN; 4],
        }
    }

    /// Converts the copied-in channels to the engine's integer scale and frees the copies.
    pub fn ready(&mut self) {
        let frames: Vec<(i32, i32)> = self
            .in_l
            .iter()
            .zip(&self.in_r)
            .map(|(&l, &r)| (scale_f64_to_internal(l as f64), scale_f64_to_internal(r as f64)))
            .collect();
        self.frames = Arc::new(frames);
        self.in_l = Vec::new();
        self.in_r = Vec::new();
    }

    pub fn set_settings(&mut self, s: Settings) {
        self.settings = s;
    }

    /// Engine parameters for the current settings (as the desktop's `cr_params`).
    pub fn params(&self) -> CrParameters {
        let s = &self.settings;
        let mut p = CrParameters::preset_stereo_lp();
        p.dcl = s.declick.clamp(0, 100);
        p.dcr = s.decrackle.clamp(0, 100);
        p.method = match s.method {
            0 => Method::Simple,
            2 => Method::WaveletX2,
            _ => Method::Wavelet,
        };
        p.detect_pitch = s.pitch;
        p.reverse = s.reverse;
        p.engine = if s.enhanced { Engine::Enhanced } else { Engine::Classic };
        p.mode = ChannelMode::Stereo;
        p
    }

    /// Corrections for the current settings (identity when switched off).
    pub fn post(&self) -> PostSettings {
        let s = &self.settings;
        if !s.post_on {
            return PostSettings::default();
        }
        PostSettings {
            flip_right_polarity: s.flip,
            balance_db: if s.balance_on { s.balance_db } else { 0.0 },
            azimuth_us: if s.azimuth_on { s.azimuth_us } else { 0.0 },
            subsonic_hz: s.subsonic_on.then_some(SUBSONIC_HZ),
            elliptical_hz: (s.elliptical_on && !s.mono_sum).then_some(s.elliptical_hz),
            mono_sum: s.mono_sum,
            gain_db: 0.0,
        }
    }

    fn thump_spans(&self) -> Arc<Vec<(usize, usize)>> {
        if !self.settings.dethump {
            return Arc::new(Vec::new());
        }
        match (&self.thumps_given, &self.analysis) {
            (Some(t), _) => Arc::new(t.clone()),
            (None, Some(a)) => Arc::new(cr_dsp::thump::thump_spans(&a.thumps, self.sample_rate)),
            _ => Arc::new(Vec::new()),
        }
    }

    /// Thump spans to repair, as frame ranges of this session's audio.
    pub fn set_thumps(&mut self, spans: Vec<(usize, usize)>) {
        self.thumps_given = Some(spans);
    }

    // ---------- analysis ----------

    pub fn analyze(&mut self) -> &str {
        let frames = Arc::clone(&self.frames);
        let never = AtomicBool::new(false);
        let a = cr_dsp::analysis::analyze_album(&frames, self.sample_rate, 2, |f, _| progress(1, f * 0.6), &never)
            .expect("analysis is never cancelled here");
        let r = cr_dsp::autotune::autotune_audio_with_analysis(&frames, self.sample_rate, 2, Some(&a), |f, _| {
            progress(1, 0.6 + f * 0.4)
        });
        self.text = analysis_json(&a, &r);
        self.analysis = Some(a);
        self.report = Some(r);
        &self.text
    }

    // ---------- live preview ----------

    pub fn stream_start(&mut self, start: usize) {
        self.stream = Some(Stream::new(
            self.params(),
            self.thump_spans(),
            self.post(),
            self.sample_rate,
            Arc::clone(&self.frames),
            start,
        ));
    }

    /// Up to `n` more frames of the stream into the stream buffers; returns how many.
    pub fn stream_pull(&mut self, n: usize) -> usize {
        for v in self.st.iter_mut() {
            v.clear();
        }
        let Some(stream) = self.stream.as_mut() else { return 0 };
        for _ in 0..n {
            let Some((o, d, f)) = stream.next() else { break };
            self.st[0].push(internal_to_f64(o.0) as f32);
            self.st[1].push(internal_to_f64(o.1) as f32);
            self.st[2].push(internal_to_f64(d.0) as f32);
            self.st[3].push(internal_to_f64(d.1) as f32);
            self.st[4].push((f.0 / FULL_SCALE) as f32);
            self.st[5].push((f.1 / FULL_SCALE) as f32);
        }
        self.st[0].len()
    }

    // ---------- waveform preview ----------

    /// Renders `start..start + len` into the range buffers and finds its repairs (declicked
    /// against original, as the desktop marks them). Returns the number of markers.
    pub fn range(&mut self, start: usize, len: usize) -> usize {
        let mut s = Stream::new(self.params(), self.thump_spans(), self.post(), self.sample_rate, Arc::clone(&self.frames), start);
        let mut runs = [Runs::new(0), Runs::new(1)];
        self.range_out = Default::default();
        for i in 0..len {
            let Some((o, d, f)) = s.next() else { break };
            runs[0].push(start + i, o.0, d.0);
            runs[1].push(start + i, o.1, d.1);
            self.range_out[0].push((f.0 / FULL_SCALE) as f32);
            self.range_out[1].push((f.1 / FULL_SCALE) as f32);
        }
        let [a, b] = runs;
        let mut m = a.finish();
        m.extend(b.finish());
        m.sort_by_key(|x| x.0);
        self.range_markers = m.into_iter().flat_map(|(f, l, c)| [f as u32, l as u32, c as u32]).collect();
        self.range_markers.len() / 3
    }

    // ---------- full render ----------

    pub fn render_begin(&mut self) {
        let total = self.frames.len();
        self.render = Some(Render {
            processor: Processor::new(self.params(), self.sample_rate, Some(total)),
            next_in: 0,
            declicked: Vec::with_capacity(total),
            chunk: Vec::new(),
            out_chunk: Vec::new(),
        });
    }

    /// Declicks the next `n` input frames; returns frames done so far.
    pub fn render_step(&mut self, n: usize) -> usize {
        let frames = Arc::clone(&self.frames);
        let Some(r) = self.render.as_mut() else { return 0 };
        let end = (r.next_in + n).min(frames.len());
        r.chunk.clear();
        r.chunk.extend_from_slice(&frames[r.next_in..end]);
        let mut out = std::mem::take(&mut r.out_chunk);
        r.processor.process_chunk(&r.chunk, &mut out);
        r.declicked.extend_from_slice(&out);
        r.out_chunk = out;
        r.next_in = end;
        end
    }

    /// Finishes the render: engine delay, then the thump repair and the corrections, in the
    /// command-line tool's order. The output goes to `emit` in blocks of interleaved samples
    /// (left, right, ...) as `emit(pass, block)`, so the whole output is never held here: a long
    /// album would not fit the browser's memory. A pass after the first starts the output over
    /// (the corrections run again with an album gain). Returns the number of frames.
    pub fn render_finish(&mut self, mut emit: impl FnMut(u32, &[f32])) -> usize {
        let total = self.frames.len();
        let Some(mut r) = self.render.take() else { return 0 };
        r.declicked.extend(r.processor.flush());
        // Any frames the engine did not return stay silent, as in the command-line tool.
        r.declicked.resize(total, (0, 0));
        let mut declicked = r.declicked;
        drop(r.processor);
        if let (Some(a), true) = (&self.analysis, self.settings.dethump) {
            cr_dsp::thump::repair_thumps(&mut declicked, self.sample_rate, &a.thumps);
        }
        let mut buf: Vec<f32> = Vec::with_capacity(2 * EMIT_FRAMES);
        let post = self.post();
        if post.is_identity() {
            for block in declicked.chunks(EMIT_FRAMES) {
                buf.clear();
                for &(a, b) in block {
                    buf.push(internal_to_f64(a) as f32);
                    buf.push(internal_to_f64(b) as f32);
                }
                emit(0, &buf);
            }
            self.levels = [f64::NAN; 4];
            return total;
        }
        // The corrections, metered; if they pushed peaks over full scale, run them again with
        // one album gain (no limiting), as the desktop render and the command-line tool do.
        let sr = self.sample_rate;
        let mut run = |pass: u32, settings: &PostSettings| {
            let mut meter = cr_dsp::post::LevelMeter::new(sr);
            cr_dsp::post::run_post(&declicked, sr, settings, |block| {
                meter.feed(block);
                buf.clear();
                for &(a, b) in block {
                    buf.push((a / FULL_SCALE) as f32);
                    buf.push((b / FULL_SCALE) as f32);
                }
                emit(pass, &buf);
            });
            meter.finish()
        };
        let mut report = run(0, &post);
        let gain_db = cr_dsp::post::album_gain_db(&report);
        if gain_db < 0.0 {
            let mut with_gain = post.clone();
            with_gain.gain_db = gain_db;
            report = run(1, &with_gain);
        }
        self.levels = [report.sample_peak_dbfs, report.true_peak_dbtp, report.loudness_lufs, gain_db.min(0.0)];
        total
    }
}

/// Groups changed samples of one channel into repairs, bridging single unchanged samples (zero
/// crossings inside one repair), as the desktop's waveform markers do.
struct Runs {
    channel: usize,
    open: Option<(usize, usize)>,
    done: Vec<(usize, usize, usize)>,
}

impl Runs {
    fn new(channel: usize) -> Self {
        Self { channel, open: None, done: Vec::new() }
    }

    fn push(&mut self, frame: usize, original: i32, repaired: i32) {
        if (original - repaired).abs() <= ROUNDOFF {
            if let Some((_, last)) = self.open {
                if frame > last + 1 {
                    self.close();
                }
            }
            return;
        }
        self.open = Some(match self.open {
            Some((start, last)) if frame <= last + 2 => (start, frame),
            Some(_) => {
                self.close();
                (frame, frame)
            }
            None => (frame, frame),
        });
    }

    fn close(&mut self) {
        if let Some((start, last)) = self.open.take() {
            self.done.push((start, last + 1 - start, self.channel));
        }
    }

    fn finish(mut self) -> Vec<(usize, usize, usize)> {
        self.close();
        self.done
    }
}

// ---------- JSON for the analysis panel ----------

struct Json(String);

impl Json {
    fn key(&mut self, k: &str) {
        if !self.0.ends_with('{') && !self.0.ends_with('[') {
            self.0.push(',');
        }
        self.0.push('"');
        self.0.push_str(k);
        self.0.push_str("\":");
    }
    fn num(&mut self, k: &str, v: f64) {
        self.key(k);
        self.0.push_str(&if v.is_finite() { format!("{v}") } else { "null".to_string() });
    }
    fn bool(&mut self, k: &str, v: bool) {
        self.key(k);
        self.0.push_str(if v { "true" } else { "false" });
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        self.0.push_str(&quote(v));
    }
}

fn quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn list<T>(items: &[T], f: impl Fn(&T) -> String) -> String {
    format!("[{}]", items.iter().map(f).collect::<Vec<_>>().join(","))
}

/// Everything the analysis panel shows, as the desktop's panel shows it.
pub fn analysis_json(a: &AlbumAnalysis, r: &AutoTuneReport) -> String {
    let mut j = Json(String::from("{"));
    j.str("condition", &a.condition.to_string());
    j.num("grade", a.condition as u32 as f64);
    j.bool("estimated", a.condition_estimated);
    j.num("audible_per_sec", a.audible_clicks_per_sec);
    j.num("pops_per_sec", a.loud_pops_per_sec);
    j.num("grade_seconds", a.grade_seconds);
    j.num("long_repairs", a.long_repairs as f64);
    j.num("thump_series", a.thumps.len() as f64);
    j.num("thumps", a.thumps.iter().map(|s| s.events.len()).sum::<usize>() as f64);
    j.num("peak_dbfs", a.peak_dbfs as f64);
    j.num("music_rms_db", a.music_rms_db as f64);
    j.num("clipped", a.clipped_samples as f64);
    j.str(
        "mode",
        match a.mode {
            ChannelMode::Stereo => "stereo",
            ChannelMode::Merge => "merge",
            ChannelMode::Mono => "mono",
        },
    );
    j.num("lr_corr", a.lr_midband_correlation);
    j.num("side_mid", a.side_to_mid_ratio);
    j.num("analysis_dcl", ANALYSIS_DCL as f64);
    j.num("repair_fraction", a.repair_fraction);
    j.key("block_exposed");
    j.0.push_str(&list(&a.block_exposed, |v| v.to_string()));
    j.key("spots");
    j.0.push_str(&list(&a.problem_spots, |s| {
        format!(
            "{{\"start\":{},\"len\":{},\"clicks\":{},\"pops\":{},\"longest_ms\":{:.2},\"level_db\":{:.1},\"edge\":{},\"long\":{}}}",
            s.start_frame,
            s.len_frames,
            s.clicks,
            s.pops,
            s.longest_repair_ms,
            s.level_db,
            s.near_silence,
            s.has_long_repair()
        )
    }));
    let s = &r.recommended;
    j.key("suggested");
    j.0.push('{');
    j.num("declick", s.dcl as f64);
    j.num("decrackle", s.dcr as f64);
    j.num(
        "method",
        match s.method {
            Method::Simple => 0.0,
            Method::Wavelet => 1.0,
            _ => 2.0,
        },
    );
    j.bool("pitch", s.detect_pitch);
    j.bool("reverse", s.reverse);
    j.0.push('}');
    let p = a.suggested_post();
    j.key("post");
    j.0.push('{');
    j.bool("flip", p.flip_right_polarity);
    j.bool("balance_on", p.balance_db != 0.0);
    j.bool("azimuth_on", p.azimuth_us != 0.0);
    j.bool("subsonic_on", p.subsonic_hz.is_some());
    j.bool("elliptical_on", p.elliptical_hz.is_some());
    j.num("elliptical_hz", p.elliptical_hz.unwrap_or(cr_dsp::analysis::ELLIPTICAL_HZ));
    j.bool("mono_sum", p.mono_sum);
    j.0.push('}');
    let c = &a.channels;
    j.num("balance_db", c.balance_db);
    j.num("azimuth_us", c.azimuth_us);
    match r.music_pickup_dcl {
        Some(d) => j.num("pickup_dcl", d as f64),
        None => {
            j.key("pickup_dcl");
            j.0.push_str("null");
        }
    }
    j.key("thump_spans");
    j.0.push_str(&list(&cr_dsp::thump::thump_spans(&a.thumps, a.sample_rate), |(x, y)| format!("{x},{y}")));
    j.key("rationale");
    j.0.push_str(&list(&r.rationale, |l| quote(l)));
    j.0.push('}');
    j.0
}

// ---------- the C interface ----------

fn settings_from(v: &[f64]) -> Settings {
    let b = |i: usize| v.get(i).copied().unwrap_or(0.0) != 0.0;
    let n = |i: usize| v.get(i).copied().unwrap_or(0.0);
    Settings {
        declick: n(0) as i32,
        decrackle: n(1) as i32,
        method: n(2) as u32,
        pitch: b(3),
        reverse: b(4),
        enhanced: b(5),
        post_on: b(6),
        flip: b(7),
        balance_on: b(8),
        azimuth_on: b(9),
        subsonic_on: b(10),
        elliptical_on: b(11),
        elliptical_hz: if n(12) > 0.0 { n(12) } else { 100.0 },
        mono_sum: b(13),
        dethump: b(14),
        balance_db: n(15),
        azimuth_us: n(16),
    }
}

/// Number of values `cg_set_settings` reads (see `settings_from` for their order).
pub const SETTINGS_LEN: usize = 17;

#[no_mangle]
pub extern "C" fn cg_session_new(sample_rate: u32, total: u32) -> *mut Session {
    Box::into_raw(Box::new(Session::new(sample_rate, total as usize)))
}

/// Scratch space for the settings array (`SETTINGS_LEN` f64s), owned by the module.
#[no_mangle]
pub extern "C" fn cg_settings_buffer() -> *mut f64 {
    static mut BUF: [f64; SETTINGS_LEN] = [0.0; SETTINGS_LEN];
    std::ptr::addr_of_mut!(BUF) as *mut f64
}

macro_rules! session_fn {
    ($(#[$m:meta])* fn $name:ident($s:ident $(, $a:ident: $t:ty)*) -> $r:ty $body:block) => {
        $(#[$m])*
        /// # Safety
        /// `s` must come from `cg_session_new` and not be freed.
        #[no_mangle]
        pub unsafe extern "C" fn $name($s: *mut Session $(, $a: $t)*) -> $r {
            let $s = &mut *$s;
            $body
        }
    };
}

session_fn!(fn cg_session_input_l(s) -> *mut f32 { s.in_l.as_mut_ptr() });
session_fn!(fn cg_session_input_r(s) -> *mut f32 { s.in_r.as_mut_ptr() });
session_fn!(fn cg_session_ready(s) -> u32 { s.ready(); s.frames.len() as u32 });
session_fn!(
    /// Reads the settings from `cg_settings_buffer`.
    fn cg_set_settings(s) -> u32 {
        let v = std::slice::from_raw_parts(cg_settings_buffer() as *const f64, SETTINGS_LEN);
        s.set_settings(settings_from(v));
        0
    }
);
session_fn!(fn cg_analyze(s) -> *const u8 { s.analyze().as_ptr() });
session_fn!(
    /// Space for `n` thump spans (start, end pairs of u32), read by `cg_set_thumps`.
    fn cg_thumps_buffer(s, n: u32) -> *mut u32 {
        s.thump_buf = vec![0; 2 * n as usize];
        s.thump_buf.as_mut_ptr()
    }
);
session_fn!(fn cg_set_thumps(s) -> u32 {
    let spans = s.thump_buf.chunks(2).map(|c| (c[0] as usize, c[1] as usize)).collect();
    s.set_thumps(spans);
    0
});
session_fn!(fn cg_text_len(s) -> u32 { s.text.len() as u32 });
session_fn!(fn cg_stream_start(s, start: u32) -> u32 { s.stream_start(start as usize); 0 });
session_fn!(fn cg_stream_pull(s, n: u32) -> u32 { s.stream_pull(n as usize) as u32 });
session_fn!(fn cg_stream_buf(s, i: u32) -> *const f32 { s.st[(i as usize).min(5)].as_ptr() });
session_fn!(fn cg_range(s, start: u32, len: u32) -> u32 { s.range(start as usize, len as usize) as u32 });
session_fn!(fn cg_range_out(s, ch: u32) -> *const f32 { s.range_out[(ch as usize).min(1)].as_ptr() });
session_fn!(fn cg_range_len(s) -> u32 { s.range_out[0].len() as u32 });
session_fn!(fn cg_range_markers(s) -> *const u32 { s.range_markers.as_ptr() });
session_fn!(fn cg_render_begin(s) -> u32 { s.render_begin(); 0 });
session_fn!(fn cg_render_step(s, n: u32) -> u32 { s.render_step(n as usize) as u32 });
session_fn!(fn cg_render_finish(s) -> u32 {
    s.render_finish(|pass, block| {
        #[cfg(target_arch = "wasm32")]
        unsafe {
            cg_emit(pass, block.as_ptr(), block.len() as u32)
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (pass, block);
    }) as u32
});
session_fn!(fn cg_render_levels(s) -> *const f64 { s.levels.as_ptr() });

/// # Safety
/// `s` must come from `cg_session_new`; it must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn cg_session_free(s: *mut Session) {
    drop(Box::from_raw(s));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tones over a little noise (on a bare sine the engine can return repairs far over full
    /// scale, an engine matter reported separately), with clicks.
    fn test_signal(sr: u32, n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        let mut seed: u32 = 12345;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f64 / (1u32 << 24) as f64 - 0.5
        };
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let tone = 0.2 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.1 * (2.0 * std::f64::consts::PI * 1_234.0 * t).sin();
            let mut a = tone + 0.003 * noise();
            let mut b = 0.8 * tone + 0.003 * noise();
            if i % 7_919 == 100 {
                a += 0.4; // a click
                b += 0.3;
            }
            l.push(a as f32);
            r.push(b as f32);
        }
        (l, r)
    }

    fn session(sr: u32, l: &[f32], r: &[f32], post_on: bool) -> Session {
        let mut s = Session::new(sr, l.len());
        s.in_l.copy_from_slice(l);
        s.in_r.copy_from_slice(r);
        s.ready();
        s.set_settings(Settings { declick: 40, method: 1, pitch: true, post_on, subsonic_on: post_on, elliptical_hz: 100.0, ..Default::default() });
        s
    }

    /// Renders the whole session as the page receives it: blocks, with a later pass starting over.
    fn render_all(s: &mut Session, step: usize) -> [Vec<f32>; 2] {
        let n = s.frames.len();
        s.render_begin();
        while s.render_step(step) < n {}
        let mut out: [Vec<f32>; 2] = Default::default();
        let mut pass = 0;
        let total = s.render_finish(|p, block| {
            if p != pass {
                pass = p;
                out = Default::default();
            }
            for f in block.chunks(2) {
                out[0].push(f[0]);
                out[1].push(f[1]);
            }
        });
        assert_eq!(total, n);
        assert_eq!(out[0].len(), n, "every frame emitted once");
        out
    }

    /// The full render returns what the engine returns, frame for frame, and keeps the length.
    #[test]
    fn render_matches_the_engine() {
        let (sr, n) = (44_100, 100_000);
        let (l, r) = test_signal(sr, n);
        let mut s = session(sr, &l, &r, false);
        let out = render_all(&mut s, 30_000);

        let (want, _) = Processor::process_slice_aligned(s.params(), sr, &s.frames);
        for i in 0..n {
            assert_eq!(out[0][i], internal_to_f64(want[i].0) as f32, "left frame {i}");
            assert_eq!(out[1][i], internal_to_f64(want[i].1) as f32, "right frame {i}");
        }
        assert!(out[0].iter().zip(&l).any(|(a, b)| a != b), "the click was repaired");
    }

    /// Corrections that push peaks over full scale run again with an album gain: the second
    /// pass replaces the first, and the output stays within full scale.
    #[test]
    fn render_reruns_with_an_album_gain() {
        let (sr, n) = (44_100, 100_000);
        let (l, r) = test_signal(sr, n);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
        let (l, r): (Vec<f32>, Vec<f32>) = (l.iter().map(|v| v * 0.95 / peak).collect(), r.iter().map(|v| v * 0.95 / peak).collect());
        let mut s = session(sr, &l, &r, true);
        s.settings.balance_on = true;
        s.settings.balance_db = 20.0;
        let out = render_all(&mut s, 65_536);
        assert!(s.levels[3] < 0.0, "an album gain was applied: {:?}", s.levels);
        let got = out[0].iter().chain(&out[1]).fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(got <= 1.0, "peak {got}");
    }

    /// The live preview from a point mid-file matches the full render there: exactly without
    /// corrections, and with them to far below hearing (the desktop's audition works the same
    /// way; its zero-phase filters see a different block edge at the seek point).
    #[test]
    fn stream_matches_the_render_from_any_start() {
        let (sr, n) = (44_100, 200_000);
        let (l, r) = test_signal(sr, n);
        for post_on in [false, true] {
            let mut s = session(sr, &l, &r, post_on);
            let out = render_all(&mut s, 65_536);
            let start = 120_000;
            s.stream_start(start);
            let got = s.stream_pull(50_000);
            assert_eq!(got, 50_000);
            let mut worst = 0.0f32;
            for i in 0..got {
                worst = worst.max((s.st[4][i] - out[0][start + i]).abs()).max((s.st[5][i] - out[1][start + i]).abs());
            }
            eprintln!("post_on {post_on}: worst difference {worst:e} ({:.1} dBFS)", 20.0 * (worst.max(1e-12) as f64).log10());
            // Without corrections: exact. With them: the filters' 1 s context cut at the seek
            // point leaves a residue far below hearing (bound: -100 dBFS).
            let tol = if post_on { 1e-5 } else { 0.0 };
            assert!(worst <= tol, "post_on {post_on}: worst difference {worst}");
        }
    }

    /// Range preview finds the injected clicks in both channels.
    #[test]
    fn range_marks_the_clicks() {
        let (sr, n) = (44_100, 100_000);
        let (l, r) = test_signal(sr, n);
        let mut s = session(sr, &l, &r, false);
        let count = s.range(20_000, 30_000);
        assert!(count >= 2, "found {count}");
        let m = &s.range_markers;
        assert!(m.chunks(3).any(|c| c[2] == 0) && m.chunks(3).any(|c| c[2] == 1));
        assert!(m.chunks(3).all(|c| c[0] >= 20_000 && c[0] < 50_000));
    }

    /// The page previews the view from a slice of the file (warm-up and context before, room for
    /// the look-ahead after): it must give what a session holding the whole file gives.
    #[test]
    fn range_from_a_slice_matches_the_whole_file() {
        let (sr, n) = (44_100, 400_000);
        let (l, r) = test_signal(sr, n);
        for post_on in [false, true] {
            let mut whole = session(sr, &l, &r, post_on);
            let (start, len) = (200_000, 60_000);
            let count = whole.range(start, len);

            let before = stream::warm_up_frames(sr) + sr as usize;
            let after = 2 * sr as usize + 16_384;
            let (a, b) = (start - before, (start + len + after).min(n));
            let mut part = session(sr, &l[a..b], &r[a..b], post_on);
            assert_eq!(part.range(start - a, len), count, "post_on {post_on}");
            assert_eq!(part.range_out, whole.range_out, "post_on {post_on}");
            let shifted: Vec<u32> = part.range_markers.chunks(3).flat_map(|c| [c[0] + a as u32, c[1], c[2]]).collect();
            assert_eq!(shifted, whole.range_markers, "post_on {post_on}");
        }
    }

    #[test]
    fn json_quotes_text() {
        assert_eq!(quote("a \"b\"\n"), "\"a \\\"b\\\"\\n\"");
    }
}
