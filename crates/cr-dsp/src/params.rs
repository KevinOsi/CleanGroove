//! Parameters, Operating Modes, Presets, and Fixed-Point Scaling
//!
//! See docs/engine/OFFLINE_ENGINE.md, sections 1 and 2, and docs/engine/REALTIME_ENGINE.md, section 2.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Simple = 0,
    Wavelet = 1,
    WaveletX2 = 2,
    WaveletX3 = 3,
}

impl Method {
    pub fn is_wavelet(&self) -> bool {
        matches!(self, Method::Wavelet | Method::WaveletX2 | Method::WaveletX3)
    }

    pub fn num_subchannels(&self) -> usize {
        match self {
            Method::Simple | Method::Wavelet => 0,
            Method::WaveletX2 => 1,
            Method::WaveletX3 => 2,
        }
    }

    pub fn dyadic_level(&self) -> usize {
        *self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Stereo = 0,
    Merge = 1,
    Mono = 2,
}

/// Engine profile (engine doc 13).
///
/// Classic is the engine described in docs/engine/OFFLINE_ENGINE.md and stays bit-identical (pinned by the
/// golden-output test). Enhanced extends it where Classic measurably falls short; so far: longer
/// reach, so defects (gouges) of 4-20 ms are repaired when they stand clear of the music.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    #[default]
    Classic,
    Enhanced,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Classic => "classic",
            Engine::Enhanced => "enhanced",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "classic" => Some(Engine::Classic),
            "enhanced" => Some(Engine::Enhanced),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrParameters {
    pub dcl: i32,
    pub dcr: i32,
    pub auto_repair: i32,
    pub detect_pitch: bool,
    pub reverse: bool,
    pub method: Method,
    pub mode: ChannelMode,
    pub compat_byte_delay: bool,
    pub engine: Engine,
}

impl Default for CrParameters {
    fn default() -> Self {
        Self::preset_stereo_lp()
    }
}

impl CrParameters {
    pub fn preset_stereo_lp() -> Self {
        Self {
            dcl: 50,
            dcr: 0,
            auto_repair: 100,
            detect_pitch: false,
            reverse: false,
            method: Method::Wavelet,
            mode: ChannelMode::Stereo,
            compat_byte_delay: true,
            engine: Engine::Classic,
        }
    }

    pub fn preset_stereo_brass() -> Self {
        Self {
            dcl: 50,
            dcr: 0,
            auto_repair: 100,
            detect_pitch: true,
            reverse: true,
            method: Method::Wavelet,
            mode: ChannelMode::Stereo,
            compat_byte_delay: true,
            engine: Engine::Classic,
        }
    }

    pub fn preset_stereo_percussion() -> Self {
        Self {
            dcl: 20,
            dcr: 0,
            auto_repair: 100,
            detect_pitch: false,
            reverse: true,
            method: Method::Simple,
            mode: ChannelMode::Stereo,
            compat_byte_delay: true,
            engine: Engine::Classic,
        }
    }

    pub fn preset_mono_lp() -> Self {
        Self {
            dcl: 50,
            dcr: 0,
            auto_repair: 100,
            detect_pitch: false,
            reverse: false,
            method: Method::Wavelet,
            mode: ChannelMode::Merge,
            compat_byte_delay: true,
            engine: Engine::Classic,
        }
    }

    pub fn preset_78_rpm() -> Self {
        Self {
            dcl: 50,
            dcr: 50,
            auto_repair: 100,
            detect_pitch: false,
            reverse: false,
            method: Method::Wavelet,
            mode: ChannelMode::Merge,
            compat_byte_delay: true,
            engine: Engine::Classic,
        }
    }
}

/// Real-Time Sensitivity selector: 0..=9 stepped slider or 0..=100 fine slider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtSensitivity {
    /// Classic ClickRepairRT 0..=9 slider (0 = Off, 5 = Default -> 40, 9 = Max -> 84)
    Stepped(u8),
    /// Direct 0..=100 sensitivity matching offline ClickRepair (0 = Off)
    Fine(u8),
}

impl Default for RtSensitivity {
    fn default() -> Self {
        Self::Stepped(5)
    }
}

impl RtSensitivity {
    pub fn is_enabled(self) -> bool {
        match self {
            Self::Stepped(n) => n > 0,
            Self::Fine(dcl) => dcl > 0,
        }
    }

    pub fn effective_s(self) -> i32 {
        match self {
            Self::Stepped(n) => 11 * (n.min(9) as i32) - 15,
            Self::Fine(dcl) => dcl.min(100) as i32,
        }
    }
}

/// Real-Time live monitoring sound modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtSoundMode {
    /// Time-aligned original clean audio (bypass).
    SoundIn = 0,
    /// Repaired restored audio.
    #[default]
    SoundOut = 1,
    /// Removed click difference signal (repaired - original).
    Noise = 2,
}

/// Configuration parameters for the Real-Time DSP engine.
#[derive(Debug, Clone, PartialEq)]
pub struct RtParameters {
    pub sensitivity: RtSensitivity,
    pub method: Method,
    pub mode: RtSoundMode,
    pub detect_pitch: bool,
    pub reverse: bool,
    pub parallel: bool,
}

impl Default for RtParameters {
    fn default() -> Self {
        Self {
            sensitivity: RtSensitivity::Stepped(5),
            method: Method::Wavelet,
            mode: RtSoundMode::SoundOut,
            detect_pitch: true,
            reverse: false,
            parallel: true,
        }
    }
}

/// The scan windows of a detector: a peak window between two shoulder windows (offline
/// engine doc, section 5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanWindows {
    /// Samples in the peak window.
    pub peak_len: usize,
    /// Samples in each shoulder window.
    pub shoulder_len: usize,
}

/// A detection threshold and the two factors the scanners use with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sensitivity {
    pub threshold: f64,
    /// A sample whose error exceeds `onset_factor * shoulder` starts or ends a click.
    pub onset_factor: f64,
    /// A peak window whose energy exceeds `detect_factor * shoulder^2` holds a click.
    pub detect_factor: f64,
}

impl Sensitivity {
    pub fn new(threshold: f64, windows: ScanWindows) -> Self {
        let onset_factor = threshold / (windows.shoulder_len as f64);
        let detect_factor = onset_factor * onset_factor * (windows.peak_len as f64);
        Self { threshold, onset_factor, detect_factor }
    }
}

/// Derived detection parameters for a real-time channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DerivedRtParams {
    pub windows: ScanWindows,
    pub sensitivity: Sensitivity,
}

impl RtParameters {
    pub fn compute_derived(&self) -> DerivedRtParams {
        let s = self.sensitivity.effective_s();
        // The RT scanner is always backward (realtime engine doc 2), and the reverse-scan +1 is already
        // folded into these constants (5 / 7 vs. offline 4 / 6). `self.reverse` must not add
        // a second +1. (Audit fix: setting reverse = true previously widened the window again.)
        let windows = if self.method.is_wavelet() {
            ScanWindows { peak_len: (5 + s / 25) as usize, shoulder_len: (80 + s / 5) as usize }
        } else {
            ScanWindows { peak_len: (7 + s / 15) as usize, shoulder_len: (100 + s / 3) as usize }
        };
        DerivedRtParams { windows, sensitivity: Sensitivity::new(threshold_for(s), windows) }
    }
}

/// Sensitivity-to-threshold 4th-degree polynomial in u = 100 - slider_val.
#[inline]
pub fn threshold_for(slider_val: i32) -> f64 {
    let u = (100 - slider_val) as f64;
    2.8 + (0.012 + (2.5e-4 + 3.5e-8 * u * u) * u) * u
}

/// Symmetric round-half-away-from-zero, the engine's rounding throughout.
#[inline(always)]
pub fn round_sym(d: f64) -> i32 {
    if d >= 0.0 {
        (d + 0.5) as i32
    } else {
        (d - 0.5) as i32
    }
}

/// Converts a 16-bit signed PCM integer into internal 25-bit domain.
#[inline(always)]
pub fn scale_i16_to_internal(sample: i16) -> i32 {
    (sample as i32) << 9
}

/// Converts a 24-bit signed PCM integer (range -8388608..=8388607) into internal 25-bit domain.
#[inline(always)]
pub fn scale_i24_to_internal(sample: i32) -> i32 {
    sample << 1
}

/// Converts a 32-bit signed PCM integer into internal 25-bit domain.
#[inline(always)]
pub fn scale_i32_to_internal(sample: i32) -> i32 {
    sample >> 7
}

/// Converts a floating-point sample in [-1.0, 1.0] to internal 25-bit integer domain.
#[inline(always)]
pub fn scale_f64_to_internal(sample: f64) -> i32 {
    round_sym(sample * 16777216.0).clamp(-16777216, 16777215)
}

/// IEEE 754 32-bit float to the internal integer domain by bit extraction (engine doc 1).
pub fn float_bits_to_internal_int(bits: u32) -> i32 {
    let shift = (127_i32.wrapping_sub(((bits >> 23) & 0xFF) as i32)) & 0xFF;
    if shift > 25 {
        return 0;
    }
    let mag = ((0x0100_0000_u32 | ((bits & 0x007F_FFFF) << 1)) as i32) >> shift;
    if (bits & 0x8000_0000) == 0 {
        mag
    } else {
        -mag
    }
}

/// Clamps and converts internal 25-bit signed integer to 24-bit signed PCM domain.
#[inline(always)]
pub fn internal_to_i24_clamped(sample_i32: i32) -> i32 {
    (sample_i32 >> 1).clamp(-8_388_607, 8_388_607)
}

/// Converts internal 25-bit integer to 16-bit PCM integer.
#[inline(always)]
pub fn internal_to_i16(sample_i32: i32) -> i16 {
    (internal_to_i24_clamped(sample_i32) >> 8) as i16
}

/// Converts internal 25-bit integer to 32-bit PCM integer.
#[inline(always)]
pub fn internal_to_i32(sample_i32: i32) -> i32 {
    internal_to_i24_clamped(sample_i32) << 8
}

/// Converts internal 25-bit integer to 64-bit float [-1.0, 1.0].
#[inline(always)]
pub fn internal_to_f64(sample_i32: i32) -> f64 {
    (sample_i32 as f64) / 16777216.0
}

/// Derived parameters for a primary Channel pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedChannelParams {
    pub declick_enabled: bool,
    pub decrackle_enabled: bool,
    pub windows: ScanWindows,
    /// Detection where nothing else is known.
    pub normal: Sensitivity,
    /// Detection where a subchannel has left a hint: 0.25 lower per subchannel.
    pub lower: Sensitivity,
    /// DeCrackle: decay of the running prediction-error level.
    pub error_decay: f64,
    /// DeCrackle: a sample is crackle when its error energy exceeds this times the level squared.
    pub crackle_factor: f64,
}

impl DerivedChannelParams {
    pub fn compute(
        dcl: i32,
        dcr: i32,
        is_wavelet: bool,
        is_reverse: bool,
        num_subchannels: usize,
    ) -> Self {
        let mut windows = if is_wavelet {
            ScanWindows { peak_len: (4 + dcl / 25) as usize, shoulder_len: (80 + dcl / 5) as usize }
        } else {
            ScanWindows { peak_len: (6 + dcl / 15) as usize, shoulder_len: (100 + dcl / 3) as usize }
        };
        if is_reverse {
            windows.peak_len += 1;
        }

        let normal = Sensitivity::new(threshold_for(dcl), windows);
        let lower = Sensitivity::new(normal.threshold - 0.25 * (num_subchannels as f64), windows);

        let error_decay = if is_wavelet { 0.996 } else { 0.998 };
        let scaled = threshold_for(dcr) * (1.0 - error_decay);
        let crackle_factor = scaled * scaled * 4.0;

        Self {
            declick_enabled: dcl > 0,
            decrackle_enabled: dcr > 0,
            windows,
            normal,
            lower,
            error_decay,
            crackle_factor,
        }
    }
}

/// Derived parameters for a SubChannel pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedSubChannelParams {
    pub windows: ScanWindows,
    pub normal: Sensitivity,
    pub lower: Sensitivity,
}

impl DerivedSubChannelParams {
    pub fn compute(dcl: i32, is_reverse: bool, remaining_subchannels: usize) -> Self {
        let windows = ScanWindows { peak_len: 3, shoulder_len: if is_reverse { 35 } else { 50 } };
        let normal = Sensitivity::new(threshold_for(dcl), windows);
        let lower = Sensitivity::new(normal.threshold - 0.25 * (remaining_subchannels as f64), windows);
        Self { windows, normal, lower }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_threshold_polynomial_values() {
        println!("n=100: {}", threshold_for(100));
        println!("n=50:  {}", threshold_for(50));
        println!("n=30:  {}", threshold_for(30));
        println!("n=22:  {}", threshold_for(22));
        println!("n=15:  {}", threshold_for(15));
        println!("n=1:   {}", threshold_for(1));
        assert!((threshold_for(100) - 2.800000).abs() < 1e-6);
        assert!((threshold_for(50) - 4.243750).abs() < 1e-6);
        assert!((threshold_for(30) - 5.70535).abs() < 1e-5);
    }

    #[test]
    fn test_derived_channel_params_follow_the_polynomial() {
        // dcl = 30, dcr = 50, Simple: the derived values follow the threshold polynomial exactly.
        let params = DerivedChannelParams::compute(30, 50, false, false, 0);
        let expected_threshold = threshold_for(30);
        assert!((params.normal.threshold - expected_threshold).abs() < 1e-12);
        let expected_onset = expected_threshold / (params.windows.shoulder_len as f64);
        assert!((params.normal.onset_factor - expected_onset).abs() < 1e-12);
        assert!((params.normal.detect_factor - expected_onset * expected_onset * (params.windows.peak_len as f64)).abs() < 1e-12);

        // Crackle factor: dcr = 50 -> threshold 4.243750, decay 0.998: (4.243750 * 0.002)^2 * 4.
        let expected_crackle_factor = (threshold_for(50) * (1.0 - 0.998)).powi(2) * 4.0;
        assert!((params.crackle_factor - expected_crackle_factor).abs() < 1e-12);
    }

    #[test]
    fn test_round_sym() {
        assert_eq!(round_sym(1.4), 1);
        assert_eq!(round_sym(1.5), 2);
        assert_eq!(round_sym(-1.4), -1);
        assert_eq!(round_sym(-1.5), -2);
        assert_eq!(round_sym(0.0), 0);
    }

    #[test]
    fn test_float_bits_to_internal_int() {
        // 1.0f32 in IEEE-754 is 0x3F80_0000 -> 16777216
        let bits_one = 1.0f32.to_bits();
        assert_eq!(float_bits_to_internal_int(bits_one), 16_777_216);

        // -1.0f32 -> -16777216
        let bits_neg_one = (-1.0f32).to_bits();
        assert_eq!(float_bits_to_internal_int(bits_neg_one), -16_777_216);

        // 0.0f32 -> 0
        let bits_zero = 0.0f32.to_bits();
        assert_eq!(float_bits_to_internal_int(bits_zero), 0);
    }
}
