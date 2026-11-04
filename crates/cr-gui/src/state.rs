use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Active background task progress state
#[derive(Clone)]
pub struct ProgressState {
    pub title: String,
    pub description: String,
    pub progress: f32, // 0.0 .. 1.0
    pub cancel_flag: Arc<AtomicBool>,
}

/// Messages sent from background worker tasks to the UI thread
pub enum JobMessage {
    Progress {
        fraction: f32,
        description: String,
    },
    AnalysisFinished {
        analysis: Box<cr_dsp::analysis::AlbumAnalysis>,
        report: Box<cr_dsp::autotune::AutoTuneReport>,
        elapsed_secs: f64,
    },
    RenderFinished {
        dest_filename: String,
        elapsed_secs: f64,
        speedup: f64,
        /// Post chain album gain (dB) and the output's levels, when the chain ran.
        levels: Option<(f64, cr_dsp::post::LevelReport)>,
    },
    ExportFinished {
        album_dir: PathBuf,
        files: usize,
        seconds: f64,
    },
    Cancelled,
    Error(String),
}

/// Cached column slices of original and repaired audio for the visible audit window
#[derive(Clone, Default)]
pub struct ViewportWaveformCache {
    pub view_start: usize,
    pub view_end: usize,
    pub width_cols: usize,
    /// Engine parameters the cache was computed with; None when the view was too long to
    /// preview repairs (rep_* then equal orig_*).
    pub params: Option<crate::dsp_bridge::RenderSettings>,
    pub repairs_previewed: bool,
    pub orig_l: Vec<f32>,
    pub rep_l: Vec<f32>,
    pub orig_r: Vec<f32>,
    pub rep_r: Vec<f32>,
    pub viewport_clicks: Vec<ClickMarker>,
}

/// Operational mode of the workstation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    File,
    Live,
    /// Metadata and track splits of a finished rip; plays the file as it is.
    Tracks,
}

/// Triad audition preview mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditionMode {
    OutClean,
    InBypass,
    NoiseDelta,
}

/// DeClick algorithm method
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeclickMethod {
    #[default]
    Wavelet,
    Simple,
    WaveletX2,
}

/// Detected click event marker
#[derive(Debug, Clone)]
pub struct ClickMarker {
    pub frame: usize,
    pub channel: usize, // 0 = Left, 1 = Right
    pub length: usize,
    pub amplitude: f32,
}

/// Audio DSP tuning parameters for preview playback
#[derive(Debug, Clone)]
pub struct DspTuning {
    pub declick: u32,
    pub decrackle: u32,
    pub pitch_protection: bool,
    pub reverse: bool,
    pub method: DeclickMethod,
    /// "Stereo>Mono": a mono record captured in stereo. Both channels are declicked separately,
    /// then the post chain aligns them and sums them to mono.
    pub stereo_to_mono: bool,
    pub audition_mode: AuditionMode,
    /// Post-declick corrections (see `cr_dsp::post`), applied in render, preview and audition.
    pub post_enabled: bool,
    pub flip_polarity: bool,
    pub balance_on: bool,
    /// Left-minus-right level difference to remove, dB.
    pub balance_db: f64,
    pub azimuth_on: bool,
    /// Delay of the right channel behind the left to remove, microseconds.
    pub azimuth_us: f64,
    pub subsonic_on: bool,
    pub elliptical_on: bool,
    pub elliptical_hz: f64,
    /// Needle-thump repair (`cr_dsp::thump`), after declicking and before the corrections, in
    /// render, preview and audition. Off until the user switches it on.
    pub dethump: bool,
    /// The spans to repair, from the record analysis (empty before one).
    pub thump_spans: std::sync::Arc<Vec<(usize, usize)>>,
}

impl Default for DspTuning {
    fn default() -> Self {
        Self {
            declick: 30,
            decrackle: 0,
            pitch_protection: true,
            reverse: false,
            method: DeclickMethod::Wavelet,
            stereo_to_mono: false,
            audition_mode: AuditionMode::OutClean,
            post_enabled: true,
            flip_polarity: false,
            balance_on: false,
            balance_db: 0.0,
            azimuth_on: false,
            azimuth_us: 0.0,
            subsonic_on: true,
            elliptical_on: true,
            elliptical_hz: cr_dsp::analysis::ELLIPTICAL_HZ,
            dethump: false,
            thump_spans: std::sync::Arc::new(Vec::new()),
        }
    }
}

/// Full track audio data and downsampled overview cache
#[derive(Clone)]
pub struct TrackData {
    pub path: Option<PathBuf>,
    pub filename: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub total_frames: usize,
    pub frames_internal: Arc<Vec<(i32, i32)>>,
    pub overview_min_max_l: Vec<(f32, f32)>,
    pub overview_min_max_r: Vec<(f32, f32)>,
}

impl TrackData {
    /// Generates overview min/max buckets from the internal-domain frames.
    pub fn compute_overview(frames: &[(i32, i32)], buckets: usize) -> (Vec<(f32, f32)>, Vec<(f32, f32)>) {
        let total = frames.len();
        if total == 0 {
            return (Vec::new(), Vec::new());
        }
        let step = (total / buckets).max(1);
        let actual_buckets = (total + step - 1) / step;
        let mut ov_l = Vec::with_capacity(actual_buckets);
        let mut ov_r = Vec::with_capacity(actual_buckets);

        for chunk in 0..actual_buckets {
            let start = chunk * step;
            let end = (start + step).min(total);
            let mut min_l = 0.0f32;
            let mut max_l = 0.0f32;
            let mut min_r = 0.0f32;
            let mut max_r = 0.0f32;

            for &(l, r) in &frames[start..end] {
                let vl = l as f32 / crate::dsp_bridge::FULL_SCALE;
                let vr = r as f32 / crate::dsp_bridge::FULL_SCALE;
                if vl < min_l { min_l = vl; }
                if vl > max_l { max_l = vl; }
                if vr < min_r { min_r = vr; }
                if vr > max_r { max_r = vr; }
            }
            ov_l.push((min_l, max_l));
            ov_r.push((min_r, max_r));
        }

        (ov_l, ov_r)
    }

}

/// Shared playhead and audio playback control
#[derive(Clone)]
pub struct AudioTransportControl {
    pub is_playing: Arc<AtomicBool>,
    pub is_looping: Arc<AtomicBool>,
    pub playhead_frame: Arc<AtomicUsize>,
    pub loop_start_frame: Arc<AtomicUsize>,
    pub loop_end_frame: Arc<AtomicUsize>,
    pub seek_requested: Arc<AtomicBool>,
}

impl Default for AudioTransportControl {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioTransportControl {
    pub fn new() -> Self {
        Self {
            is_playing: Arc::new(AtomicBool::new(false)),
            is_looping: Arc::new(AtomicBool::new(true)),
            playhead_frame: Arc::new(AtomicUsize::new(0)),
            loop_start_frame: Arc::new(AtomicUsize::new(0)),
            loop_end_frame: Arc::new(AtomicUsize::new(0)),
            seek_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn play(&self) {
        let cur = self.playhead_frame.load(Ordering::Relaxed);
        let start = self.loop_start_frame.load(Ordering::Relaxed);
        let end = self.loop_end_frame.load(Ordering::Relaxed);
        if start < end {
            if cur >= end || cur < start {
                self.playhead_frame.store(start, Ordering::SeqCst);
                self.seek_requested.store(true, Ordering::SeqCst);
            }
        }
        self.is_playing.store(true, Ordering::SeqCst);
    }

    pub fn pause(&self) {
        self.is_playing.store(false, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.is_playing.store(false, Ordering::SeqCst);
        let start = self.loop_start_frame.load(Ordering::Relaxed);
        self.playhead_frame.store(start, Ordering::SeqCst);
        self.seek_requested.store(true, Ordering::SeqCst);
    }

    pub fn seek_to(&self, frame: usize) {
        self.playhead_frame.store(frame, Ordering::SeqCst);
        self.seek_requested.store(true, Ordering::SeqCst);
    }
}

/// The view `start..end` (frames) after a wheel pan of `pan` pixels across a waveform `width`
/// pixels wide (negative: forward in time) and a zoom by `zoom` (above 1: in) around the
/// point `at` (0..1) across it, which stays where it is. Kept inside the file, at least
/// `min_len` long.
#[allow(clippy::too_many_arguments)]
pub fn pan_zoom_view(start: usize, end: usize, total: usize, min_len: usize, width: f32, at: f32, pan: f32, zoom: f32) -> (usize, usize) {
    let total_f = total as f64;
    let width = width.max(1.0) as f64;
    let mut len = end.saturating_sub(start).max(1) as f64;
    let mut s = start as f64;
    if zoom != 1.0 && zoom > 0.0 {
        let anchor = s + at as f64 * len;
        len = (len / zoom as f64).clamp((min_len as f64).min(total_f), total_f);
        s = anchor - at as f64 * len;
    }
    s -= pan as f64 * len / width;
    let s = s.clamp(0.0, (total_f - len).max(0.0)).round() as usize;
    (s, (s + len.round() as usize).min(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pan_zoom_view() {
        // 10 000-frame view of a 100 000-frame file, 1000 px wide.
        // Panning 100 px forward moves a tenth of the view.
        assert_eq!(pan_zoom_view(20_000, 30_000, 100_000, 100, 1000.0, 0.5, -100.0, 1.0), (21_000, 31_000));
        assert_eq!(pan_zoom_view(20_000, 30_000, 100_000, 100, 1000.0, 0.5, 100.0, 1.0), (19_000, 29_000));
        // Stops at the ends of the file.
        assert_eq!(pan_zoom_view(1_000, 11_000, 100_000, 100, 1000.0, 0.5, 500.0, 1.0), (0, 10_000));
        assert_eq!(pan_zoom_view(89_000, 99_000, 100_000, 100, 1000.0, 0.5, -500.0, 1.0), (90_000, 100_000));
        // Zooming in 2x around the left quarter keeps that frame (22 500) at the same place.
        let (s, e) = pan_zoom_view(20_000, 30_000, 100_000, 100, 1000.0, 0.25, 0.0, 2.0);
        assert_eq!((s, e), (21_250, 26_250));
        assert_eq!(s + (e - s) / 4, 22_500);
        // Zooming out never goes past the whole file, zooming in never below the minimum.
        assert_eq!(pan_zoom_view(20_000, 30_000, 100_000, 100, 1000.0, 0.5, 0.0, 0.01), (0, 100_000));
        let (s, e) = pan_zoom_view(20_000, 30_000, 100_000, 100, 1000.0, 0.5, 0.0, 1000.0);
        assert_eq!(e - s, 100);
    }

    #[test]
    fn test_transport_seek_and_play() {
        let transport = AudioTransportControl::new();
        assert!(!transport.is_playing.load(Ordering::Relaxed));

        transport.loop_start_frame.store(1000, Ordering::Relaxed);
        transport.loop_end_frame.store(5000, Ordering::Relaxed);

        transport.seek_to(2500);
        assert_eq!(transport.playhead_frame.load(Ordering::Relaxed), 2500);
        assert!(transport.seek_requested.load(Ordering::Relaxed));

        transport.play();
        assert!(transport.is_playing.load(Ordering::Relaxed));

        transport.stop();
        assert!(!transport.is_playing.load(Ordering::Relaxed));
        assert_eq!(transport.playhead_frame.load(Ordering::Relaxed), 1000);
    }
}
