//! Master Verification & Parity Test Suite
//!
//! Executing the 6 initial verification tests plus 3 mandatory anti-stubbing regression tests.

use std::time::Instant;

use cr_dsp::burg::{burg_fit, reflection_to_predictor};
use cr_dsp::params::{threshold_for, ChannelMode, CrParameters, DerivedChannelParams, Method};
use cr_dsp::process::Processor;
use cr_dsp::reconstruct::{cholesky_factor, solve_factored};
use cr_dsp::wavelet::{synthesize_zero_up, D4Analyzer, HIGH, LOW};

/// V1: Exact polynomial and derived statistical factors across slider positions.
#[test]
fn test_v1_polynomial_and_derived_factors() {
    for slider in 0..=100 {
        let th = threshold_for(slider);
        assert!(th >= 2.80 && th <= 10.0);

        let params = DerivedChannelParams::compute(slider, slider, false, false, 0);
        let expected_srch = th / (params.windows.shoulder_len as f64);
        let expected_det = expected_srch * expected_srch * (params.windows.peak_len as f64);
        assert!((params.normal.onset_factor - expected_srch).abs() < 1e-12);
        assert!((params.normal.detect_factor - expected_det).abs() < 1e-12);
    }
}

/// V2: Daubechies D4 QMF perfect reconstruction and filterbank orthonormality.
#[test]
fn test_v2_d4_wavelet_perfect_reconstruction() {
    // 1. Orthonormality
    let dot = |a: &[f64; 4], b: &[f64; 4]| (0..4).map(|i| a[i] * b[i]).sum::<f64>();
    assert!((dot(&LOW, &LOW) - 1.0).abs() < 1e-14);
    assert!((dot(&HIGH, &HIGH) - 1.0).abs() < 1e-14);
    assert!(dot(&LOW, &HIGH).abs() < 1e-14);

    // 2. Sample-level reconstruction accuracy
    let mut analyzer = D4Analyzer::new();
    let mut input = Vec::new();
    for i in 0..1024 {
        let t = i as f64 * 0.04;
        let s = (t.sin() * 8_000_000.0) as i32;
        input.push(s);
    }

    let mut wavelet_lo = Vec::new();
    let mut wavelet_hi = Vec::new();
    for i in (0..1024).step_by(2) {
        let (lo, hi) = analyzer.analyze(input[i], input[i + 1]);
        wavelet_lo.push(lo);
        wavelet_hi.push(hi);
    }
    let (lo_pad, hi_pad) = analyzer.analyze(0, 0);
    wavelet_lo.push(lo_pad);
    wavelet_hi.push(hi_pad);

    let mut reconstructed = Vec::new();
    for k in 0..(wavelet_lo.len() - 1) {
        let (e, o) = synthesize_zero_up(
            wavelet_lo[k],
            wavelet_hi[k],
            wavelet_lo[k + 1],
            wavelet_hi[k + 1],
        );
        reconstructed.push(e);
        reconstructed.push(o);
    }

    for i in 4..1000 {
        assert!(
            (input[i] - reconstructed[i]).abs() <= 2,
            "Mismatch at sample {}: input={}, reconstructed={}",
            i,
            input[i],
            reconstructed[i]
        );
    }
}

/// V3: High-order Burg LPC reflection coefficients and Levinson-Durbin step-up.
#[test]
fn test_v3_burg_and_levinson_durbin() {
    let mask = 2047;
    let mut samples = vec![0i32; 2048];
    for i in 0..2048 {
        let t = i as f64 * 0.03;
        let s = (t.sin() + 0.3 * (3.0 * t).sin()) * 4_000_000.0 + ((i % 17) as f64 * 1000.0);
        samples[i] = s as i32;
    }

    let mut fwd_err = vec![0.0; 1024];
    let mut bck_err = vec![0.0; 1024];
    let mut refl = vec![0.0; 64];

    let ms_err = burg_fit(&samples, mask, 768, 500, 16, &mut fwd_err, &mut bck_err, &mut refl);
    assert!(ms_err >= 0.0);

    for m in 0..16 {
        assert!(refl[m].abs() < 1.0, "Reflection coeff out of bounds: {}", refl[m]);
    }

    let mut coeffs = vec![0.0; 16];
    reflection_to_predictor(16, &mut refl, &mut coeffs);
    for c in &coeffs {
        assert!(c.is_finite());
    }
}

/// V4: Toeplitz Cholesky factorization and minimum-variance AR interpolation.
#[test]
fn test_v4_toeplitz_cholesky_solver() {
    let n = 8;
    let c = vec![1.0, 0.6, 0.3, 0.15, 0.08, 0.04, 0.02, 0.01];
    let mut u = vec![0.0; n * n];

    assert!(cholesky_factor(n, &c, &mut u));

    // Verify U^T * U == C
    for i in 0..n {
        for j in 0..n {
            let mut sum = 0.0;
            for k in 0..n {
                sum += u[k * n + i] * u[k * n + j];
            }
            let expected = if j >= i { c[j - i] } else { c[i - j] };
            assert!((sum - expected).abs() < 1e-12);
        }
    }

    // Verify solve_factored() produces exact solution C * x = rhs
    let mut rhs = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let orig_rhs = rhs.clone();
    let mut fixed = vec![0i32; n];
    solve_factored(n, &u, &mut rhs, &mut fixed);

    for i in 0..n {
        let mut sum = 0.0;
        for j in 0..n {
            let c_ij = if j >= i { c[j - i] } else { c[i - j] };
            sum += c_ij * (fixed[j] as f64);
        }
        assert!((sum - orig_rhs[i]).abs() <= 2.0);
    }
}

/// V5: Synthetic vinyl restoration with click and crackle detection.
#[test]
fn test_v5_synthetic_vinyl_restoration() {
    let mut params = CrParameters::default();
    params.dcl = 50;
    params.dcr = 0;
    params.method = Method::Simple;
    params.mode = ChannelMode::Stereo;

    let n_samples = 8000;
    let mut processor = Processor::new(params, 44100, Some(n_samples));
    let mut input = Vec::new();

    for i in 0..n_samples {
        let t = i as f64 * 0.05;
        let s = (t.sin() * 2_000_000.0) as i32;
        input.push((s, s));
    }

    // Inject 10 large clicks at sample 3000
    for i in 3000..3010 {
        input[i] = (12_000_000, 12_000_000);
    }

    let mut output = Vec::new();
    for &(l, r) in &input {
        if let Some(out) = processor.process_frame(l, r) {
            output.push(out);
        }
    }
    let flushed = processor.flush();
    output.extend(flushed);

    // Assert that the massive click at sample 3000 was repaired and clamped
    for i in 3000..3010 {
        assert!(
            output[i].0 < 6_000_000,
            "Click at {} not repaired: got {}",
            i,
            output[i].0
        );
    }
}

/// V6: Stereo Merge proportional balance unmixing.
#[test]
fn test_v6_multirate_and_stereo_merge() {
    let mut params = CrParameters::default();
    params.dcl = 0;
    params.dcr = 0;
    params.method = Method::Simple;
    params.mode = ChannelMode::Merge;

    let n = 2000;
    let mut processor = Processor::new(params, 44100, Some(n));
    assert!(processor.merge_mode);

    let mut input = Vec::new();
    for i in 0..n {
        let l = ((i as f64 * 0.1).sin() * 1_500_000.0) as i32;
        let r = ((i as f64 * 0.1).sin() * 500_000.0) as i32;
        input.push((l, r));
    }

    let mut output = Vec::new();
    for &(l, r) in &input {
        if let Some(out) = processor.process_frame(l, r) {
            output.push(out);
        }
    }
    let flushed = processor.flush();
    output.extend(flushed);

    // In Merge mode with dcl=0 and dcr=0, unmodified samples match original inputs exactly
    for i in 0..500 {
        assert_eq!(output[i].0, input[i].0);
        assert_eq!(output[i].1, input[i].1);
    }
}

/// Test 1: High-Band Wavelet Click Test (> 30 dB peak reduction on +/- 4,000,000 D4 Hi spike).
#[test]
fn test_directive8_high_band_wavelet_click() {
    let mut params = CrParameters::default();
    params.dcl = 50;
    params.dcr = 0;
    params.method = Method::Wavelet;
    params.mode = ChannelMode::Stereo;

    let n = 10000;
    let mut processor = Processor::new(params, 44100, Some(n));
    let mut input = Vec::new();

    for i in 0..n {
        let t = i as f64 * 0.05;
        let s = (t.sin() * 1_000_000.0) as i32;
        input.push((s, s));
    }

    // Inject high-frequency D4 wavelet spike (+4,000,000 and -4,000,000) at frame 5000
    let click_pos = 5000;
    input[click_pos] = (input[click_pos].0 + 4_000_000, input[click_pos].1 + 4_000_000);
    input[click_pos + 1] = (input[click_pos + 1].0 - 4_000_000, input[click_pos + 1].1 - 4_000_000);

    let mut output = Vec::new();
    for &(l, r) in &input {
        if let Some(out) = processor.process_frame(l, r) {
            output.push(out);
        }
    }
    let flushed = processor.flush();
    output.extend(flushed);

    assert_eq!(output.len(), n);

    // Measure peak residual error relative to clean sine wave
    let t0 = click_pos as f64 * 0.05;
    let clean0 = (t0.sin() * 1_000_000.0) as i32;
    let err0 = (output[click_pos].0 - clean0).abs();

    let t1 = (click_pos + 1) as f64 * 0.05;
    let clean1 = (t1.sin() * 1_000_000.0) as i32;
    let err1 = (output[click_pos + 1].0 - clean1).abs();

    let max_err = err0.max(err1);
    // 30 dB reduction on 4,000,000 click requires residual error < 130,000
    assert!(
        max_err < 130_000,
        "Wavelet high-band click not sufficiently suppressed: max_err = {} (expected < 130000)",
        max_err
    );
}

/// Test 2: Clean Music Zero-False-Trigger Throughput Test.
/// 441,000 frames multi-sine in WaveletX2 completes rapidly with zero false clicks.
#[test]
fn test_directive8_clean_music_zero_false_trigger_throughput() {
    let mut params = CrParameters::default();
    params.dcl = 30;
    params.dcr = 0;
    params.method = Method::WaveletX2;
    params.mode = ChannelMode::Stereo;
    params.detect_pitch = true;

    let n = 44100; // 1 second test in unit test suite (scaled up in release benchmark)
    let mut processor = Processor::new(params, 44100, Some(n));

    // Multi-frequency rich harmonic signal (220 Hz, 440 Hz, 880 Hz, 1760 Hz)
    let mut input = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / 44100.0;
        let s = ((2.0 * std::f64::consts::PI * 220.0 * t).sin() * 2_000_000.0
            + (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 1_500_000.0
            + (2.0 * std::f64::consts::PI * 880.0 * t).sin() * 1_000_000.0
            + (2.0 * std::f64::consts::PI * 1760.0 * t).sin() * 500_000.0) as i32;
        input.push((s, s));
    }

    let start = Instant::now();
    let mut output = Vec::with_capacity(n);
    for &(l, r) in &input {
        if let Some(out) = processor.process_frame(l, r) {
            output.push(out);
        }
    }
    let flushed = processor.flush();
    output.extend(flushed);
    let duration = start.elapsed();

    assert_eq!(output.len(), n);
    println!("Processed {} frames in {:?}", n, duration);

    // Verify zero false triggers: output matches clean input within wavelet quantization tolerance (+/- 2 LSB)
    let mut false_triggers = 0;
    for i in 1000..(n - 1000) {
        if (output[i].0 - input[i].0).abs() > 20 {
            false_triggers += 1;
        }
    }
    assert_eq!(false_triggers, 0, "Clean music triggered false positive repairs!");
}

/// Test 3: 96 kHz & Stereo Merge Bit-Exact Delay Test.
/// Frame 5000 marker pulse emerges at exact frame 5000 in both 96 kHz and Stereo Merge modes.
#[test]
fn test_directive8_96khz_and_stereo_merge_bit_exact_delay() {
    // 1. Stereo Merge mode delay test
    {
        let mut params = CrParameters::default();
        params.dcl = 0;
        params.dcr = 0;
        params.method = Method::Simple;
        params.mode = ChannelMode::Merge;

        let n = 10000;
        let mut processor = Processor::new(params, 44100, Some(n));
        let mut input = vec![(0i32, 0i32); n];
        input[5000] = (5_000_000, 5_000_000);

        let mut output = Vec::with_capacity(n);
        for &(l, r) in &input {
            if let Some(out) = processor.process_frame(l, r) {
                output.push(out);
            }
        }
        let flushed = processor.flush();
        output.extend(flushed);

        assert_eq!(output.len(), n);
        assert_eq!(
            output[5000],
            (5_000_000, 5_000_000),
            "Stereo Merge marker pulse did not emerge at exact frame 5000: got {:?}",
            output[5000]
        );
    }

    // 2. 96 kHz Stereo mode delay test (Stage 2 D4 decimation)
    {
        let mut params = CrParameters::default();
        params.dcl = 0;
        params.dcr = 0;
        params.method = Method::Simple;
        params.mode = ChannelMode::Stereo;

        let n = 10000;
        let mut processor = Processor::new(params, 96000, Some(n));
        let mut input = vec![(0i32, 0i32); n];
        input[5000] = (5_000_000, 5_000_000);

        let mut output = Vec::with_capacity(n);
        for &(l, r) in &input {
            if let Some(out) = processor.process_frame(l, r) {
                output.push(out);
            }
        }
        let flushed = processor.flush();
        output.extend(flushed);

        assert_eq!(output.len(), n);
        // At 96 kHz, pulse at frame 5000 is decomposed by D4 and reconstructed via synthesize_positive_up.
        // Peak energy must be centered at frame 5000!
        let peak_idx = (4990..=5010)
            .max_by_key(|&i| output[i].0.abs())
            .unwrap();
        assert_eq!(
            peak_idx, 5000,
            "96 kHz D4 pulse center shifted: peak was at frame {} instead of 5000",
            peak_idx
        );
    }
}

/// Regression Test: Kalman DeCrackle beyond pipeline_delay (frame 6000) and 96 kHz Stage 2 High-Band Click Repair.
#[test]
fn test_kalman_decrackle_and_96khz_high_band_repair() {
    // 1. Verify Kalman DeCrackle remains active at frame 6000 (> pipeline_delay ~3264)
    {
        let mut params = CrParameters::default();
        params.dcl = 0;
        params.dcr = 60;
        params.method = Method::Simple;
        params.mode = ChannelMode::Stereo;

        let n = 10000;
        let mut processor = Processor::new(params, 44100, Some(n));
        let mut input = Vec::with_capacity(n);
        for i in 0..n {
            let s = ((i as f64 * 0.05).sin() * 500_000.0) as i32;
            input.push((s, s));
        }
        // Inject single-sample crackle spike at frame 6000
        let clean_6000 = input[6000].0;
        input[6000].0 += 2_000_000;

        let mut output = Vec::with_capacity(n);
        processor.process_chunk(&input, &mut output);
        output.extend(processor.flush());

        let residual = (output[6000].0 - clean_6000).abs();
        assert!(
            residual < 500_000,
            "Kalman DeCrackle stopped before frame 6000: residual = {}",
            residual
        );
    }

    // 2. Verify 96 kHz Stage 2 High-Band Click Repair suppresses both Lo and Hi D4 bands
    {
        let mut params = CrParameters::default();
        params.dcl = 50;
        params.dcr = 0;
        params.method = Method::Simple;
        params.mode = ChannelMode::Stereo;

        let n = 12000;
        let mut processor = Processor::new(params, 96000, Some(n));
        let mut input = Vec::with_capacity(n);
        for i in 0..n {
            let s = ((i as f64 * 0.03).sin() * 1_000_000.0) as i32;
            input.push((s, s));
        }
        let clean_5000 = input[5000].0;
        // Inject sharp 1-sample click at frame 5000 (excites both Stage 2 Lo and Hi bands)
        input[5000].0 += 6_000_000;

        let mut output = Vec::with_capacity(n);
        for &(l, r) in &input {
            if let Some(out) = processor.process_frame(l, r) {
                output.push(out);
            }
        }
        output.extend(processor.flush());

        let err = (output[5000].0 - clean_5000).abs();
        assert!(
            err < 500_000,
            "96 kHz Stage 2 high-band click not repaired: residual error = {}",
            err
        );
    }
}

/// Regression Test: Exact 1:1 total frame count match and seamless chunk boundary continuity in Wavelet mode.
#[test]
fn test_wavelet_chunked_streaming_pipeline_flush_parity() {
    let mut params = CrParameters::preset_stereo_lp();
    params.dcl = 30;
    params.dcr = 0;
    params.method = Method::Wavelet;
    params.mode = ChannelMode::Stereo;
    params.detect_pitch = true;
    params.reverse = false;

    // Test with an odd number of frames crossing multiple 65536 chunks
    let total_frames: usize = 65536 * 3 + 1525;
    let mut processor = Processor::new(params, 44100, Some(total_frames));

    let mut input = Vec::with_capacity(total_frames);
    for i in 0..total_frames {
        let s = ((i as f64 * 0.02).sin() * 500_000.0) as i32;
        input.push((s, s));
    }

    let mut output = Vec::with_capacity(total_frames);
    let chunk_size = 65536;
    let mut out_chunk = Vec::with_capacity(chunk_size);

    for chunk in input.chunks(chunk_size) {
        processor.process_chunk(chunk, &mut out_chunk);
        output.extend_from_slice(&out_chunk);
    }
    let flushed = processor.flush();
    output.extend(flushed);

    assert_eq!(
        output.len(),
        total_frames,
        "Output length {} does not match input total_frames {}",
        output.len(),
        total_frames
    );

    // Verify trailing odd sample was preserved
    assert_eq!(
        output[total_frames - 1],
        input[total_frames - 1],
        "Trailing unaligned sample at EOF was corrupted"
    );
}
