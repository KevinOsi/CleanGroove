//! A running live session the GUI or CLI controls while it plays.
//!
//! cpal streams cannot move between threads, so a session thread owns them for their whole
//! life; the caller talks to it through commands and reads events back. The recorder runs on
//! its own capture thread and is started, paused and stopped while monitoring carries on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cr_dsp::params::{RtParameters, RtSoundMode};
use cr_dsp::rt::RtTelemetry;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};

use super::engine::{Capture, DspWorker, Engine, LevelSnapshot, Shared, StreamPlan, PRE_ROLL_FRAMES};
use super::recorder::{TapeReport, TapeStatus};
use super::waveform::WaveColumn;
use crate::backend::cpal_host::{
    begin_timer_resolution, build_input_stream, build_output_stream, end_timer_resolution,
    find_best_config,
};
use crate::backend::devices::find_device;

/// How often the session sends level meters.
const LEVELS_PERIOD: Duration = Duration::from_millis(50);
/// How often the capture thread sends the recorder status.
const TAPE_STATUS_PERIOD: Duration = Duration::from_millis(200);
const EVENT_QUEUE: usize = 512;

#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Device name (exact, or a unique part of it); None = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub sample_rate: u32,
    pub params: RtParameters,
    /// Start recording the raw input (before the DSP) to this WAV right away.
    pub record: Option<PathBuf>,
    /// Monitor volume 0..=1, applied after the DSP; never touches the recording.
    pub monitor_volume: f32,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            sample_rate: 44_100,
            params: RtParameters::default(),
            record: None,
            monitor_volume: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub input_device: String,
    pub output_device: String,
    pub input_format: String,
    pub output_format: String,
    pub plan: StreamPlan,
    /// DSP latency in frames at the processing rate.
    pub dsp_latency_frames: usize,
    pub warnings: Vec<String>,
}

impl StreamInfo {
    /// Input-to-output delay: DSP latency plus the output cushion.
    pub fn latency_ms(&self) -> f64 {
        latency_ms(self.dsp_latency_frames, self.plan.processing_rate())
    }
}

fn latency_ms(dsp_frames: usize, rate: u32) -> f64 {
    (dsp_frames + PRE_ROLL_FRAMES) as f64 * 1000.0 / rate.max(1) as f64
}

#[derive(Debug, Clone)]
pub enum LiveEvent {
    Levels(LevelSnapshot),
    Telemetry(RtTelemetry),
    /// Waveform columns for the live view, about 100 per second, in step with the output.
    Wave(Vec<WaveColumn>),
    /// Recorder state, a few times a second.
    Tape(TapeStatus),
    /// A take ended: by `record_stop`, by the session stopping, or after a write error.
    TapeStopped(TapeReport),
    /// The method changed, so the DSP restarted with a new latency (ms).
    LatencyChanged(f64),
    Warning(String),
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct StopReport {
    /// The take that was still open when the session stopped.
    pub tape: Option<TapeReport>,
    /// Totals at the end (dropped frames, underruns).
    pub levels: LevelSnapshot,
}

enum Command {
    SetParams(RtParameters),
    Stop,
}

enum TapeCommand {
    Start(PathBuf, RtParameters, Sender<Result<()>>),
    Pause(Sender<Result<()>>),
    Resume(Sender<Result<()>>),
    Stop(Sender<Result<TapeReport>>),
}

pub struct LiveSession {
    commands: Sender<Command>,
    tape: Sender<TapeCommand>,
    events: Receiver<LiveEvent>,
    shared: Arc<Shared>,
    info: StreamInfo,
    params: RtParameters,
    thread: Option<JoinHandle<Result<StopReport>>>,
}

struct Ready {
    info: StreamInfo,
    shared: Arc<Shared>,
    tape: Sender<TapeCommand>,
}

impl LiveSession {
    /// Opens the devices and starts monitoring. Returns once audio is flowing, or the error.
    pub fn start(config: LiveConfig) -> Result<Self> {
        let (cmd_tx, cmd_rx) = bounded::<Command>(64);
        let (ev_tx, ev_rx) = bounded::<LiveEvent>(EVENT_QUEUE);
        let (ready_tx, ready_rx) = bounded::<Result<Ready>>(1);
        let params = config.params.clone();

        let thread = thread::Builder::new()
            .name("cr-rt session".into())
            .spawn(move || session_thread(config, cmd_rx, ev_tx, ready_tx))?;

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Self {
                commands: cmd_tx,
                tape: ready.tape,
                events: ev_rx,
                shared: ready.shared,
                info: ready.info,
                params,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("Live session thread ended before the stream started"))
            }
        }
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Meters, telemetry, recorder status, warnings and device errors. Drain it regularly;
    /// when it is full new events are dropped.
    pub fn events(&self) -> &Receiver<LiveEvent> {
        &self.events
    }

    pub fn params(&self) -> &RtParameters {
        &self.params
    }

    /// Sensitivity, pitch protection and mode apply at once. A method change restarts the
    /// DSP (the monitor goes quiet for one latency period, the recording carries on).
    pub fn set_params(&mut self, params: RtParameters) {
        self.params = params.clone();
        let _ = self.commands.send(Command::SetParams(params));
    }

    pub fn set_mode(&mut self, mode: RtSoundMode) {
        let mut p = self.params.clone();
        p.mode = mode;
        self.set_params(p);
    }

    pub fn set_monitor_volume(&self, gain: f32) {
        self.shared.set_monitor_volume(gain);
    }

    pub fn monitor_volume(&self) -> f32 {
        self.shared.monitor_volume()
    }

    /// Starts recording the raw input to a new WAV (never overwrites an existing file).
    /// The current RT settings go into its sidecar.
    pub fn record_start(&self, path: &Path) -> Result<()> {
        let params = self.params.clone();
        self.tape_call(|r| TapeCommand::Start(path.to_path_buf(), params, r))
    }

    /// Stops writing; the take stays open and Resume continues the same file.
    pub fn record_pause(&self) -> Result<()> {
        self.tape_call(TapeCommand::Pause)
    }

    /// Continues a paused take; the join is marked in the sidecar and the label file.
    pub fn record_resume(&self) -> Result<()> {
        self.tape_call(TapeCommand::Resume)
    }

    /// Finishes the take: WAV, sidecar and (if it was paused) Audacity labels.
    pub fn record_stop(&self) -> Result<TapeReport> {
        self.tape_call(TapeCommand::Stop)
    }

    fn tape_call<T>(&self, make: impl FnOnce(Sender<Result<T>>) -> TapeCommand) -> Result<T> {
        let (tx, rx) = bounded(1);
        self.tape
            .send(make(tx))
            .map_err(|_| anyhow!("Recorder is not running"))?;
        rx.recv().map_err(|_| anyhow!("Recorder stopped"))?
    }

    /// Stops the streams, finishes any open take and reports it.
    pub fn stop(mut self) -> Result<StopReport> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<StopReport> {
        let Some(thread) = self.thread.take() else {
            return Ok(StopReport::default());
        };
        let _ = self.commands.send(Command::Stop);
        thread
            .join()
            .map_err(|_| anyhow!("Live session thread panicked"))?
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn session_thread(
    config: LiveConfig,
    commands: Receiver<Command>,
    events: Sender<LiveEvent>,
    ready: Sender<Result<Ready>>,
) -> Result<StopReport> {
    let running = Arc::new(AtomicBool::new(true));
    let capturing = Arc::new(AtomicBool::new(true));
    let (param_tx, param_rx) = bounded::<RtParameters>(64);
    let (tape_tx, tape_rx) = bounded::<TapeCommand>(16);

    let opened = open_streams(&config, &events, &running, param_rx);
    let (info, shared, streams, mut capture, worker) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(StopReport::default());
        }
    };
    if let Some(path) = &config.record {
        if let Err(e) = capture.start(path, config.params.clone()) {
            running.store(false, Ordering::SeqCst);
            let _ = worker.join();
            drop(streams);
            end_timer_resolution();
            let _ = ready.send(Err(e));
            return Ok(StopReport::default());
        }
    }
    let capture_thread = spawn_capture(capture, capturing.clone(), tape_rx, events.clone());

    for w in &info.warnings {
        let _ = events.try_send(LiveEvent::Warning(w.clone()));
    }
    let _ = ready.send(Ok(Ready {
        info,
        shared: shared.clone(),
        tape: tape_tx,
    }));

    let mut next_levels = Instant::now() + LEVELS_PERIOD;
    loop {
        let wait = next_levels.saturating_duration_since(Instant::now());
        match commands.recv_timeout(wait) {
            Ok(Command::SetParams(p)) => {
                let _ = param_tx.send(p);
            }
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_levels {
            let _ = events.try_send(LiveEvent::Levels(shared.take_levels()));
            next_levels += LEVELS_PERIOD;
        }
    }

    // Input first, so the recorder sees every frame that arrived, then the recorder.
    running.store(false, Ordering::SeqCst);
    let _ = worker.join();
    drop(streams);
    end_timer_resolution();
    capturing.store(false, Ordering::SeqCst);
    let tape = capture_thread
        .join()
        .map_err(|_| anyhow!("Recorder thread panicked"))??;

    Ok(StopReport {
        tape,
        levels: shared.take_levels(),
    })
}

type Opened = (
    StreamInfo,
    Arc<Shared>,
    (cpal::Stream, cpal::Stream),
    Capture,
    JoinHandle<()>,
);

fn open_streams(
    config: &LiveConfig,
    events: &Sender<LiveEvent>,
    running: &Arc<AtomicBool>,
    param_rx: Receiver<RtParameters>,
) -> Result<Opened> {
    let input_device = find_device(true, config.input_device.as_deref())?;
    let output_device = find_device(false, config.output_device.as_deref())?;
    let in_cfg = find_best_config(&input_device, config.sample_rate, true)?;
    let out_cfg = find_best_config(&output_device, config.sample_rate, false)?;

    let plan = StreamPlan {
        requested_rate: config.sample_rate,
        input_rate: in_cfg.sample_rate().0,
        output_rate: out_cfg.sample_rate().0,
        input_channels: in_cfg.channels() as usize,
        output_channels: out_cfg.channels() as usize,
    };
    let input_name = input_device.name().unwrap_or_default();

    let Engine {
        input,
        worker,
        output,
        capture,
        shared,
    } = Engine::build(
        &plan,
        config.params.clone(),
        config.monitor_volume,
        Some(input_name.clone()),
    )?;

    let info = StreamInfo {
        input_device: input_name,
        output_device: output_device.name().unwrap_or_default(),
        input_format: format!("{:?}, {} ch", in_cfg.sample_format(), in_cfg.channels()),
        output_format: format!("{:?}, {} ch", out_cfg.sample_format(), out_cfg.channels()),
        dsp_latency_frames: worker.latency_frames(),
        warnings: plan.warnings(),
        plan,
    };

    let err_events = events.clone();
    let in_err = move |e: cpal::StreamError| {
        let _ = err_events.try_send(LiveEvent::Error(format!("Input stream: {}", e)));
    };
    let err_events = events.clone();
    let out_err = move |e: cpal::StreamError| {
        let _ = err_events.try_send(LiveEvent::Error(format!("Output stream: {}", e)));
    };
    let input_stream = build_input_stream(&input_device, &in_cfg, input, in_err)?;
    let output_stream = build_output_stream(&output_device, &out_cfg, output, out_err)?;

    begin_timer_resolution();
    let worker = spawn_worker(
        worker,
        running.clone(),
        param_rx,
        events.clone(),
        info.plan.processing_rate(),
    );

    // If either stream fails to start, stop the worker before reporting the error.
    if let Err(e) = input_stream.play().and_then(|_| output_stream.play()) {
        running.store(false, Ordering::SeqCst);
        let _ = worker.join();
        end_timer_resolution();
        return Err(e.into());
    }

    Ok((info, shared, (input_stream, output_stream), capture, worker))
}

/// The DSP worker: drains the input queue, applies parameter changes between batches,
/// spins briefly, then yields, then sleeps when idle.
fn spawn_worker(
    mut worker: DspWorker,
    running: Arc<AtomicBool>,
    param_rx: Receiver<RtParameters>,
    events: Sender<LiveEvent>,
    rate: u32,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut empty_count = 0usize;
        while running.load(Ordering::Relaxed) {
            while let Ok(p) = param_rx.try_recv() {
                if worker.set_params(p) {
                    let ms = latency_ms(worker.latency_frames(), rate);
                    let _ = events.try_send(LiveEvent::LatencyChanged(ms));
                }
            }

            if worker.step() > 0 {
                empty_count = 0;
                let _ = events.try_send(LiveEvent::Telemetry(worker.telemetry()));
                let wave = worker.take_wave();
                if !wave.is_empty() {
                    let _ = events.try_send(LiveEvent::Wave(wave));
                }
            } else {
                empty_count += 1;
                if empty_count < 50 {
                    std::hint::spin_loop();
                } else if empty_count < 200 {
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_micros(250));
                }
            }
        }
    })
}

/// The capture thread: always drains the raw input queue, writes it while recording, and
/// answers the recorder commands. On a write error (disk full) it closes the take so the
/// file stays readable. Returns the take still open when the session ends.
fn spawn_capture(
    mut capture: Capture,
    capturing: Arc<AtomicBool>,
    commands: Receiver<TapeCommand>,
    events: Sender<LiveEvent>,
) -> JoinHandle<Result<Option<TapeReport>>> {
    thread::spawn(move || {
        let mut next_status = Instant::now();
        while capturing.load(Ordering::Relaxed) {
            while let Ok(cmd) = commands.try_recv() {
                match cmd {
                    TapeCommand::Start(path, params, reply) => {
                        let _ = reply.send(capture.start(&path, params));
                    }
                    TapeCommand::Pause(reply) => {
                        let _ = reply.send(capture.pause());
                    }
                    TapeCommand::Resume(reply) => {
                        let _ = reply.send(capture.resume());
                    }
                    TapeCommand::Stop(reply) => {
                        let r = capture.stop();
                        if let Ok(ref report) = r {
                            let _ = events.try_send(LiveEvent::TapeStopped(report.clone()));
                        }
                        let _ = reply.send(r);
                    }
                }
                next_status = Instant::now();
            }

            let n = match capture.pump() {
                Ok(n) => n,
                Err(e) => {
                    let _ = events.try_send(LiveEvent::Error(format!("Recording stopped: {}", e)));
                    if let Ok(report) = capture.close_after_error() {
                        let _ = events.try_send(LiveEvent::TapeStopped(report));
                    }
                    0
                }
            };

            if Instant::now() >= next_status {
                let _ = events.try_send(LiveEvent::Tape(capture.status()));
                next_status = Instant::now() + TAPE_STATUS_PERIOD;
            }
            if n == 0 {
                thread::sleep(Duration::from_millis(5));
            }
        }

        let report = capture.finish()?;
        if let Some(ref r) = report {
            let _ = events.try_send(LiveEvent::TapeStopped(r.clone()));
        }
        Ok(report)
    })
}
