//! Asynchronous Lock-Free Raw Audio Archiving ("Tape Recorder")
//!
//! Concurrently writes pristine input frames to disk without stalling the audio callback.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use cr_audio_io::{AudioFileWriter, AudioMetadata};
use cr_dsp::params::RtParameters;
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeSidecarMetadata {
    pub version: String,
    pub created_at: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub total_frames_recorded: usize,
    pub rt_monitoring_settings: TapeMonitoringSettings,
    pub recommended_offline_command: String,
    /// Capture device, when known (live sessions).
    #[serde(default)]
    pub input_device: Option<String>,
    /// Where the recording was paused and resumed: the file joins there.
    #[serde(default)]
    pub pauses: Vec<TapePause>,
    /// Input frames lost from the recording (queue full). Should always be 0.
    #[serde(default)]
    pub dropped_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TapePause {
    /// Frame in the file where audio from after the pause starts.
    pub at_frame: u64,
    pub at_seconds: f64,
}

/// What a live recording adds to the sidecar.
#[derive(Debug, Clone, Default)]
pub struct SidecarExtras {
    pub input_device: Option<String>,
    pub pauses: Vec<TapePause>,
    pub dropped_frames: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeMonitoringSettings {
    pub sensitivity: u8,
    pub effective_s: i32,
    pub method: String,
    pub pitch_protection: bool,
    pub monitoring_mode: String,
}

pub struct TapeProducer {
    producer: Producer<(i32, i32)>,
}

impl TapeProducer {
    /// Pushes a single raw frame into the tape queue non-blockingly from real-time audio thread.
    #[inline(always)]
    pub fn push_frame(&mut self, left: i32, right: i32) -> bool {
        self.producer.push((left, right)).is_ok()
    }
}

pub struct TapeHandle {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<usize>>>,
    wav_path: PathBuf,
}

impl TapeHandle {
    /// Signals the background thread to stop, flushes remaining frames, and writes JSON metadata sidecar.
    pub fn stop(mut self) -> Result<usize> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| anyhow::anyhow!("Tape recorder thread panicked"))?
        } else {
            Ok(0)
        }
    }

    /// Path to the WAV file being recorded.
    pub fn wav_path(&self) -> &Path {
        &self.wav_path
    }
}

pub fn format_system_time_iso8601(t: std::time::SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let total_secs = dur.as_secs();
    let sec = total_secs % 60;
    let min = (total_secs / 60) % 60;
    let hour = (total_secs / 3600) % 24;
    let days = total_secs / 86400;

    let mut year = 1970;
    let mut days_rem = days;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if days_rem < days_in_year {
            break;
        }
        days_rem -= days_in_year;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &dim in &days_in_month {
        if days_rem < dim {
            break;
        }
        days_rem -= dim;
        month += 1;
    }
    let day = days_rem + 1;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

impl Drop for TapeHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.running.store(false, Ordering::SeqCst);
            let _ = handle.join();
        }
    }
}

pub struct TapeRecorder {
    producer: Option<Producer<(i32, i32)>>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<usize>>>,
    wav_path: PathBuf,
}

impl TapeRecorder {
    /// Compute the companion sidecar JSON path for a given audio file path.
    pub fn sidecar_path<P: AsRef<Path>>(wav_path: P) -> PathBuf {
        let p = wav_path.as_ref();
        let mut os_str = p.as_os_str().to_os_string();
        os_str.push(".cleangroove-meta.json");
        PathBuf::from(os_str)
    }

    pub fn new<P: AsRef<Path>>(
        output_wav: P,
        sample_rate: u32,
        bits_per_sample: u16,
        params: RtParameters,
    ) -> Result<Self> {
        let wav_path = output_wav.as_ref().to_path_buf();
        let ring_capacity = 524_288; // ~11.8 seconds at 44.1 kHz
        let (producer, consumer) = RingBuffer::<(i32, i32)>::new(ring_capacity);

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread_wav_path = wav_path.clone();

        let handle = thread::spawn(move || {
            Self::worker_loop(
                thread_wav_path,
                consumer,
                thread_running,
                sample_rate,
                bits_per_sample,
                params,
            )
        });

        Ok(Self {
            producer: Some(producer),
            running,
            handle: Some(handle),
            wav_path,
        })
    }

    /// Splits into non-blocking producer for audio thread and controller handle for main thread.
    pub fn split(mut self) -> (TapeProducer, TapeHandle) {
        let producer = TapeProducer {
            producer: self.producer.take().expect("TapeRecorder producer already taken"),
        };
        let handle = TapeHandle {
            running: self.running.clone(),
            handle: self.handle.take(),
            wav_path: self.wav_path.clone(),
        };
        (producer, handle)
    }

    /// Path to the WAV file being recorded.
    pub fn wav_path(&self) -> &Path {
        &self.wav_path
    }

    /// Pushes a single raw frame into the tape queue non-blockingly from real-time audio thread.
    #[inline(always)]
    pub fn push_frame(&mut self, left: i32, right: i32) -> bool {
        self.producer
            .as_mut()
            .map(|p| p.push((left, right)).is_ok())
            .unwrap_or(false)
    }

    /// Signals the background thread to stop, flushes remaining frames, and writes JSON metadata sidecar.
    pub fn stop(mut self) -> Result<usize> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| anyhow::anyhow!("Tape recorder thread panicked"))?
        } else {
            Ok(0)
        }
    }

    fn worker_loop(
        wav_path: PathBuf,
        mut consumer: Consumer<(i32, i32)>,
        running: Arc<AtomicBool>,
        sample_rate: u32,
        bits_per_sample: u16,
        params: RtParameters,
    ) -> Result<usize> {
        let meta = AudioMetadata {
            channels: 2,
            sample_rate,
            bits_per_sample,
            is_float: false,
            total_frames: 0,
        };

        let mut writer = AudioFileWriter::create(&wav_path, meta)?;
        let mut total_written = 0usize;
        let mut chunk = Vec::with_capacity(65536);

        while running.load(Ordering::Relaxed) || !consumer.is_empty() {
            chunk.clear();
            while chunk.len() < 65536 {
                if let Ok(frame) = consumer.pop() {
                    chunk.push(frame);
                } else {
                    break;
                }
            }

            if !chunk.is_empty() {
                writer.write_chunk(&chunk)?;
                total_written += chunk.len();
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Finalize audio file
        writer.finalize()?;
        eprintln!(
            "\n[Tape Recorder] Finalized: {} frames written to {}",
            total_written,
            wav_path.display()
        );

        write_sidecar(
            &wav_path,
            sample_rate,
            bits_per_sample,
            total_written,
            &params,
            SidecarExtras::default(),
        )?;
        Ok(total_written)
    }
}

/// Writes the companion `<file>.cleangroove-meta.json` for a finished recording; returns its path.
pub fn write_sidecar(
    wav_path: &Path,
    sample_rate: u32,
    bits_per_sample: u16,
    total_frames: usize,
    params: &RtParameters,
    extras: SidecarExtras,
) -> Result<PathBuf> {
    let sidecar_path = TapeRecorder::sidecar_path(wav_path);
    let sens_val = match params.sensitivity {
        cr_dsp::params::RtSensitivity::Stepped(n) => n,
        cr_dsp::params::RtSensitivity::Fine(dcl) => dcl,
    };
    let effective_s = params.sensitivity.effective_s();

    let offline_method = match params.method {
        cr_dsp::params::Method::Simple => "simple",
        cr_dsp::params::Method::Wavelet => "wavelet",
        cr_dsp::params::Method::WaveletX2 => "wavelet-x2",
        cr_dsp::params::Method::WaveletX3 => "wavelet-x3",
    };

    let wav_filename = wav_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("recording.wav");
    let restored_filename = format!("{}_RESTORED.wav", wav_filename.trim_end_matches(".wav"));

    let recommended_offline_command = format!(
        "cleangroove-cli --input \"{}\" --output \"{}\" --dcl {} --dcr 0 --pitch-protection {} --method {} --mode stereo",
        wav_filename, restored_filename, effective_s.max(0), params.detect_pitch, offline_method
    );

    let sidecar = TapeSidecarMetadata {
        version: "1.0".to_string(),
        created_at: format_system_time_iso8601(std::time::SystemTime::now()),
        sample_rate,
        channels: 2,
        bits_per_sample,
        total_frames_recorded: total_frames,
        rt_monitoring_settings: TapeMonitoringSettings {
            sensitivity: sens_val,
            effective_s,
            method: format!("{:?}", params.method),
            pitch_protection: params.detect_pitch,
            monitoring_mode: format!("{:?}", params.mode),
        },
        recommended_offline_command,
        input_device: extras.input_device,
        pauses: extras.pauses,
        dropped_frames: extras.dropped_frames,
    };

    let mut sidecar_file = File::create(&sidecar_path)?;
    let json_str = serde_json::to_string_pretty(&sidecar)?;
    sidecar_file.write_all(json_str.as_bytes())?;

    Ok(sidecar_path)
}

impl Drop for TapeRecorder {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.running.store(false, Ordering::SeqCst);
            let _ = handle.join();
        }
    }
}
