//! The live session's recorder: raw input to a 24-bit WAV, started, paused, resumed and
//! stopped while monitoring keeps running.
//!
//! A pause skips audio and keeps the same file; each place the file joins is listed in the
//! sidecar and written as an Audacity label file next to the WAV (side breaks, track gaps).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, bail, Result};
use cr_audio_io::{AudioFileWriter, AudioMetadata};
use cr_dsp::params::RtParameters;

use crate::tape::{format_system_time_iso8601, write_sidecar, SidecarExtras, TapePause};

/// Raw captures are always 24-bit.
pub const TAPE_BITS: u16 = 24;

/// Default file name template; see `next_take_path`.
pub const DEFAULT_TEMPLATE: &str = "{name}_side{n}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TapeState {
    #[default]
    Idle,
    Recording,
    Paused,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TapeStatus {
    pub state: TapeState,
    pub path: Option<PathBuf>,
    pub frames: u64,
    pub seconds: f64,
    /// Frames lost from this take (queue full). Must stay 0.
    pub dropped: u64,
    pub pauses: usize,
}

/// A finished take.
#[derive(Debug, Clone, PartialEq)]
pub struct TapeReport {
    pub wav_path: PathBuf,
    pub sidecar_path: PathBuf,
    /// Audacity labels at the pause points; None when the take was never paused.
    pub labels_path: Option<PathBuf>,
    pub frames: u64,
    pub sample_rate: u32,
    pub pauses: Vec<TapePause>,
    pub dropped: u64,
}

struct Take {
    writer: AudioFileWriter,
    path: PathBuf,
    params: RtParameters,
    frames: u64,
    paused: bool,
    pauses: Vec<TapePause>,
    dropped_at_start: u64,
}

pub struct Recorder {
    sample_rate: u32,
    input_device: Option<String>,
    take: Option<Take>,
}

impl Recorder {
    pub fn new(sample_rate: u32, input_device: Option<String>) -> Self {
        Self {
            sample_rate,
            input_device,
            take: None,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// A take is open (recording or paused).
    pub fn is_active(&self) -> bool {
        self.take.is_some()
    }

    /// Starts a take. Refuses to overwrite an existing file: a raw rip may be the only copy.
    /// `dropped_now` is the session's running count of dropped tape frames.
    pub fn start(&mut self, path: &Path, params: RtParameters, dropped_now: u64) -> Result<()> {
        if self.take.is_some() {
            bail!("Already recording; stop the current take first");
        }
        if path.exists() {
            bail!("{} already exists; not overwriting it", path.display());
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let writer = AudioFileWriter::create(
            path,
            AudioMetadata {
                channels: 2,
                sample_rate: self.sample_rate,
                bits_per_sample: TAPE_BITS,
                is_float: false,
                total_frames: 0,
            },
        )?;
        self.take = Some(Take {
            writer,
            path: path.to_path_buf(),
            params,
            frames: 0,
            paused: false,
            pauses: Vec::new(),
            dropped_at_start: dropped_now,
        });
        Ok(())
    }

    /// Captured frames; written only while recording.
    pub fn write(&mut self, frames: &[(i32, i32)]) -> Result<()> {
        if let Some(t) = self.take.as_mut().filter(|t| !t.paused) {
            t.writer.write_chunk(frames)?;
            t.frames += frames.len() as u64;
        }
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        match self.take.as_mut() {
            Some(t) if !t.paused => {
                t.paused = true;
                Ok(())
            }
            Some(_) => bail!("Already paused"),
            None => bail!("Not recording"),
        }
    }

    pub fn resume(&mut self) -> Result<()> {
        let rate = self.sample_rate as f64;
        match self.take.as_mut() {
            Some(t) if t.paused => {
                t.paused = false;
                // Only a join inside the file is worth marking.
                if t.frames > 0 && t.pauses.last().map(|p| p.at_frame) != Some(t.frames) {
                    t.pauses.push(TapePause {
                        at_frame: t.frames,
                        at_seconds: t.frames as f64 / rate,
                    });
                }
                Ok(())
            }
            Some(_) => bail!("Not paused"),
            None => bail!("Not recording"),
        }
    }

    /// Finishes the WAV, writes the sidecar and (when paused at least once) the labels.
    pub fn stop(&mut self, dropped_now: u64) -> Result<TapeReport> {
        let t = self.take.take().ok_or_else(|| anyhow!("Not recording"))?;
        t.writer.finalize()?;
        let dropped = dropped_now.saturating_sub(t.dropped_at_start);
        let sidecar_path = write_sidecar(
            &t.path,
            self.sample_rate,
            TAPE_BITS,
            t.frames as usize,
            &t.params,
            SidecarExtras {
                input_device: self.input_device.clone(),
                pauses: t.pauses.clone(),
                dropped_frames: dropped,
            },
        )?;
        let labels_path = if t.pauses.is_empty() {
            None
        } else {
            let p = labels_path(&t.path);
            fs::write(&p, audacity_labels(&t.pauses))?;
            Some(p)
        };
        Ok(TapeReport {
            wav_path: t.path,
            sidecar_path,
            labels_path,
            frames: t.frames,
            sample_rate: self.sample_rate,
            pauses: t.pauses,
            dropped,
        })
    }

    pub fn status(&self, dropped_now: u64) -> TapeStatus {
        match &self.take {
            None => TapeStatus::default(),
            Some(t) => TapeStatus {
                state: if t.paused { TapeState::Paused } else { TapeState::Recording },
                path: Some(t.path.clone()),
                frames: t.frames,
                seconds: t.frames as f64 / self.sample_rate as f64,
                dropped: dropped_now.saturating_sub(t.dropped_at_start),
                pauses: t.pauses.len(),
            },
        }
    }
}

/// `side_a.wav` -> `side_a_labels.txt`
pub fn labels_path(wav: &Path) -> PathBuf {
    let stem = wav.file_stem().and_then(|s| s.to_str()).unwrap_or("recording");
    wav.with_file_name(format!("{}_labels.txt", stem))
}

/// Audacity label track text: one point label per join.
pub fn audacity_labels(pauses: &[TapePause]) -> String {
    pauses
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{:.6}\t{:.6}\tPause {}\n", p.at_seconds, p.at_seconds, i + 1))
        .collect()
}

/// Builds the path of the next take in `folder` from `template`:
/// `{name}` the user's name for the record, `{date}` YYYY-MM-DD, `{time}` HH-MM-SS (both UTC),
/// `{n}` the first number from 1 up that gives a file that does not exist yet.
/// Without `{n}`, an existing file gets `_2`, `_3`, ... added. `.wav` is added if missing.
pub fn next_take_path(folder: &Path, template: &str, name: &str, now: SystemTime) -> PathBuf {
    let stamp = format_system_time_iso8601(now); // YYYY-MM-DDTHH:MM:SSZ
    let date = &stamp[0..10];
    let time = stamp[11..19].replace(':', "-");
    let name = sanitize(name.trim());
    let name = if name.is_empty() { "recording".to_string() } else { name };
    let base = template
        .replace("{name}", &name)
        .replace("{date}", date)
        .replace("{time}", &time);
    let base = sanitize(&base);
    let base = base.strip_suffix(".wav").unwrap_or(&base).to_string();

    let make = |stem: &str| folder.join(format!("{}.wav", stem));
    if base.contains("{n}") {
        (1..)
            .map(|n| make(&base.replace("{n}", &n.to_string())))
            .find(|p| !p.exists())
            .unwrap()
    } else {
        std::iter::once(make(&base))
            .chain((2..).map(|n| make(&format!("{}_{}", base, n))))
            .find(|p| !p.exists())
            .unwrap()
    }
}

/// Replaces characters Windows does not allow in file names.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}
