//! Cross-Platform Hardware Audio Streaming Backend via CPAL
//!
//! Provides lock-free full-duplex real-time audio I/O on Windows (WASAPI/ASIO), macOS (CoreAudio), and Linux (ALSA/JACK).
//! The session logic lives in `crate::live`; this module only negotiates formats and turns the
//! engine's input and output halves into cpal streams.

use anyhow::{anyhow, Result};
use cpal::traits::DeviceTrait;
use cr_dsp::params::{
    internal_to_i16, internal_to_i24_clamped, internal_to_i32, scale_f64_to_internal,
    scale_i16_to_internal, scale_i32_to_internal,
};

use crate::live::engine::{InputSide, OutputSide};

pub use super::devices::list_devices;

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(u_period: u32) -> u32;
    fn timeEndPeriod(u_period: u32) -> u32;
}

/// 1 ms OS timer resolution while streaming, so the DSP worker's short sleeps are short.
pub(crate) fn begin_timer_resolution() {
    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
    }
}

pub(crate) fn end_timer_resolution() {
    #[cfg(windows)]
    unsafe {
        timeEndPeriod(1);
    }
}

/// The sample formats the stream callbacks convert, best first.
const USABLE_FORMATS: [cpal::SampleFormat; 3] =
    [cpal::SampleFormat::F32, cpal::SampleFormat::I32, cpal::SampleFormat::I16];

/// Picks the stream config for a device. The caller compares the result's rate with the
/// target to tell the user about a fallback.
pub(crate) fn find_best_config(
    device: &cpal::Device,
    target_sample_rate: u32,
    is_input: bool,
) -> Result<cpal::SupportedStreamConfig> {
    let (offered, default): (Vec<_>, _) = if is_input {
        (device.supported_input_configs()?.collect(), device.default_input_config().ok())
    } else {
        (device.supported_output_configs()?.collect(), device.default_output_config().ok())
    };
    pick_config(&offered, target_sample_rate, default.map(|d| d.sample_rate().0))
}

/// Ranks what a device offers: the requested rate if it can run it, then stereo over mono,
/// then the best format the callbacks convert (F32, I32, I16). The rate is the target, else
/// the device's default rate, else the highest it offers.
///
/// Audit fix: this used to take the first config at the target rate whatever its format and
/// channel count. ALSA lists a USB codec's 8-bit format first, so a direct `hw:` input was
/// opened as 8-bit ("Unsupported input sample format"), and could as well have been mono.
pub(crate) fn pick_config(
    offered: &[cpal::SupportedStreamConfigRange],
    target: u32,
    default_rate: Option<u32>,
) -> Result<cpal::SupportedStreamConfig> {
    let fits = |c: &cpal::SupportedStreamConfigRange, r: u32| {
        c.min_sample_rate().0 <= r && c.max_sample_rate().0 >= r
    };
    let best = offered
        .iter()
        .filter_map(|c| {
            let format_rank = USABLE_FORMATS.iter().position(|f| *f == c.sample_format())?;
            // Higher is better: rate fits, stereo, then format.
            Some(((fits(c, target), c.channels() >= 2, usize::MAX - format_rank), c))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, c)| c)
        .ok_or_else(|| {
            let formats: Vec<String> = offered
                .iter()
                .map(|c| format!("{:?} {} ch", c.sample_format(), c.channels()))
                .collect();
            anyhow!(
                "The device offers no sample format this program can use (needs F32, I32 or I16; offers {})",
                formats.join(", ")
            )
        })?;
    let rate = if fits(best, target) {
        target
    } else {
        default_rate
            .filter(|&r| fits(best, r))
            .unwrap_or(best.max_sample_rate().0)
    };
    Ok(best.with_sample_rate(cpal::SampleRate(rate)))
}

pub(crate) fn build_input_stream<E>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut side: InputSide,
    err_fn: E,
) -> Result<cpal::Stream>
where
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let ch = config.channels() as usize;
    let cfg = config.config();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg,
            move |d: &[f32], _: &cpal::InputCallbackInfo| {
                side.push_interleaved(d, ch, |s| scale_f64_to_internal(s as f64))
            },
            err_fn,
            None,
        )?,
        // Engine doc 1: 16-bit PCM -> 25-bit internal domain is << 9.
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg,
            move |d: &[i16], _: &cpal::InputCallbackInfo| {
                side.push_interleaved(d, ch, scale_i16_to_internal)
            },
            err_fn,
            None,
        )?,
        // Engine doc 1: 32-bit PCM -> 25-bit internal domain is >> 7.
        cpal::SampleFormat::I32 => device.build_input_stream(
            &cfg,
            move |d: &[i32], _: &cpal::InputCallbackInfo| {
                side.push_interleaved(d, ch, scale_i32_to_internal)
            },
            err_fn,
            None,
        )?,
        fmt => return Err(anyhow!("Unsupported input sample format {:?}", fmt)),
    };
    Ok(stream)
}

pub(crate) fn build_output_stream<E>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut side: OutputSide,
    err_fn: E,
) -> Result<cpal::Stream>
where
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let ch = config.channels() as usize;
    let cfg = config.config();
    let stream = match config.sample_format() {
        // Engine doc 1 output path: >> 1 and clamp to 24-bit, then normalise.
        cpal::SampleFormat::F32 => device.build_output_stream(
            &cfg,
            move |d: &mut [f32], _: &cpal::OutputCallbackInfo| {
                side.fill_interleaved(d, ch, |x| internal_to_i24_clamped(x) as f32 / 8_388_608.0)
            },
            err_fn,
            None,
        )?,
        // Engine doc 1: internal -> 16-bit is (x >> 1, clamp 24-bit) >> 8.
        cpal::SampleFormat::I16 => device.build_output_stream(
            &cfg,
            move |d: &mut [i16], _: &cpal::OutputCallbackInfo| {
                side.fill_interleaved(d, ch, internal_to_i16)
            },
            err_fn,
            None,
        )?,
        // Engine doc 1: internal -> 32-bit is (x >> 1, clamp 24-bit) << 8.
        cpal::SampleFormat::I32 => device.build_output_stream(
            &cfg,
            move |d: &mut [i32], _: &cpal::OutputCallbackInfo| {
                side.fill_interleaved(d, ch, internal_to_i32)
            },
            err_fn,
            None,
        )?,
        fmt => return Err(anyhow!("Unsupported output sample format {:?}", fmt)),
    };
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfigRange};

    fn range(ch: u16, min: u32, max: u32, f: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(ch, SampleRate(min), SampleRate(max), SupportedBufferSize::Unknown, f)
    }

    #[test]
    fn usb_codec_on_alsa_gets_16_bit_stereo() {
        // hw:CARD=CODEC on the MX Linux box (TI PCM29xx): S8 and S16_LE, 1 or 2 channels,
        // 8-48 kHz, 8-bit listed first. The old choice took the first match: 8-bit, which
        // the callbacks cannot convert ("Unsupported input sample format").
        let offered = [
            range(1, 8_000, 48_000, SampleFormat::I8),
            range(2, 8_000, 48_000, SampleFormat::I8),
            range(1, 8_000, 48_000, SampleFormat::I16),
            range(2, 8_000, 48_000, SampleFormat::I16),
        ];
        let c = pick_config(&offered, 48_000, Some(48_000)).unwrap();
        assert_eq!((c.sample_format(), c.channels(), c.sample_rate().0), (SampleFormat::I16, 2, 48_000));
    }

    #[test]
    fn float_is_preferred_and_mono_only_when_nothing_else() {
        // WASAPI shared mode lists U8, I16, I32, F32 at the mix rate.
        let wasapi = [
            range(2, 48_000, 48_000, SampleFormat::U8),
            range(2, 48_000, 48_000, SampleFormat::I16),
            range(2, 48_000, 48_000, SampleFormat::I32),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ];
        assert_eq!(pick_config(&wasapi, 48_000, None).unwrap().sample_format(), SampleFormat::F32);

        let mono_only = [range(1, 44_100, 48_000, SampleFormat::I16)];
        assert_eq!(pick_config(&mono_only, 48_000, None).unwrap().channels(), 1);
    }

    #[test]
    fn rate_falls_back_to_the_device_default_then_its_maximum() {
        let offered = [range(2, 44_100, 48_000, SampleFormat::I16)];
        assert_eq!(pick_config(&offered, 96_000, Some(44_100)).unwrap().sample_rate().0, 44_100);
        assert_eq!(pick_config(&offered, 96_000, Some(8_000)).unwrap().sample_rate().0, 48_000);
        assert_eq!(pick_config(&offered, 96_000, None).unwrap().sample_rate().0, 48_000);
    }

    #[test]
    fn no_usable_format_is_an_error_that_says_what_was_offered() {
        let offered = [range(2, 48_000, 48_000, SampleFormat::U8)];
        let err = pick_config(&offered, 48_000, None).unwrap_err().to_string();
        assert!(err.contains("U8"), "{}", err);
    }
}
