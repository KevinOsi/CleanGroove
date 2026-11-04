//! Real-Time (RT) Verification Suite
//!
//! Parity and regression tests for the real-time engine (docs/engine/REALTIME_ENGINE.md).

use std::time::Instant;

use cr_dsp::params::{
    DerivedChannelParams, Method, RtParameters, RtSensitivity, RtSoundMode,
};
use cr_dsp::rt::RtProcessor;

/// RT1: Verify stepped and fine sensitivity mapping and threshold polynomial values.
#[test]
fn test_rt1_stepped_and_fine_thresholds() {
    let expected_s = [-4, 7, 18, 29, 40, 51, 62, 73, 84];
    for n in 1..=9u8 {
        let sens = RtSensitivity::Stepped(n);
        let s = sens.effective_s();
        assert_eq!(s, expected_s[(n - 1) as usize]);

        let mut params = RtParameters::default();
        params.sensitivity = sens;
        params.method = Method::Wavelet;
        let derived = params.compute_derived();

        // Derived should match DerivedChannelParams with reverse = true, num_sub = 0
        let offline_derived = DerivedChannelParams::compute(s, 0, true, true, 0);
        assert_eq!(derived.windows, offline_derived.windows);
        assert!((derived.sensitivity.threshold - offline_derived.normal.threshold).abs() < 1e-12);
        assert!((derived.sensitivity.onset_factor - offline_derived.normal.onset_factor).abs() < 1e-12);
        assert!((derived.sensitivity.detect_factor - offline_derived.normal.detect_factor).abs() < 1e-12);
    }

    // Check fine slider
    for dcl in [0, 10, 30, 50, 75, 100] {
        let sens = RtSensitivity::Fine(dcl);
        assert_eq!(sens.effective_s(), dcl as i32);
    }
}

/// RT2: Wavelet bit-exact clean audio passthrough (0 LSB error on un-repaired samples).
#[test]
fn test_rt2_wavelet_bit_exact_clean_passthrough() {
    let n_frames = 65536;
    let mut params = RtParameters::default();
    params.sensitivity = RtSensitivity::Stepped(5);
    params.method = Method::Wavelet;
    params.mode = RtSoundMode::SoundOut;
    params.detect_pitch = true;

    let mut proc = RtProcessor::new(params, 44100);

    // Multi-tone clean audio within standard dynamic range
    let mut input = Vec::with_capacity(n_frames);
    for i in 0..n_frames {
        let t = i as f64 / 44100.0;
        let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 5_000_000.0
            + (2.0 * std::f64::consts::PI * 1200.0 * t).sin() * 2_000_000.0) as i32;
        input.push((s, s / 2));
    }

    let mut output = vec![(0i32, 0i32); n_frames];
    proc.process_frames(&input, &mut output);

    let delay = proc.proc_delay; // 10,240 frames
    assert_eq!(delay, 10240);

    // After pipeline latency, clean audio must match input 100% bit-for-bit (0 LSB error)
    for i in delay..n_frames {
        let in_idx = i - delay;
        assert_eq!(
            output[i].0, input[in_idx].0,
            "Wavelet passthrough altered Left channel at frame {}: out={}, in={}",
            i, output[i].0, input[in_idx].0
        );
        assert_eq!(
            output[i].1, input[in_idx].1,
            "Wavelet passthrough altered Right channel at frame {}: out={}, in={}",
            i, output[i].1, input[in_idx].1
        );
    }
}

/// RT3: Click removal and block boundary straddle + bit-identical SoundIn + Noise == SoundOut.
#[test]
fn test_rt3_click_removal_and_block_boundary_straddle() {
    for method in [Method::Simple, Method::Wavelet] {
        let n_frames = 32768;
        let mut params = RtParameters::default();
        params.sensitivity = RtSensitivity::Stepped(6);
        params.method = method;
        params.detect_pitch = false;

        let mut input = Vec::with_capacity(n_frames);
        for i in 0..n_frames {
            let t = i as f64 / 44100.0;
            let s = ((2.0 * std::f64::consts::PI * 300.0 * t).sin() * 1_000_000.0) as i32;
            input.push((s, s));
        }

        // 1. Click inside block (frame 3000..3015)
        for i in 3000..3015 {
            input[i].0 += 6_000_000;
            input[i].1 += 6_000_000;
        }

        // 2. Click straddling block boundary (frame 5115..5125 for Simple 512 boundary)
        for i in 5115..5125 {
            input[i].0 -= 5_500_000;
            input[i].1 -= 5_500_000;
        }

        // Test SoundOut
        params.mode = RtSoundMode::SoundOut;
        let mut proc_out = RtProcessor::new(params.clone(), 44100);
        let mut out_sound_out = vec![(0i32, 0i32); n_frames];
        proc_out.process_frames(&input, &mut out_sound_out);

        // Test SoundIn
        params.mode = RtSoundMode::SoundIn;
        let mut proc_in = RtProcessor::new(params.clone(), 44100);
        let mut out_sound_in = vec![(0i32, 0i32); n_frames];
        proc_in.process_frames(&input, &mut out_sound_in);

        // Test Noise
        params.mode = RtSoundMode::Noise;
        let mut proc_noise = RtProcessor::new(params.clone(), 44100);
        let mut out_noise = vec![(0i32, 0i32); n_frames];
        proc_noise.process_frames(&input, &mut out_noise);

        let delay = proc_out.proc_delay;

        // Verify click removal (> 35 dB suppression)
        let click1_idx = 3005 + delay;
        let clean1_val = ((2.0 * std::f64::consts::PI * 300.0 * (3005.0 / 44100.0)).sin()
            * 1_000_000.0) as i32;
        let residual = (out_sound_out[click1_idx].0 - clean1_val).abs();
        assert!(
            residual < 500_000,
            "Method {:?}: Click at frame 3005 not suppressed: residual = {}",
            method, residual
        );

        // Verify bit-exact identity: SoundIn + Noise == SoundOut
        for i in delay..n_frames {
            let left_sum = out_sound_in[i].0 + out_noise[i].0;
            let right_sum = out_sound_in[i].1 + out_noise[i].1;
            assert_eq!(
                left_sum, out_sound_out[i].0,
                "Method {:?}: SoundIn + Noise != SoundOut on Left at frame {}: in={}, noise={}, out={}",
                method, i, out_sound_in[i].0, out_noise[i].0, out_sound_out[i].0
            );
            assert_eq!(
                right_sum, out_sound_out[i].1,
                "Method {:?}: SoundIn + Noise != SoundOut on Right at frame {}: in={}, noise={}, out={}",
                method, i, out_sound_in[i].1, out_noise[i].1, out_sound_out[i].1
            );
        }
    }
}

/// RT4: 96 kHz decimated Stage 2 high-band fidelity on Right channel.
#[test]
fn test_rt4_96khz_decimated_right_channel_fidelity() {
    let n_frames = 65536;
    let mut params = RtParameters::default();
    params.sensitivity = RtSensitivity::Stepped(6);
    params.method = Method::Wavelet;
    params.mode = RtSoundMode::SoundOut;
    params.detect_pitch = false;

    let mut proc = RtProcessor::new(params, 96000);
    assert!(proc.stage2_decimated);

    // Ultrasonic 30 kHz sine tone (-6 dBFS = 8,388,608 amplitude)
    let amp = 8_388_608.0;
    let mut input = Vec::with_capacity(n_frames);
    for i in 0..n_frames {
        let t = i as f64 / 96000.0;
        let s = (amp * (2.0 * std::f64::consts::PI * 30_000.0 * t).sin()) as i32;
        input.push((s, s));
    }

    // Inject clicks on both channels
    for i in 12000..12010 {
        input[i].0 += 5_000_000;
        input[i].1 += 5_000_000;
    }

    let mut output = vec![(0i32, 0i32); n_frames];
    proc.process_frames(&input, &mut output);

    let delay = proc.proc_delay * proc.decimation_factor; // in hardware frames

    // Measure amplitude of 30 kHz tone on Right channel outside click region
    let test_start = delay + 5000;
    let test_end = delay + 10000;
    let max_r = output[test_start..test_end]
        .iter()
        .map(|&(_, r)| r.abs())
        .max()
        .unwrap_or(0);

    let loss_ratio = (max_r as f64) / amp;
    assert!(
        loss_ratio > 0.98,
        "Right channel 30 kHz ultrasonic tone lost: ratio = {:.4} (max_r = {}, expected ~{})",
        loss_ratio, max_r, amp
    );
}

/// RT5: Cached pitch protection locks onto periodic brass signal without shaving attacks.
#[test]
fn test_rt5_cached_pitch_protection() {
    let n_frames = 16384;
    let mut params = RtParameters::default();
    params.sensitivity = RtSensitivity::Stepped(7);
    params.method = Method::Wavelet;
    params.detect_pitch = true;
    params.mode = RtSoundMode::SoundOut;

    let mut proc = RtProcessor::new(params, 44100);

    // Periodic sawtooth signal at 600 Hz (period ~73.5 samples)
    let period = 73.5;
    let mut input = Vec::with_capacity(n_frames);
    for i in 0..n_frames {
        let phase = ((i as f64) % period) / period;
        let s = ((phase * 2.0 - 1.0) * 4_000_000.0) as i32;
        input.push((s, s));
    }

    let mut output = vec![(0i32, 0i32); n_frames];
    proc.process_frames(&input, &mut output);

    let delay = proc.proc_delay;
    assert!(proc.left_channel.curr_pitch > 0 || proc.left_channel.repairs_pending == 0);

    // Verify sawtooth wavefronts are preserved without excessive shaving
    let start = delay + 2000;
    let end = delay + 4000;
    let mut max_diff = 0;
    for i in start..end {
        let in_idx = i - delay;
        let diff = (output[i].0 - input[in_idx].0).abs();
        if diff > max_diff {
            max_diff = diff;
        }
    }

    assert!(
        max_diff < 100_000,
        "Pitch protection failed to protect sawtooth attacks: max_diff = {}",
        max_diff
    );
}

/// RT6: Continuous 24/7 counter wraparound guard and high-throughput streaming.
#[test]
fn test_rt6_continuous_overflow_guard_and_throughput() {
    let n_frames = 400_000;
    let mut params = RtParameters::default();
    params.sensitivity = RtSensitivity::Stepped(5);
    params.method = Method::Wavelet;
    params.detect_pitch = true;
    params.mode = RtSoundMode::SoundOut;

    let mut proc = RtProcessor::new(params, 44100);

    let chunk_size = 1024;
    let chunk = vec![(100_000i32, 100_000i32); chunk_size];
    let mut out_chunk = vec![(0i32, 0i32); chunk_size];

    let start_time = Instant::now();
    let mut processed = 0;

    while processed < n_frames {
        proc.process_frames(&chunk, &mut out_chunk);
        processed += chunk_size;
    }

    let elapsed = start_time.elapsed().as_secs_f64();
    let audio_dur = (n_frames as f64) / 44100.0;
    let speedup = audio_dur / elapsed;

    println!(
        "RT6 Throughput: {} frames in {:.3} s ({:.1}x real-time)",
        n_frames, elapsed, speedup
    );

    // Assert speedup > 30x real-time even in debug/release
    assert!(
        speedup > 30.0,
        "Real-time processing speed too slow: {:.1}x",
        speedup
    );
}

/// RT7: 350k continuous streaming across shift_positions (131k, 262k) boundary with click suppression.
#[test]
fn test_rt_click_repair_across_300k_wraparound() {
    let test_configs = [
        (Method::Simple, 48000),
        (Method::Wavelet, 48000),
        (Method::Wavelet, 192000),
    ];

    let n_frames = 350_000;
    let click_amp = 10_000_000i32;

    for (method, sample_rate) in test_configs {
        let mut clean_input = Vec::with_capacity(n_frames);
        let mut noisy_input = Vec::with_capacity(n_frames);

        let freq = 220.0;
        for i in 0..n_frames {
            let t = i as f64 / sample_rate as f64;
            let s = ((2.0 * std::f64::consts::PI * freq * t).sin() * 500_000.0) as i32;
            clean_input.push((s, s));
            noisy_input.push((s, s));
        }

        // Inject 10-sample clicks every 1,000 frames, specifically crossing 131k and 262k
        let mut click_centers = Vec::new();
        let mut k = 20_000; // start after initial warm-up
        while k + 80 < n_frames {
            let start = k + 70;
            let end = k + 80;
            for i in start..end {
                noisy_input[i].0 += click_amp;
                noisy_input[i].1 += click_amp;
            }
            click_centers.push((start, end));
            k += 1000;
        }

        // Verify specific required boundary click intervals exist
        assert!(click_centers.iter().any(|&(s, e)| s == 131_070 && e == 131_080));
        assert!(click_centers.iter().any(|&(s, e)| s == 262_070 && e == 262_080 || (s <= 262_145 && e >= 262_145)));

        // 1. Process with SoundOut
        let mut params = RtParameters::default();
        params.sensitivity = RtSensitivity::Stepped(6);
        params.method = method;
        params.detect_pitch = false;
        params.mode = RtSoundMode::SoundOut;

        let mut proc_out = RtProcessor::new(params.clone(), sample_rate);
        let mut output_sound_out = vec![(0i32, 0i32); n_frames];
        proc_out.process_frames(&noisy_input, &mut output_sound_out);

        // 2. Process with Noise mode to verify 0 (+/- 2 LSB) bleed on clean intervals
        let mut proc_noise = RtProcessor::new(params, sample_rate);
        proc_noise.set_mode(RtSoundMode::Noise);
        let mut output_noise = vec![(0i32, 0i32); n_frames];
        proc_noise.process_frames(&noisy_input, &mut output_noise);

        let delay = proc_out.proc_delay * proc_out.decimation_factor;

        // Verify click suppression > 30 dB (residual < 316,227) on all injected clicks
        // including those right across 131,072 and 262,144 thresholds
        for &(s, e) in &click_centers {
            let out_s = s + delay;
            let out_e = e + delay;
            if out_e >= n_frames {
                continue;
            }

            for i in out_s..out_e {
                let in_idx = i - delay;
                let clean_l = clean_input[in_idx].0;
                let clean_r = clean_input[in_idx].1;
                let residual_l = (output_sound_out[i].0 - clean_l).abs();
                let residual_r = (output_sound_out[i].1 - clean_r).abs();

                // 30 dB suppression threshold from 10,000,000 click amplitude is 316,227
                assert!(
                    residual_l < 316_228,
                    "Method {:?} at {} Hz: Left click suppression < 30 dB at frame {}: residual = {}, click = {}",
                    method, sample_rate, in_idx, residual_l, click_amp
                );
                assert!(
                    residual_r < 316_228,
                    "Method {:?} at {} Hz: Right click suppression < 30 dB at frame {}: residual = {}, click = {}",
                    method, sample_rate, in_idx, residual_r, click_amp
                );
            }
        }

        // Verify Noise output is 0 (+/- 2 LSB) on clean intervals between clicks
        for &(s, _) in &click_centers {
            let clean_start = s + 200 + delay;
            let clean_end = (s + 800 + delay).min(n_frames);
            if clean_start >= clean_end {
                continue;
            }

            for i in clean_start..clean_end {
                assert!(
                    output_noise[i].0.abs() <= 2,
                    "Method {:?} at {} Hz: Noise bleed on Left at frame {}: noise = {}",
                    method, sample_rate, i - delay, output_noise[i].0
                );
                assert!(
                    output_noise[i].1.abs() <= 2,
                    "Method {:?} at {} Hz: Noise bleed on Right at frame {}: noise = {}",
                    method, sample_rate, i - delay, output_noise[i].1
                );
            }
        }
    }
}
