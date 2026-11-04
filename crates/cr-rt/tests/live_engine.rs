//! The live engine without audio hardware: frames go in through the input half, through the
//! DSP worker, and out of the output half, all from in-memory buffers.

use std::fs;

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_rt::live::engine::{Engine, StreamPlan, PRE_ROLL_FRAMES, RING_FRAMES};
use cr_rt::tape::{TapeRecorder, TapeSidecarMetadata};

fn plan(requested: u32, input: u32, output: u32) -> StreamPlan {
    StreamPlan {
        requested_rate: requested,
        input_rate: input,
        output_rate: output,
        input_channels: 2,
        output_channels: 2,
    }
}

/// Interleaved internal-domain stereo test tone, as 24-bit PCM arrives (even values, so the
/// 24-bit recording holds it exactly).
fn tone(frames: usize, amp: f64) -> Vec<i32> {
    let mut v = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let t = i as f64 / 44_100.0;
        v.push((amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8_388_607.0) as i32 * 2);
        v.push((amp * (2.0 * std::f64::consts::PI * 660.0 * t).sin() * 8_388_607.0) as i32 * 2);
    }
    v
}

/// Runs the DSP until its input queue is empty.
fn drain(engine: &mut Engine) {
    while engine.worker.step() > 0 {}
}

/// Pulls `frames` stereo frames out of the output half in 512-frame callbacks.
fn pull(engine: &mut Engine, frames: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0i32; 1024];
    while out.len() < frames * 2 {
        engine.output.fill_interleaved(&mut buf, 2, |x| x);
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

#[test]
fn tape_uses_negotiated_input_rate_not_requested() {
    // Asked for 44.1 kHz, the device fell back to its 48 kHz default.
    let dir = std::env::temp_dir().join(format!("cr_live_rate_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("rate.wav");
    let p = plan(44_100, 48_000, 48_000);
    assert!(!p.warnings().is_empty(), "a rate fallback must be reported");

    let mut engine = Engine::build(&p, RtParameters::default(), 1.0, None).unwrap();
    engine.capture.start(&wav, RtParameters::default()).unwrap();
    let data = tone(4800, 0.25);
    engine.input.push_interleaved(&data, 2, |x| x);
    let report = engine.capture.stop().unwrap();
    assert_eq!((report.frames, report.sample_rate), (4800, 48_000));

    let reader = AudioFileReader::open(&wav).unwrap();
    assert_eq!(reader.meta.sample_rate, 48_000, "WAV header must carry the real rate");
    let sidecar: TapeSidecarMetadata =
        serde_json::from_str(&fs::read_to_string(TapeRecorder::sidecar_path(&wav)).unwrap())
            .unwrap();
    assert_eq!(sidecar.sample_rate, 48_000);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rate_mismatch_between_devices_is_warned() {
    assert!(plan(48_000, 48_000, 48_000).warnings().is_empty());
    let w = plan(48_000, 48_000, 44_100).warnings();
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("wrong speed"));
}

#[test]
fn bypass_is_bit_exact_after_latency_and_volume_scales_monitor_only() {
    let params = RtParameters {
        mode: RtSoundMode::SoundIn,
        method: Method::Simple,
        ..RtParameters::default()
    };
    let dir = std::env::temp_dir().join(format!("cr_live_bypass_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("bypass.wav");
    let mut engine = Engine::build(&plan(44_100, 44_100, 44_100), params.clone(), 1.0, None).unwrap();
    engine.capture.start(&wav, params).unwrap();
    let delay = engine.worker.latency_frames();
    assert_eq!(delay, 5120, "Simple at 44.1 kHz: proc_delay 4096 + 2 * 512");

    // Feed and play in step like a real stream, so the queue stays below the drift limit.
    let n = 30_000;
    let data = tone(n, 0.5);
    let mut out = Vec::new();
    for (i, chunk) in data.chunks(2048).enumerate() {
        engine.input.push_interleaved(chunk, 2, |x| x);
        drain(&mut engine);
        if i >= PRE_ROLL_FRAMES / 1024 {
            if out.len() >= (delay + 8000) * 2 {
                engine.shared.set_monitor_volume(0.5);
            }
            out.extend(pull(&mut engine, 1024));
        }
    }
    let half_from = (delay + 8000) / 1024 * 1024 + 1024;
    assert!(out.len() >= (half_from + 4000) * 2);
    assert!(out[..delay * 2].iter().all(|&s| s == 0), "warm-up is silent");
    assert_eq!(&out[delay * 2..(delay + 8000) * 2], &data[..8000 * 2], "Sound In passes the input unchanged");

    // Half volume halves what is heard...
    for (k, &s) in out[half_from * 2..(half_from + 4000) * 2].iter().enumerate() {
        let src = data[(half_from - delay) * 2 + k];
        assert!((s - src / 2).abs() <= 1, "frame {}: {} vs {}", k / 2, s, src / 2);
    }

    // ...but the recording keeps the raw input.
    let report = engine.capture.stop().unwrap();
    assert_eq!(report.frames, n as u64);
    let mut reader = AudioFileReader::open(&wav).unwrap();
    let recorded = reader.read_all_frames().unwrap();
    for (k, &(l, r)) in recorded.iter().enumerate() {
        assert_eq!((l, r), (data[2 * k], data[2 * k + 1]), "recorded frame {}", k);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn meters_report_peaks_and_clipping_then_reset() {
    let mut engine =
        Engine::build(&plan(44_100, 44_100, 44_100), RtParameters::default(), 1.0, None).unwrap();

    // Left at -6 dBFS, right pinned at full scale for 10 samples.
    let mut data = tone(1000, 0.5);
    for k in 0..10 {
        data[2 * (100 + k) + 1] = 16_777_214;
    }
    engine.input.push_interleaved(&data, 2, |x| x);
    let lv = engine.shared.take_levels();
    assert!((lv.input_peak[0] - 0.5).abs() < 0.01, "left peak {}", lv.input_peak[0]);
    assert!(lv.input_peak[1] > 0.999);
    assert_eq!(lv.input_clips, [0, 10]);
    assert_eq!(lv.dropped_input, 0);

    let again = engine.shared.take_levels();
    assert_eq!(again.input_peak, [0.0, 0.0]);
    assert_eq!(again.input_clips, [0, 0]);
}

#[test]
fn mono_input_feeds_both_channels() {
    let mut engine =
        Engine::build(&plan(44_100, 44_100, 44_100), RtParameters::default(), 1.0, None).unwrap();
    let mono: Vec<i32> = (0..500).map(|i| i * 1000).collect();
    engine.input.push_interleaved(&mono, 1, |x| x);
    let lv = engine.shared.take_levels();
    assert_eq!(lv.input_peak[0], lv.input_peak[1]);
    assert!(lv.input_peak[0] > 0.0);
    assert_eq!((lv.input_frames, lv.input_lr_differs), (500, 0));
}

#[test]
fn identical_channels_are_counted() {
    let mut engine =
        Engine::build(&plan(48_000, 48_000, 48_000), RtParameters::default(), 1.0, None).unwrap();

    // Stereo: the two channels differ almost everywhere.
    engine.input.push_interleaved(&tone(1000, 0.3), 2, |x| x);
    let lv = engine.shared.take_levels();
    assert_eq!(lv.input_frames, 1000);
    assert!(lv.input_lr_differs > 900);

    // One channel copied to both, as Windows does for a 1-channel device format.
    let copied: Vec<i32> = tone(1000, 0.3).chunks(2).flat_map(|f| [f[0], f[0]]).collect();
    engine.input.push_interleaved(&copied, 2, |x| x);
    let lv = engine.shared.take_levels();
    assert_eq!((lv.input_frames, lv.input_lr_differs), (1000, 0));
}

#[test]
fn full_dsp_queue_counts_dropped_frames() {
    let mut engine =
        Engine::build(&plan(44_100, 44_100, 44_100), RtParameters::default(), 1.0, None).unwrap();
    // The DSP never runs, so everything past the queue size is lost and must be counted.
    let data = vec![0i32; (RING_FRAMES + 100) * 2];
    engine.input.push_interleaved(&data, 2, |x| x);
    assert_eq!(engine.shared.take_levels().dropped_input, 100);
}

#[test]
fn output_waits_for_preroll_and_counts_underruns() {
    let mut engine =
        Engine::build(&plan(44_100, 44_100, 44_100), RtParameters::default(), 1.0, None).unwrap();
    let mut buf = vec![7i32; 512];

    // Not enough processed audio yet: silence, nothing consumed, no underrun.
    engine.input.push_interleaved(&tone(1000, 0.1), 2, |x| x);
    drain(&mut engine);
    engine.output.fill_interleaved(&mut buf, 2, |x| x);
    assert!(buf.iter().all(|&s| s == 0));
    assert_eq!(engine.shared.take_levels().underruns, 0);

    // Primed, then the queue runs dry inside one callback: one underrun.
    engine.input.push_interleaved(&tone(PRE_ROLL_FRAMES, 0.1), 2, |x| x);
    drain(&mut engine);
    let mut big = vec![0i32; (PRE_ROLL_FRAMES + 1000 + 100) * 2];
    engine.output.fill_interleaved(&mut big, 2, |x| x);
    assert_eq!(engine.shared.take_levels().underruns, 1);
}

#[test]
fn method_change_restarts_dsp_other_changes_do_not() {
    let mut engine =
        Engine::build(&plan(44_100, 44_100, 44_100), RtParameters::default(), 1.0, None).unwrap();
    assert_eq!(engine.worker.latency_frames(), 10_240, "Wavelet default");

    let mut p = engine.worker.params().clone();
    p.sensitivity = RtSensitivity::Fine(70);
    p.mode = RtSoundMode::Noise;
    p.detect_pitch = false;
    assert!(!engine.worker.set_params(p.clone()));
    assert_eq!(engine.worker.params(), &p);

    p.method = Method::Simple;
    assert!(engine.worker.set_params(p.clone()));
    assert_eq!(engine.worker.latency_frames(), 5120);
    assert_eq!(engine.worker.params(), &p);
}

#[test]
fn high_rates_report_latency_in_hardware_frames() {
    let engine =
        Engine::build(&plan(96_000, 96_000, 96_000), RtParameters::default(), 1.0, None).unwrap();
    assert_eq!(engine.worker.latency_frames(), 2 * 10_240);
}
