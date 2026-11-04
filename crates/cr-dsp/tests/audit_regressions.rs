//! Regression tests for defects found during the DSP audit.
//!
//! Each test targets one confirmed defect and fails on the pre-audit code.

use cr_dsp::params::{ChannelMode, CrParameters, Method, RtParameters, RtSensitivity};
use cr_dsp::process::Processor;
use cr_dsp::reconstruct::{interpolation_system, solve_factored};

/// Deterministic LCG noise in [-0.5, 0.5).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f64 / (1u64 << 31) as f64) - 0.5
    }
}

/// Brute-force least-squares gap interpolation for predictor `c`
/// (e_i = x_i - sum_j c_j x_{i-1-j}), minimising the sum of e_i^2 over every error term that
/// touches the gap. Returns the interpolated gap samples.
fn brute_force_interpolate(x: &[f64], g0: usize, gl: usize, c: &[f64]) -> Vec<f64> {
    let p = c.len();
    let mut ata = vec![vec![0.0; gl]; gl];
    let mut atb = vec![0.0; gl];
    for i in p..x.len() {
        let mut row = vec![0.0; gl];
        let mut b = 0.0;
        for m in 0..=p {
            let w = if m == 0 { 1.0 } else { -c[m - 1] };
            let idx = i - m;
            if idx >= g0 && idx < g0 + gl {
                row[idx - g0] += w;
            } else {
                b -= w * x[idx];
            }
        }
        if row.iter().all(|v| *v == 0.0) {
            continue;
        }
        for a in 0..gl {
            atb[a] += row[a] * b;
            for bb in 0..gl {
                ata[a][bb] += row[a] * row[bb];
            }
        }
    }
    for k in 0..gl {
        let piv = ata[k][k];
        for j in k..gl {
            ata[k][j] /= piv;
        }
        atb[k] /= piv;
        for r in 0..gl {
            if r != k {
                let f = ata[r][k];
                for j in k..gl {
                    ata[r][j] -= f * ata[k][j];
                }
                atb[r] -= f * atb[k];
            }
        }
    }
    atb
}

/// SubChannel Toeplitz interpolation (engine doc 8) must equal the exact least-squares interpolant.
/// Pre-audit, the right-hand side never used samples after the gap and the result was unrelated
/// to the signal.
#[test]
fn audit_subchannel_aat_matches_least_squares() {
    let mut rng = Lcg(12345);
    let n = 400usize;
    let mut x = vec![0.0f64; n];
    for i in 2..n {
        x[i] = 1.6 * x[i - 1] - 0.9 * x[i - 2] + 10_000.0 * rng.next();
    }
    let mask = 511usize;
    let mut samples = vec![0i32; 512];
    for i in 0..n {
        samples[i] = x[i].round() as i32;
    }
    let xi: Vec<f64> = samples[..n].iter().map(|v| *v as f64).collect();

    for (c, gl) in [(vec![1.6, -0.9], 6usize), (vec![1.6, -0.9], 1), (vec![1.2, -0.5, 0.1, 0.05], 12)] {
        let p = c.len();
        let g0 = 200usize;
        let mut a = vec![0.0; 130];
        let mut cm = vec![0.0; 4096];
        let mut u = vec![0.0; 4096];
        let mut rhs = vec![0.0; 64];
        assert!(interpolation_system(&samples, mask, g0 as isize, gl, p, &c, &mut a, &mut cm, &mut u, &mut rhs));
        let mut fixed = vec![0i32; 64];
        solve_factored(gl, &u, &mut rhs, &mut fixed);

        let pad = 50;
        let local: Vec<f64> = xi[g0 - p - pad..g0 + gl + p + pad].to_vec();
        let bf = brute_force_interpolate(&local, p + pad, gl, &c);
        let err = (0..gl).map(|k| (fixed[k] as f64 - bf[k]).abs()).fold(0.0, f64::max);
        assert!(err < 1.0, "p={} gap={}: the interpolation differs from least squares by {}", p, gl, err);
    }
}

fn clicky_test_signal(n: usize, seed: u64) -> (Vec<(i32, i32)>, Vec<usize>) {
    let mut rng = Lcg(seed);
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / 44_100.0;
        let s = (2.0 * std::f64::consts::PI * 330.0 * t).sin() * 2_000_000.0
            + (2.0 * std::f64::consts::PI * 1_250.0 * t).sin() * 600_000.0
            + rng.next() * 40_000.0;
        let v = s as i32;
        frames.push((v, v));
    }
    let mut clicks = Vec::new();
    let mut pos = 9_000;
    let mut sign = 1i32;
    while pos + 20 < n - 9_000 {
        for k in 0..4 {
            frames[pos + k].0 += sign * 3_500_000;
            frames[pos + k].1 += sign * 3_500_000;
        }
        clicks.push(pos);
        sign = -sign;
        pos += 3_731;
    }
    (frames, clicks)
}

/// Reverse-mode (engine doc 5) detection must work as well as forward mode. Pre-audit the cached
/// window sums in scan_backward drifted on ~99.8% of steps, so reverse presets behaved
/// erratically.
#[test]
fn audit_reverse_mode_detects_clicks_like_forward_mode() {
    let n = 120_000;
    let (input, clicks) = clicky_test_signal(n, 99);
    let (clean, _) = {
        let mut rng = Lcg(99);
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / 44_100.0;
            let s = (2.0 * std::f64::consts::PI * 330.0 * t).sin() * 2_000_000.0
                + (2.0 * std::f64::consts::PI * 1_250.0 * t).sin() * 600_000.0
                + rng.next() * 40_000.0;
            v.push((s as i32, s as i32));
        }
        (v, ())
    };

    for method in [Method::Simple, Method::Wavelet] {
        for reverse in [false, true] {
            let mut params = CrParameters::preset_stereo_lp();
            params.method = method;
            params.reverse = reverse;
            params.mode = ChannelMode::Mono;
            let (out, _stats) = Processor::process_slice_aligned(params, 44_100, &input);
            assert_eq!(out.len(), n);

            let mut residual_peak = 0i64;
            for &c in &clicks {
                for k in c.saturating_sub(4)..c + 8 {
                    residual_peak = residual_peak.max((out[k].0 as i64 - clean[k].0 as i64).abs());
                }
            }
            assert!(
                residual_peak < 700_000,
                "{:?} reverse={}: clicks not removed (worst residual {} vs injected 3.5M)",
                method,
                reverse,
                residual_peak
            );
        }
    }
}

/// Merge mode (engine doc 3): with all energy in the left channel, the repair delta must be
/// distributed entirely to the left channel. Pre-audit a permanent 800M phantom level kept
/// pulling the balance toward 50/50, injecting part of every repair into the silent right
/// channel as a new click.
#[test]
fn audit_merge_mode_does_not_leak_repairs_into_silent_channel() {
    let n = 60_000;
    let (mut input, _clicks) = clicky_test_signal(n, 7);
    for f in input.iter_mut() {
        f.1 = 0;
    }
    let mut params = CrParameters::preset_mono_lp();
    params.mode = ChannelMode::Merge;
    let (out, stats) = Processor::process_slice_aligned(params, 44_100, &input);
    assert_eq!(out.len(), n);
    assert!(stats.declick_events > 0, "expected the merged channel to repair clicks");
    let leaked = out.iter().map(|f| (f.1 as i64).abs()).max().unwrap();
    assert!(leaked <= 1, "repair leaked into the silent right channel (peak {})", leaked);
}

/// Realtime engine doc 2: the RT window constants already include the backward-scan +1;
/// the `reverse` flag must not widen the window a second time.
#[test]
fn audit_rt_reverse_flag_does_not_change_window() {
    for method in [Method::Simple, Method::Wavelet] {
        let mut p = RtParameters::default();
        p.method = method;
        p.sensitivity = RtSensitivity::Stepped(5);
        p.reverse = false;
        let a = p.compute_derived();
        p.reverse = true;
        let b = p.compute_derived();
        assert_eq!(a, b);
    }
}

/// Offline scanner (engine doc 5): when the Lo band's click ends before the Hi band's, a Lo
/// re-trigger on the click's tail used to overwrite the pending Lo onset, so the analysis only
/// saw the tail and the click was never repaired. Pre-audit, the default Stereo LP preset
/// (Wavelet, dcl=50) left every one of these 13-sample clicks untouched.
#[test]
fn audit_staggered_band_exit_does_not_drop_click() {
    let n = 120_000;
    let mut rng = Lcg(5);
    let mut clean = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / 44_100.0;
        let s = (2.0 * std::f64::consts::PI * 330.0 * t).sin() * 2e6
            + (2.0 * std::f64::consts::PI * 1250.0 * t).sin() * 6e5
            + rng.next() * 4e4;
        clean.push((s as i32, s as i32));
    }
    let mut input = clean.clone();
    let mut clicks = Vec::new();
    let mut pos = 9_000 + 5_844; // reproduces the original failing alignment
    while pos + 40 < n - 9_000 {
        for j in 0..13 {
            input[pos + j].0 += 1_600_000;
            input[pos + j].1 += 1_600_000;
        }
        clicks.push(pos);
        pos += 27_110;
    }
    for method in [Method::Wavelet, Method::WaveletX2] {
        for dcl in [50, 60] {
            let mut p = CrParameters::preset_stereo_lp();
            p.method = method;
            p.dcl = dcl;
            p.mode = ChannelMode::Mono;
            let (out, _) = Processor::process_slice_aligned(p, 44_100, &input);
            for &c in &clicks {
                let resid = (c..c + 13)
                    .map(|j| (out[j].0 as i64 - clean[j].0 as i64).abs())
                    .max()
                    .unwrap();
                assert!(resid < 500_000, "{:?} dcl={} click at {} not repaired (residual {})", method, dcl, c, resid);
            }
        }
    }
}
