//! Bidirectional AR Reconstruction & Toeplitz Cholesky Solver
//!
//! See docs/engine/OFFLINE_ENGINE.md, sections 7 and 8.

use crate::burg::{burg_fit, reflection_to_predictor, burg_fit_direct};
use crate::params::round_sym;

/// Reusable workspace buffers to prevent dynamic heap allocations in the inner restoration loop.
#[derive(Debug, Clone)]
pub struct ScratchSpace {
    pub fwd_err: Vec<f64>,
    pub bck_err: Vec<f64>,
    pub refl: Vec<f64>,
    pub refl_bck: Vec<f64>,
    pub fwd_coeffs: Vec<f64>,
    pub bck_coeffs: Vec<f64>,
    pub fwd_data: Vec<f64>,
    pub bck_data: Vec<f64>,
    pub a: Vec<f64>,
    pub c: Vec<f64>,
    pub u: Vec<f64>,
    pub rhs: Vec<f64>,
}

impl Default for ScratchSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl ScratchSpace {
    pub fn new() -> Self {
        Self {
            fwd_err: vec![0.0; 2048],
            bck_err: vec![0.0; 2048],
            refl: vec![0.0; 512],
            refl_bck: vec![0.0; 512],
            fwd_coeffs: vec![0.0; 512],
            bck_coeffs: vec![0.0; 512],
            fwd_data: vec![0.0; 4096],
            bck_data: vec![0.0; 4096],
            a: vec![0.0; 130],
            c: vec![0.0; 64 * 64],
            u: vec![0.0; 64 * 64],
            rhs: vec![0.0; 64],
        }
    }
}

/// Generalized bounded repair constructor using 8th-power soft-clamped bidirectional AR extrapolation and quadratic crossfade.
/// Used by both offline Channel (max_repair=2048, max_ar_order=512) and Real-Time Channel (max_repair=128, max_ar_order=256).
pub fn fill_gap_bounded(
    samples: &[i32],
    mask: usize,
    gap: [isize; 2],
    max_repair: usize,
    max_ar_order: usize,
    use_explicit_burg: bool,
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) -> f64 {
    let gap_len = (gap[1] - gap[0]) as usize;
    if gap_len == 0 || gap_len > max_repair {
        return 0.0;
    }

    let ar_order = (2 * gap_len).clamp(50, max_ar_order);
    let burg_window = 2 * ar_order + 256;

    let ms_fwd = if use_explicit_burg {
        crate::burg::burg_fit_direct(
            samples,
            mask,
            burg_window,
            gap[0] - (burg_window as isize),
            ar_order,
            &mut scratch.fwd_err,
            &mut scratch.bck_err,
            &mut scratch.refl,
        )
    } else {
        burg_fit(
            samples,
            mask,
            burg_window,
            gap[0] - (burg_window as isize),
            ar_order,
            &mut scratch.fwd_err,
            &mut scratch.bck_err,
            &mut scratch.refl,
        )
    };
    let rms_fwd = ms_fwd.sqrt().max(1e-12);
    reflection_to_predictor(ar_order, &mut scratch.refl, &mut scratch.fwd_coeffs);

    let ms_bck = if use_explicit_burg {
        crate::burg::burg_fit_direct(
            samples,
            mask,
            burg_window,
            gap[1],
            ar_order,
            &mut scratch.fwd_err,
            &mut scratch.bck_err,
            &mut scratch.refl,
        )
    } else {
        burg_fit(
            samples,
            mask,
            burg_window,
            gap[1],
            ar_order,
            &mut scratch.fwd_err,
            &mut scratch.bck_err,
            &mut scratch.refl,
        )
    };
    let rms_bck = ms_bck.sqrt().max(1e-12);
    reflection_to_predictor(ar_order, &mut scratch.refl, &mut scratch.bck_coeffs);

    let damping = 0.7 + ((gap_len as f64) / 125.0).powi(2);

    // Forward extrapolation
    for i in 0..(ar_order + gap_len) {
        let ring_i = ((gap[0] - (ar_order as isize) + (i as isize)) as usize) & mask;
        scratch.fwd_data[i] = samples[ring_i] as f64;
    }

    for k in 0..gap_len {
        let idx = ar_order + k;
        let mut pred = 0.0;
        let mut tap = idx;
        for i in 0..ar_order {
            tap -= 1;
            pred += scratch.fwd_coeffs[i] * scratch.fwd_data[tap];
        }
        let residual = scratch.fwd_data[idx] - pred;
        let r1 = damping * residual / rms_fwd;
        let r2 = r1 * r1;
        let r4 = r2 * r2;
        let r8 = r4 * r4;
        scratch.fwd_data[idx] = pred + residual / (1.0 + r8);
    }

    // Backward extrapolation
    for i in 0..(gap_len + ar_order) {
        let ring_i = ((gap[0] + (i as isize)) as usize) & mask;
        scratch.bck_data[i] = samples[ring_i] as f64;
    }

    for k in (0..gap_len).rev() {
        let mut pred = 0.0;
        let mut tap = k;
        for i in 0..ar_order {
            tap += 1;
            pred += scratch.bck_coeffs[i] * scratch.bck_data[tap];
        }
        let residual = scratch.bck_data[k] - pred;
        let r1 = damping * residual / rms_bck;
        let r2 = r1 * r1;
        let r4 = r2 * r2;
        let r8 = r4 * r4;
        scratch.bck_data[k] = pred + residual / (1.0 + r8);
    }

    // Piecewise quadratic crossfade
    let denom = (gap_len + 1) as f64;
    for k in 0..gap_len {
        let t = ((k + 1) as f64) / denom;
        let (w_fwd, w_bck) = if t < 0.5 {
            let wf = 1.0 - 2.0 * t * t;
            (wf, 1.0 - wf)
        } else {
            let u = 1.0 - t;
            let wb = 1.0 - 2.0 * u * u;
            (1.0 - wb, wb)
        };
        fixed[k] = round_sym(w_fwd * scratch.fwd_data[ar_order + k] + w_bck * scratch.bck_data[k]);
    }

    rms_fwd.max(rms_bck)
}

/// Primary Channel repair constructor using 8th-power soft-clamped bidirectional AR extrapolation and quadratic crossfade.
#[inline]
pub fn fill_gap(
    samples: &[i32],
    mask: usize,
    gap: [isize; 2],
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) -> f64 {
    fill_gap_bounded(samples, mask, gap, 2048, 512, false, fixed, scratch)
}

/// Fast Toeplitz Cholesky factorization of symmetric positive-definite matrix C into upper factor U such that U^T * U == C.
pub fn cholesky_factor(n: usize, c: &[f64], u: &mut [f64]) -> bool {
    if c.is_empty() || c[0] <= 0.0 || n * n > u.len() || n > c.len() {
        return false;
    }
    u[..n * n].fill(0.0);
    for i in 0..n {
        for j in i..n {
            let mut s = c[j - i];
            for k in 0..i {
                s -= u[k * n + i] * u[k * n + j];
            }
            if i == j {
                if s <= 0.0 {
                    return false;
                }
                u[i * n + i] = s.sqrt();
            } else {
                u[i * n + j] = s / u[i * n + i];
            }
        }
    }
    true
}

/// Triangular solver for U^T * U * x = rhs using forward and backward substitution.
pub fn solve_factored(n: usize, u: &[f64], rhs: &mut [f64], fixed: &mut [i32]) {
    if n * n > u.len() || n > rhs.len() || n > fixed.len() {
        return;
    }
    // Forward substitution: U^T * y = rhs
    for i in 0..n {
        let mut sum = rhs[i];
        let mut col_idx = i;
        for k in 0..i {
            sum -= u[col_idx] * rhs[k];
            col_idx += n;
        }
        rhs[i] = sum / u[col_idx];
    }
    // Backward substitution: U * x = y
    for i in (0..n).rev() {
        let row_offset = i * n;
        let mut sum = rhs[i];
        for k in (i + 1)..n {
            sum -= u[row_offset + k] * rhs[k];
        }
        rhs[i] = sum / u[row_offset + i];
        fixed[i] = round_sym(rhs[i]);
    }
}

/// Constructs prediction error filter a, Toeplitz autocorrelation matrix C, and boundary right-hand side rhs.
pub fn interpolation_system(
    samples: &[i32],
    mask: usize,
    start_frame: isize,
    gap_len: usize,
    ar_order: usize,
    coeffs: &[f64],
    a: &mut [f64],
    c: &mut [f64],
    u: &mut [f64],
    rhs: &mut [f64],
) -> bool {
    if gap_len * gap_len > u.len() || gap_len > rhs.len() || gap_len > c.len() || ar_order >= a.len() {
        return false;
    }
    for i in 0..ar_order {
        a[i] = -coeffs[ar_order - 1 - i];
    }
    a[ar_order] = 1.0;

    let k_max = gap_len.min(ar_order + 1);
    for j in 0..k_max {
        let mut sum = 0.0;
        for i in j..=ar_order {
            sum += a[i] * a[i - j];
        }
        c[j] = sum;
    }
    for j in k_max..gap_len {
        c[j] = 0.0;
    }

    if !cholesky_factor(gap_len, c, u) {
        return false;
    }

    // Right-hand side of the normal equations C * x_gap = rhs.
    //
    // Local index r = 0 corresponds to sample (start_frame - ar_order); the gap occupies
    // r in [ar_order, ar_order + gap_len) and known context lies on BOTH sides:
    // r in [0, ar_order) before the gap and r in [ar_order + gap_len, 2 * ar_order + gap_len)
    // after it. Prediction error e_i = sum_{m=0..=p} a[p - m] * x[i - m], so the unknown at
    // q = p + j appears in e_i for i in [q, q + p] with weight a[p - (i - q)].
    //
    // (Audit fix: this right-hand side previously only iterated i < p + gap_len
    // and mixed up the a[] index offsets, so it never used the samples after the gap and did not
    // produce the least-squares interpolant. Verified against a brute-force
    // least-squares solve in tests/audit_regressions.rs.)
    let p = ar_order;
    let gap_end = p + gap_len;
    for j in 0..gap_len {
        let q = p + j;
        let mut sum = 0.0;
        for i in q..=(q + p) {
            let w_q = a[p - (i - q)];
            for r in (i - p)..=i {
                if r < p || r >= gap_end {
                    let sample_idx =
                        ((start_frame - (p as isize) + (r as isize)) as usize) & mask;
                    sum -= w_q * a[p - (i - r)] * (samples[sample_idx] as f64);
                }
            }
        }
        rhs[j] = sum;
    }
    true
}

/// Decimated SubChannel repair constructor using exact minimum-variance Toeplitz Cholesky AR interpolation.
pub fn fill_gap_subchannel(
    samples: &[i32],
    mask: usize,
    gap: [isize; 2],
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) -> f64 {
    let gap_len = (gap[1] - gap[0]) as usize;
    if gap_len == 0 {
        return 0.0;
    }
    if gap_len > 64 {
        // Wide repair intervals exceed exact Cholesky matrix capacity: fallback directly to bidirectional AR extrapolation
        return fill_gap_bounded(samples, mask, gap, 256, 128, false, fixed, scratch);
    }

    let ar_order = (2 * gap_len).clamp(8, 128);
    let burg_window = 2 * ar_order + 64;

    let ms_fwd = burg_fit_direct(
        samples,
        mask,
        burg_window,
        gap[0] - (burg_window as isize),
        ar_order,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl,
    );
    let ms_bck = burg_fit_direct(
        samples,
        mask,
        burg_window,
        gap[1],
        ar_order,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl_bck,
    );
    let rms = 0.5 * (ms_fwd.sqrt() + ms_bck.sqrt());

    for i in 0..ar_order {
        scratch.refl[i] = 0.5 * (scratch.refl[i] + scratch.refl_bck[i]);
    }
    reflection_to_predictor(ar_order, &mut scratch.refl, &mut scratch.fwd_coeffs);

    let ok = interpolation_system(
        samples,
        mask,
        gap[0],
        gap_len,
        ar_order,
        &scratch.fwd_coeffs[..ar_order],
        &mut scratch.a,
        &mut scratch.c,
        &mut scratch.u,
        &mut scratch.rhs,
    );

    if ok {
        solve_factored(gap_len, &scratch.u, &mut scratch.rhs, fixed);
    } else {
        // Fallback: bidirectional linear AR extrapolation crossfade
        for i in 0..(ar_order + gap_len) {
            let ring_i = ((gap[0] - (ar_order as isize) + (i as isize)) as usize) & mask;
            scratch.fwd_data[i] = samples[ring_i] as f64;
        }
        for k in 0..gap_len {
            let idx = ar_order + k;
            let mut pred = 0.0;
            let mut tap = idx;
            for i in 0..ar_order {
                tap -= 1;
                pred += scratch.fwd_coeffs[i] * scratch.fwd_data[tap];
            }
            scratch.fwd_data[idx] = pred;
        }

        for i in 0..(gap_len + ar_order) {
            let ring_i = ((gap[0] + (i as isize)) as usize) & mask;
            scratch.bck_data[i] = samples[ring_i] as f64;
        }
        for k in (0..gap_len).rev() {
            let mut pred = 0.0;
            let mut tap = k;
            for i in 0..ar_order {
                tap += 1;
                pred += scratch.fwd_coeffs[i] * scratch.bck_data[tap];
            }
            scratch.bck_data[k] = pred;
        }

        let denom = (gap_len + 1) as f64;
        for k in 0..gap_len {
            let w_bck = ((k + 1) as f64) / denom;
            let w_fwd = 1.0 - w_bck;
            fixed[k] =
                round_sym(w_fwd * scratch.fwd_data[ar_order + k] + w_bck * scratch.bck_data[k]);
        }
    }

    rms
}

/// 2nd-order Butterworth IIR low-pass filter step used on SubChannel reference stream.
#[inline]
pub fn lowpass_step(idx: usize, mask: usize, input: &[i32], output: &mut [i32]) -> i32 {
    let idx_m1 = idx.wrapping_sub(1) & mask;
    let idx_m2 = idx.wrapping_sub(2) & mask;
    let y = 0.285811 * ((input[idx] + input[idx_m2]) as f64)
        + 0.5716221 * (input[idx_m1] as f64)
        + 0.05423258 * (output[idx_m1] as f64)
        - 0.19747668 * (output[idx_m2] as f64);
    let val = round_sym(y);
    output[idx] = val;
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toeplitz_cholesky_factorization() {
        // Construct a known 4x4 Toeplitz symmetric matrix C
        let n = 4;
        let c = vec![1.0, 0.5, 0.2, 0.1];
        let mut u = vec![0.0; n * n];

        assert!(cholesky_factor(n, &c, &mut u));

        // Verify U^T * U matches C
        for i in 0..n {
            for j in 0..n {
                let mut sum = 0.0;
                for k in 0..n {
                    // U^T(i, k) is U(k, i)
                    sum += u[k * n + i] * u[k * n + j];
                }
                let expected = if j >= i { c[j - i] } else { c[i - j] };
                assert!(
                    (sum - expected).abs() < 1e-12,
                    "Mismatch at ({}, {}): got {}, expected {}",
                    i,
                    j,
                    sum,
                    expected
                );
            }
        }
    }

    #[test]
    fn test_fill_gap_restoration() {
        let mask = 4095;
        let mut samples = vec![0i32; 4096];
        for i in 0..4096 {
            let t = i as f64 * 0.05;
            samples[i] = (t.sin() * 2_000_000.0) as i32;
        }

        // Corrupt samples 1500..1520 with click
        let gap = [1500, 1520];
        let mut corrupt_samples = samples.clone();
        for i in 1500..1520 {
            corrupt_samples[i] = 7_000_000;
        }

        let mut fixed = vec![0i32; 20];
        let mut scratch = ScratchSpace::new();

        let rms = fill_gap(&corrupt_samples, mask, gap, &mut fixed, &mut scratch);
        assert!(rms > 0.0);

        // Verify restored samples match original clean samples closely
        let mut max_err = 0;
        for i in 0..20 {
            let err = (samples[1500 + i] - fixed[i]).abs();
            if err > max_err {
                max_err = err;
            }
        }
        // Error should be less than 5% of peak amplitude (100,000 / 2,000,000)
        assert!(
            max_err < 100_000,
            "Max error too large: {} vs 2,000,000",
            max_err
        );
    }
}
