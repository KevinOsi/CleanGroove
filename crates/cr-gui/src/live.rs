//! Live mode: the GUI side of a `cr_rt::LiveSession`. Device choice, monitoring, the
//! recorder and the level meters. The UI thread polls events once per frame.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};
use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_dsp::rt::RtTelemetry;
use std::collections::VecDeque;

use cr_rt::live::engine::{Engine, StreamPlan};
use cr_rt::live::{
    next_take_path, LevelSnapshot, LiveConfig, LiveEvent, LiveSession, StreamInfo, TapeReport,
    TapeState, TapeStatus, WaveColumn, COLUMNS_PER_SEC, DEFAULT_TEMPLATE,
};
use cr_rt::{common_rates, scan_devices, DeviceInfo, DeviceScan};

use crate::state::{AuditionMode, DeclickMethod, DspTuning};

/// Where the user changes a device's sample rate and channel count.
#[cfg(windows)]
pub const DEVICE_FORMAT_HINT: &str =
    "Windows Sound settings (More sound settings > Recording / Playback > the device > Properties > \
     Advanced > Default Format)";
#[cfg(not(windows))]
pub const DEVICE_FORMAT_HINT: &str =
    "the sound server: PipeWire runs devices at its clock rate (48 kHz unless set, e.g. \
     `pw-metadata -n settings 0 clock.force-rate 96000`), and pavucontrol > Configuration picks the \
     card's stereo input profile; or choose the card's hw: device to bypass it";

/// Meter floor; anything quieter shows as empty.
pub const METER_FLOOR_DB: f32 = -60.0;
/// The peak-hold line stays put this long, then falls.
const HOLD_TIME: Duration = Duration::from_secs(2);
/// Bar fall rate, dB per second (peak-programme style: fast up, slow down).
const FALL_DB_PER_SEC: f32 = 24.0;

pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= 0.0 {
        METER_FLOOR_DB
    } else {
        (20.0 * lin.log10()).max(METER_FLOOR_DB)
    }
}

/// The live engine's settings from the shared tuning controls. The RT engine has no
/// DeCrackle, no Reverse and no Wavelet x2: those stay File mode settings.
pub fn rt_params(t: &DspTuning) -> RtParameters {
    RtParameters {
        sensitivity: RtSensitivity::Fine(t.declick.min(100) as u8),
        method: match t.method {
            DeclickMethod::Simple => Method::Simple,
            DeclickMethod::Wavelet | DeclickMethod::WaveletX2 => Method::Wavelet,
        },
        mode: match t.audition_mode {
            AuditionMode::OutClean => RtSoundMode::SoundOut,
            AuditionMode::InBypass => RtSoundMode::SoundIn,
            AuditionMode::NoiseDelta => RtSoundMode::Noise,
        },
        detect_pitch: t.pitch_protection,
        reverse: false,
        parallel: true,
    }
}

/// Bar, peak hold, session maximum and clip light for one meter (two channels).
#[derive(Debug, Clone)]
pub struct Meter {
    /// What the bar shows, dB.
    pub bar_db: [f32; 2],
    pub hold_db: [f32; 2],
    hold_since: [Instant; 2],
    /// Highest peak since the last reset, dB: what the level check reads.
    pub max_db: [f32; 2],
    /// Lit by any clipped sample until the user resets it.
    pub clip: [bool; 2],
    pub clip_count: [u64; 2],
}

impl Default for Meter {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            bar_db: [METER_FLOOR_DB; 2],
            hold_db: [METER_FLOOR_DB; 2],
            hold_since: [now; 2],
            max_db: [METER_FLOOR_DB; 2],
            clip: [false; 2],
            clip_count: [0; 2],
        }
    }
}

impl Meter {
    /// One reading of linear peaks, taken `dt` after the previous one.
    pub fn update(&mut self, peak: [f32; 2], clips: [u32; 2], dt: Duration, now: Instant) {
        let fall = FALL_DB_PER_SEC * dt.as_secs_f32();
        for c in 0..2 {
            let db = lin_to_db(peak[c]);
            self.bar_db[c] = db.max(self.bar_db[c] - fall);
            if db >= self.hold_db[c] {
                self.hold_db[c] = db;
                self.hold_since[c] = now;
            } else if now.duration_since(self.hold_since[c]) > HOLD_TIME {
                self.hold_db[c] = (self.hold_db[c] - fall).max(self.bar_db[c]);
            }
            self.max_db[c] = self.max_db[c].max(db);
            if clips[c] > 0 {
                self.clip[c] = true;
                self.clip_count[c] += clips[c] as u64;
            }
        }
    }

    /// Clears the clip lights, clip counts and maxima.
    pub fn reset(&mut self) {
        self.clip = [false; 2];
        self.clip_count = [0; 2];
        self.max_db = [METER_FLOOR_DB; 2];
    }
}

/// How much live waveform is kept (for longer views and Freeze), seconds.
const WAVE_HISTORY_SECS: usize = 120;
/// View lengths offered, seconds.
pub const WAVE_VIEW_SECS: [u32; 4] = [5, 10, 30, 60];

/// The recent live waveform, newest last.
#[derive(Debug, Clone)]
pub struct LiveWave {
    pub columns: VecDeque<WaveColumn>,
    pub view_secs: u32,
    /// While frozen, the view stays on the column it ended at; columns keep arriving.
    pub frozen_at: Option<u64>,
}

impl Default for LiveWave {
    fn default() -> Self {
        Self {
            columns: VecDeque::with_capacity(WAVE_HISTORY_SECS * COLUMNS_PER_SEC as usize),
            view_secs: 10,
            frozen_at: None,
        }
    }
}

impl LiveWave {
    pub fn push(&mut self, cols: &[WaveColumn]) {
        let cap = WAVE_HISTORY_SECS * COLUMNS_PER_SEC as usize;
        for c in cols {
            if self.columns.len() == cap {
                self.columns.pop_front();
            }
            self.columns.push_back(*c);
        }
        // A frozen view that scrolled out of the history follows the oldest kept column.
        if let (Some(f), Some(first)) = (self.frozen_at, self.columns.front()) {
            let view = (self.view_secs * COLUMNS_PER_SEC) as u64;
            if f < first.index + view {
                self.frozen_at = Some(first.index + view);
            }
        }
    }

    pub fn set_frozen(&mut self, frozen: bool) {
        self.frozen_at = if frozen { self.columns.back().map(|c| c.index) } else { None };
    }

    /// Column index range [start, end] the view shows: `view_secs` ending at the newest
    /// column (or where it was frozen).
    pub fn view_range(&self) -> Option<(u64, u64)> {
        let end = self.frozen_at.or_else(|| self.columns.back().map(|c| c.index))?;
        let len = (self.view_secs * COLUMNS_PER_SEC) as u64;
        Some((end.saturating_sub(len - 1), end))
    }

    /// The kept columns with index in [start, end].
    pub fn slice(&self, start: u64, end: u64) -> impl Iterator<Item = &WaveColumn> {
        // Indices increase but can jump (the DSP restarts on a method change).
        let skip = self.columns.partition_point(|c| c.index < start);
        self.columns.iter().skip(skip).take_while(move |c| c.index <= end)
    }

    pub fn clear(&mut self) {
        self.columns.clear();
        self.frozen_at = None;
    }
}

/// One second of the current take.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TakeSecond {
    /// 10 ms columns with a repair / a held-back click / clipping, out of 100.
    pub repaired: u16,
    pub protected: u16,
    pub clipped: u16,
}

/// Overview of the current (or last) take: repair density per second and pause points.
#[derive(Debug, Clone, Default)]
pub struct TakeTimeline {
    pub seconds: Vec<TakeSecond>,
    /// Take time (s) of each pause join.
    pub pauses: Vec<f64>,
    columns: u64,
}

impl TakeTimeline {
    pub fn add(&mut self, c: &WaveColumn) {
        let sec = (self.columns / COLUMNS_PER_SEC as u64) as usize;
        if self.seconds.len() <= sec {
            self.seconds.resize(sec + 1, TakeSecond::default());
        }
        let s = &mut self.seconds[sec];
        s.repaired += (c.repaired != [0, 0]) as u16;
        s.protected += (c.protected != [0, 0]) as u16;
        s.clipped += (c.clipped != [0, 0]) as u16;
        self.columns += 1;
    }

    pub fn mark_pause(&mut self) {
        let t = self.length_secs();
        if t > 0.0 && self.pauses.last() != Some(&t) {
            self.pauses.push(t);
        }
    }

    pub fn length_secs(&self) -> f64 {
        self.columns as f64 / COLUMNS_PER_SEC as f64
    }
}

/// Signal below this (linear peak) is too quiet to judge L against R.
const MONO_CHECK_MIN_PEAK: f32 = 0.003;
/// L and R identical this long with signal present means the input is one channel copied.
const MONO_CHECK_TIME: Duration = Duration::from_secs(2);

/// Watches for an input whose L and R are bit-identical. Two converter channels always
/// differ by their own noise, so this means the device (or Windows' format setting for it)
/// delivers one channel and copies it to both: a stereo record would be archived as mono.
#[derive(Debug, Clone, Default)]
pub struct MonoCheck {
    identical_for: Duration,
}

impl MonoCheck {
    pub fn update(&mut self, l: &LevelSnapshot, dt: Duration) {
        let loud = l.input_peak[0].max(l.input_peak[1]) >= MONO_CHECK_MIN_PEAK;
        if l.input_frames == 0 || !loud {
            return;
        }
        if l.input_lr_differs == 0 {
            self.identical_for += dt;
        } else {
            self.identical_for = Duration::ZERO;
        }
    }

    pub fn is_mono(&self) -> bool {
        self.identical_for >= MONO_CHECK_TIME
    }
}

pub struct LiveController {
    pub scan: Option<DeviceScan>,
    /// Chosen devices by name; None = the system default.
    pub input: Option<String>,
    pub output: Option<String>,
    pub sample_rate: u32,
    pub session: Option<LiveSession>,
    pub info: Option<StreamInfo>,
    pub input_meter: Meter,
    pub output_meter: Meter,
    pub mono_check: MonoCheck,
    /// Running totals: dropped frames, underruns.
    pub counters: LevelSnapshot,
    pub telemetry: RtTelemetry,
    pub tape: TapeStatus,
    pub wave: LiveWave,
    pub timeline: TakeTimeline,
    /// The take the timeline belongs to.
    timeline_path: Option<PathBuf>,
    pub last_take: Option<TapeReport>,
    pub folder: PathBuf,
    pub take_name: String,
    pub template: String,
    pub monitor_volume: f32,
    pub latency_ms: f64,
    last_levels: Instant,
}

impl Default for LiveController {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveController {
    pub fn new() -> Self {
        let folder = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|h| PathBuf::from(h).join("Music").join("CleanGroove Recordings"))
            .unwrap_or_else(|| PathBuf::from("recordings"));
        Self {
            scan: None,
            input: None,
            output: None,
            sample_rate: 48_000,
            session: None,
            info: None,
            input_meter: Meter::default(),
            output_meter: Meter::default(),
            mono_check: MonoCheck::default(),
            counters: LevelSnapshot::default(),
            telemetry: RtTelemetry::default(),
            tape: TapeStatus::default(),
            last_take: None,
            folder,
            take_name: String::new(),
            template: DEFAULT_TEMPLATE.to_string(),
            monitor_volume: 1.0,
            latency_ms: 0.0,
            last_levels: Instant::now(),
            wave: LiveWave::default(),
            timeline: TakeTimeline::default(),
            timeline_path: None,
        }
    }

    /// Recorder state from the session or from a button. A take with a new file starts a new
    /// timeline; each join the recorder reports is marked. (Status events can arrive just
    /// after a button changed the state, so this goes by the file and the recorder's own
    /// pause count, not by state changes.)
    fn set_tape(&mut self, t: TapeStatus) {
        if t.state != TapeState::Idle && t.path.is_some() && t.path != self.timeline_path {
            self.timeline = TakeTimeline::default();
            self.timeline_path = t.path.clone();
        }
        if t.state != TapeState::Idle && t.path == self.timeline_path {
            while self.timeline.pauses.len() < t.pauses {
                self.timeline.mark_pause();
            }
        }
        self.tape = t;
    }

    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// Lists the devices again. Keeps the chosen ones if still there, else picks the defaults.
    pub fn rescan(&mut self) -> Result<()> {
        let scan = scan_devices()?;
        let keep = |chosen: &Option<String>, list: &[DeviceInfo]| {
            chosen
                .as_ref()
                .filter(|n| list.iter().any(|d| &d.name == *n))
                .cloned()
                .or_else(|| list.iter().find(|d| d.is_default).map(|d| d.name.clone()))
                .or_else(|| list.first().map(|d| d.name.clone()))
        };
        self.input = keep(&self.input, &scan.inputs);
        self.output = keep(&self.output, &scan.outputs);
        self.scan = Some(scan);
        self.fit_rate();
        Ok(())
    }

    pub fn input_info(&self) -> Option<&DeviceInfo> {
        let scan = self.scan.as_ref()?;
        scan.inputs.iter().find(|d| Some(&d.name) == self.input.as_ref())
    }

    pub fn output_info(&self) -> Option<&DeviceInfo> {
        let scan = self.scan.as_ref()?;
        scan.outputs.iter().find(|d| Some(&d.name) == self.output.as_ref())
    }

    /// Rates both chosen devices support. Empty when they share none.
    pub fn rates(&self) -> Vec<u32> {
        match (self.input_info(), self.output_info()) {
            (Some(i), Some(o)) => common_rates(i, o),
            (Some(i), None) => i.sample_rates.clone(),
            _ => Vec::new(),
        }
    }

    /// After a device change: keep the rate if both devices still support it.
    pub fn fit_rate(&mut self) {
        let rates = self.rates();
        if !rates.contains(&self.sample_rate) {
            if let Some(r) = self
                .input_info()
                .and_then(|i| i.default_rate)
                .filter(|r| rates.contains(r))
                .or_else(|| rates.first().copied())
            {
                self.sample_rate = r;
            }
        }
    }

    /// Why the chosen devices cannot start, if so: one is busy, or they share no rate.
    pub fn rate_problem(&self) -> Option<String> {
        let (i, o) = (self.input_info()?, self.output_info()?);
        if let Some(d) = [i, o].into_iter().find(|d| !d.available) {
            return Some(format!(
                "{} is busy: another program has it open. Close that program and press Rescan, \
                 or pick a shared device (PipeWire / system default).",
                d.label
            ));
        }
        if common_rates(i, o).is_empty() {
            Some(format!(
                "Input runs at {}, output at {}: set both to the same rate in {}.",
                rate_list(&i.sample_rates),
                rate_list(&o.sample_rates),
                DEVICE_FORMAT_HINT
            ))
        } else {
            None
        }
    }

    pub fn start(&mut self, tuning: &DspTuning) -> Result<Vec<String>> {
        if self.session.is_some() {
            return Ok(Vec::new());
        }
        let session = LiveSession::start(LiveConfig {
            input_device: self.input.clone(),
            output_device: self.output.clone(),
            sample_rate: self.sample_rate,
            params: rt_params(tuning),
            record: None,
            monitor_volume: self.monitor_volume,
        })?;
        let info = session.info().clone();
        self.latency_ms = info.latency_ms();
        let warnings = info.warnings.clone();
        self.info = Some(info);
        self.session = Some(session);
        self.input_meter = Meter::default();
        self.output_meter = Meter::default();
        self.mono_check = MonoCheck::default();
        self.counters = LevelSnapshot::default();
        self.tape = TapeStatus::default();
        self.wave.clear();
        self.last_levels = Instant::now();
        Ok(warnings)
    }

    /// Stops monitoring; an open take is finished and returned.
    pub fn stop(&mut self) -> Result<Option<TapeReport>> {
        let Some(session) = self.session.take() else {
            return Ok(None);
        };
        let report = session.stop()?;
        self.counters = report.levels;
        self.tape = TapeStatus::default();
        if let Some(ref t) = report.tape {
            self.last_take = Some(t.clone());
        }
        Ok(report.tape)
    }

    /// Reads the session's events; returns messages for the status bar.
    pub fn poll(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        let Some(session) = self.session.as_ref() else {
            return messages;
        };
        let events: Vec<LiveEvent> = session.events().try_iter().collect();
        for ev in events {
            match ev {
                LiveEvent::Levels(l) => {
                    let now = Instant::now();
                    let dt = now.duration_since(self.last_levels);
                    self.last_levels = now;
                    self.input_meter.update(l.input_peak, l.input_clips, dt, now);
                    self.output_meter.update(l.output_peak, [0, 0], dt, now);
                    self.mono_check.update(&l, dt);
                    self.counters = l;
                }
                LiveEvent::Telemetry(t) => self.telemetry = t,
                LiveEvent::Wave(cols) => {
                    self.wave.push(&cols);
                    if self.tape.state == TapeState::Recording {
                        for c in &cols {
                            self.timeline.add(c);
                        }
                    }
                }
                LiveEvent::Tape(t) => self.set_tape(t),
                LiveEvent::TapeStopped(t) => {
                    messages.push(take_summary(&t));
                    self.tape = TapeStatus::default();
                    self.last_take = Some(t);
                }
                LiveEvent::LatencyChanged(ms) => {
                    self.latency_ms = ms;
                    messages.push(format!("Method changed: latency now {:.0} ms", ms));
                }
                LiveEvent::Warning(w) => messages.push(format!("Warning: {}", w)),
                LiveEvent::Error(e) => messages.push(format!("Error: {}", e)),
            }
        }
        messages
    }

    pub fn apply_tuning(&mut self, tuning: &DspTuning) {
        if let Some(s) = self.session.as_mut() {
            let p = rt_params(tuning);
            if *s.params() != p {
                s.set_params(p);
            }
        }
    }

    pub fn set_monitor_volume(&mut self, gain: f32) {
        self.monitor_volume = gain;
        if let Some(s) = self.session.as_ref() {
            s.set_monitor_volume(gain);
        }
    }

    /// Development aid (`cleangroove --live-demo FILE`): runs a file through the live engine
    /// offline and fills the waveform and take timeline as if it had been recorded, so the
    /// live views can be checked without an audio interface. Keeps the last 2 minutes.
    pub fn load_demo(&mut self, frames: &[(i32, i32)], rate: u32, tuning: &DspTuning) -> Result<()> {
        let plan = StreamPlan {
            requested_rate: rate,
            input_rate: rate,
            output_rate: rate,
            input_channels: 2,
            output_channels: 2,
        };
        let mut e = Engine::build(&plan, rt_params(tuning), 1.0, None)?;
        let mut sink = vec![0i32; 4096];
        let mut cols = Vec::new();
        let mut flat = Vec::with_capacity(4096);
        for chunk in frames.chunks(2048) {
            flat.clear();
            flat.extend(chunk.iter().flat_map(|&(l, r)| [l, r]));
            e.input.push_interleaved(&flat, 2, |x| x);
            while e.worker.step() > 0 {}
            cols.extend(e.worker.take_wave());
            e.output.fill_interleaved(&mut sink, 2, |x| x);
        }
        self.wave.clear();
        self.wave.push(&cols);
        self.timeline = TakeTimeline::default();
        for c in &cols {
            self.timeline.add(c);
        }
        Ok(())
    }

    /// The file the next Rec writes to.
    pub fn next_take(&self) -> PathBuf {
        next_take_path(&self.folder, &self.template, &self.take_name, SystemTime::now())
    }

    pub fn record_start(&mut self) -> Result<PathBuf> {
        let session = self.session.as_ref().ok_or_else(|| anyhow!("Start monitoring first"))?;
        let path = self.next_take();
        session.record_start(&path)?;
        self.set_tape(TapeStatus {
            state: TapeState::Recording,
            path: Some(path.clone()),
            ..Default::default()
        });
        Ok(path)
    }

    /// Pause when recording, resume when paused.
    pub fn record_pause_toggle(&mut self) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| anyhow!("Not monitoring"))?;
        match self.tape.state {
            TapeState::Recording => {
                session.record_pause()?;
                self.tape.state = TapeState::Paused;
            }
            TapeState::Paused => {
                session.record_resume()?;
                self.tape.state = TapeState::Recording;
            }
            TapeState::Idle => {}
        }
        Ok(())
    }

    pub fn record_stop(&mut self) -> Result<TapeReport> {
        let session = self.session.as_ref().ok_or_else(|| anyhow!("Not monitoring"))?;
        let report = session.record_stop()?;
        self.tape = TapeStatus::default();
        self.last_take = Some(report.clone());
        Ok(report)
    }
}

fn rate_list(rates: &[u32]) -> String {
    if rates.is_empty() {
        return "no standard rate".to_string();
    }
    rates
        .iter()
        .map(|r| format!("{} kHz", *r as f64 / 1000.0))
        .collect::<Vec<_>>()
        .join(" / ")
}

pub fn take_summary(t: &TapeReport) -> String {
    let secs = t.frames as f64 / t.sample_rate.max(1) as f64;
    let mut s = format!(
        "Recorded {:02}:{:04.1} to {}",
        (secs / 60.0) as u64,
        secs % 60.0,
        t.wav_path.display()
    );
    if !t.pauses.is_empty() {
        s += &format!(" ({} pause(s), Audacity labels written)", t.pauses.len());
    }
    if t.dropped > 0 {
        s += &format!(". WARNING: {} frames were lost", t.dropped);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_maps_to_rt_params() {
        let mut t = DspTuning::default();
        t.declick = 45;
        t.method = DeclickMethod::WaveletX2;
        t.audition_mode = AuditionMode::NoiseDelta;
        t.pitch_protection = false;
        t.decrackle = 30;
        t.reverse = true;
        let p = rt_params(&t);
        assert_eq!(p.sensitivity, RtSensitivity::Fine(45));
        assert_eq!(p.method, Method::Wavelet);
        assert_eq!(p.mode, RtSoundMode::Noise);
        assert!(!p.detect_pitch);
        assert!(!p.reverse, "the RT engine has no reverse scan");

        t.method = DeclickMethod::Simple;
        t.audition_mode = AuditionMode::InBypass;
        let p = rt_params(&t);
        assert_eq!((p.method, p.mode), (Method::Simple, RtSoundMode::SoundIn));
    }

    #[test]
    fn meter_rises_at_once_falls_slowly_and_holds() {
        let t0 = Instant::now();
        let step = Duration::from_millis(50);
        let mut m = Meter::default();
        m.update([0.5, 0.0], [0, 0], step, t0);
        let half = lin_to_db(0.5);
        assert!((m.bar_db[0] - half).abs() < 1e-4);
        assert_eq!(m.bar_db[1], METER_FLOOR_DB);

        // Signal stops: the bar falls 24 dB/s, the hold stays for 2 s.
        m.update([0.0, 0.0], [0, 0], step, t0 + step);
        assert!((m.bar_db[0] - (half - 1.2)).abs() < 1e-3);
        assert_eq!(m.hold_db[0], half);
        let late = t0 + Duration::from_millis(2100);
        m.update([0.0, 0.0], [0, 0], step, late);
        assert!(m.hold_db[0] < half);
        assert_eq!(m.max_db[0], half);
    }

    #[test]
    fn clip_light_latches_until_reset() {
        let now = Instant::now();
        let mut m = Meter::default();
        m.update([1.0, 0.2], [3, 0], Duration::from_millis(50), now);
        m.update([0.1, 0.1], [0, 0], Duration::from_millis(50), now);
        assert_eq!(m.clip, [true, false]);
        assert_eq!(m.clip_count, [3, 0]);
        m.reset();
        assert_eq!(m.clip, [false, false]);
        assert_eq!(m.max_db, [METER_FLOOR_DB; 2]);
    }

    fn col(index: u64, repaired: u16) -> WaveColumn {
        WaveColumn {
            index,
            orig_peak: [0.1; 2],
            out_peak: [0.1; 2],
            mode: RtSoundMode::SoundOut,
            repaired: [repaired, 0],
            protected: [0; 2],
            clipped: [0; 2],
        }
    }

    #[test]
    fn wave_view_follows_newest_and_freezes() {
        let mut w = LiveWave::default();
        w.view_secs = 5;
        let cols: Vec<WaveColumn> = (0..1000).map(|i| col(i, 0)).collect();
        w.push(&cols);
        assert_eq!(w.view_range(), Some((500, 999)));
        assert_eq!(w.slice(500, 999).count(), 500);

        w.set_frozen(true);
        let more: Vec<WaveColumn> = (1000..1300).map(|i| col(i, 0)).collect();
        w.push(&more);
        assert_eq!(w.view_range(), Some((500, 999)), "frozen view stays put");
        w.set_frozen(false);
        assert_eq!(w.view_range(), Some((800, 1299)));
    }

    #[test]
    fn wave_slice_handles_a_gap_and_the_history_limit() {
        let mut w = LiveWave::default();
        // A method change skips the frames inside the old pipeline.
        let mut cols: Vec<WaveColumn> = (0..100).map(|i| col(i, 0)).collect();
        cols.extend((121..200).map(|i| col(i, 0)));
        w.push(&cols);
        let got: Vec<u64> = w.slice(90, 130).map(|c| c.index).collect();
        let want: Vec<u64> = (90..100).chain(121..=130).collect();
        assert_eq!(got, want);

        let cap = WAVE_HISTORY_SECS * COLUMNS_PER_SEC as usize;
        let lots: Vec<WaveColumn> = (200..200 + cap as u64 + 50).map(|i| col(i, 0)).collect();
        w.push(&lots);
        assert_eq!(w.columns.len(), cap);
        assert_eq!(w.columns.back().unwrap().index, 200 + cap as u64 + 49);
    }

    #[test]
    fn take_timeline_bins_seconds_and_marks_reported_pauses() {
        let mut c = LiveController::new();
        let take = PathBuf::from("side1.wav");
        let status = |state, pauses| TapeStatus {
            state,
            path: Some(take.clone()),
            pauses,
            ..Default::default()
        };
        c.set_tape(status(TapeState::Recording, 0));
        for i in 0..250 {
            c.timeline.add(&col(i, (i % 10 == 0) as u16));
        }
        assert_eq!(c.timeline.seconds.len(), 3);
        assert_eq!(c.timeline.seconds[0].repaired, 10);
        assert_eq!(c.timeline.seconds[2].repaired, 5);

        // A late "Recording" status after Pause was pressed must not mark a join...
        c.set_tape(status(TapeState::Paused, 0));
        c.set_tape(status(TapeState::Recording, 0));
        assert!(c.timeline.pauses.is_empty());
        // ...the recorder's own count does.
        c.set_tape(status(TapeState::Recording, 1));
        assert_eq!(c.timeline.pauses, vec![2.5]);

        // A new file starts a new timeline.
        c.set_tape(TapeStatus::default());
        c.set_tape(TapeStatus { state: TapeState::Recording, path: Some("side2.wav".into()), ..Default::default() });
        assert!(c.timeline.seconds.is_empty() && c.timeline.pauses.is_empty());
    }

    #[test]
    fn mono_check_needs_two_seconds_of_identical_signal() {
        let step = Duration::from_millis(50);
        let snap = |differs: u32, peak: f32| LevelSnapshot {
            input_peak: [peak, peak],
            input_frames: 2400,
            input_lr_differs: differs,
            ..Default::default()
        };
        let mut m = MonoCheck::default();
        // Silence between tracks says nothing either way.
        for _ in 0..100 {
            m.update(&snap(0, 0.0), step);
        }
        assert!(!m.is_mono());
        for _ in 0..39 {
            m.update(&snap(0, 0.3), step);
        }
        assert!(!m.is_mono());
        m.update(&snap(0, 0.3), step);
        assert!(m.is_mono());
        // One period of real stereo clears it.
        m.update(&snap(1800, 0.3), step);
        assert!(!m.is_mono());
    }

    #[test]
    fn rates_follow_the_chosen_devices() {
        let dev = |name: &str, rates: &[u32], default: bool| DeviceInfo {
            name: name.to_string(),
            label: name.to_string(),
            available: true,
            is_default: default,
            sample_rates: rates.to_vec(),
            default_rate: rates.first().copied(),
            max_channels: 2,
        };
        let mut c = LiveController::new();
        c.scan = Some(DeviceScan {
            host: "test".into(),
            inputs: vec![dev("USB", &[48_000], true), dev("Line", &[44_100, 96_000], false)],
            outputs: vec![dev("Speakers", &[48_000, 96_000], true)],
        });
        c.input = Some("USB".into());
        c.output = Some("Speakers".into());
        c.sample_rate = 44_100;
        c.fit_rate();
        assert_eq!(c.sample_rate, 48_000);
        assert!(c.rate_problem().is_none());

        c.input = Some("Line".into());
        c.fit_rate();
        assert_eq!(c.rates(), vec![96_000]);
        assert_eq!(c.sample_rate, 96_000);

        c.scan.as_mut().unwrap().outputs[0].sample_rates = vec![48_000];
        assert!(c.rates().is_empty());
        assert!(c.rate_problem().unwrap().contains(DEVICE_FORMAT_HINT));

        // A device another program holds is listed, and says so.
        let s = c.scan.as_mut().unwrap();
        s.inputs[1].available = false;
        s.inputs[1].sample_rates.clear();
        s.inputs[1].label = "USB AUDIO CODEC - USB Audio (direct)".into();
        assert!(c.rate_problem().unwrap().starts_with("USB AUDIO CODEC - USB Audio (direct) is busy"));
    }
}
