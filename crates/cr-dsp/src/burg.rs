//! Burg autoregressive (AR) model fitting, and the prediction error the click detector reads.
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 4.
//!
//! A fit over a window of `len` samples leaves the stage-`order` prediction errors in two
//! buffers, indexed by position in the window:
//! - `fwd[i]`: forward error of window sample `i + 1` (predicted from the samples before it);
//! - `bck[i]`: backward error of window sample `i` (predicted from the samples after it);
//! for `i` in `0..len - 1`, and the reflection coefficients in `refl[0..order]`.

/// Reads window sample `i` of the ring buffer `samples` starting at `start`.
#[inline(always)]
fn window_sample(samples: &[i32], mask: usize, start: isize, i: usize) -> f64 {
    samples[((start + i as isize) as usize) & mask] as f64
}

/// Burg fit with the denominator carried from stage to stage (each stage removes the two
/// error terms that drop out of the sums). Returns the mean square prediction error.
pub fn burg_fit(
    samples: &[i32],
    mask: usize,
    len: usize,
    start: isize,
    order: usize,
    fwd: &mut [f64],
    bck: &mut [f64],
    refl: &mut [f64],
) -> f64 {
    let pairs0 = len - 1;
    let mut energy = 0.0;
    let mut den = 0.0;
    for i in 0..pairs0 {
        let a = window_sample(samples, mask, start, i);
        let b = window_sample(samples, mask, start, i + 1);
        energy += a * a;
        bck[i] = a;
        fwd[i] = b;
        den += a * a + b * b;
    }
    let mut mean_sq = energy / (len as f64);

    let mut k = 0.0;
    for m in 0..order {
        // Stage m pairs fwd[m + i] with bck[i] for i in 0..pairs.
        let pairs = pairs0 - m;
        let mut num = 0.0;
        for i in 0..pairs {
            num += fwd[m + i] * bck[i];
        }
        if m > 0 {
            den *= 1.0 - k * k;
            den -= fwd[m - 1] * fwd[m - 1] + bck[pairs] * bck[pairs];
        }
        if den.is_nan() || den <= 0.0 {
            refl[m..order].fill(0.0);
            break;
        }
        k = 2.0 * num / den;
        refl[m] = k;
        mean_sq *= 1.0 - k * k;

        for i in 0..pairs {
            let b = bck[i];
            bck[i] = b - fwd[m + i] * k;
            fwd[m + i] -= b * k;
        }
    }
    mean_sq
}

/// Burg fit with the denominator summed afresh at every stage. Used by the subchannels and
/// the real-time engine. Returns the mean square prediction error.
pub fn burg_fit_direct(
    samples: &[i32],
    mask: usize,
    len: usize,
    start: isize,
    order: usize,
    fwd: &mut [f64],
    bck: &mut [f64],
    refl: &mut [f64],
) -> f64 {
    let pairs0 = len - 1;
    let mut energy = 0.0;
    for i in 0..pairs0 {
        let a = window_sample(samples, mask, start, i);
        energy += a * a;
        bck[i] = a;
        fwd[i] = window_sample(samples, mask, start, i + 1);
    }
    let mut mean_sq = energy / (len as f64);

    for m in 0..order {
        let pairs = pairs0 - m;
        let mut num = 0.0;
        let mut den = 0.0;
        for i in 0..pairs {
            let b = bck[i];
            let f = fwd[m + i];
            num += b * f;
            den += b * b + f * f;
        }
        let k = if den > 0.0 { 2.0 * num / den } else { 0.0 };
        refl[m] = k;
        mean_sq *= 1.0 - k * k;

        for i in 0..pairs {
            let b = bck[i];
            let f = fwd[m + i];
            bck[i] = b - k * f;
            fwd[m + i] = f - k * b;
        }
    }
    mean_sq
}

/// Longest AR order the engines fit.
const MAX_ORDER: usize = 512;

/// Levinson-Durbin step-up: turns reflection coefficients `refl[0..order]` into predictor
/// coefficients `pred[0..order]` (`x[n]` is predicted as `sum(pred[j] * x[n - 1 - j])`).
pub fn reflection_to_predictor(order: usize, refl: &[f64], pred: &mut [f64]) {
    assert!(order <= MAX_ORDER);
    pred[..order].copy_from_slice(&refl[..order]);
    let mut prev = [0.0f64; MAX_ORDER];
    for i in 1..order {
        prev[..i].copy_from_slice(&pred[..i]);
        let k = pred[i];
        for j in 0..i {
            pred[j] = prev[j] - k * prev[i - 1 - j];
        }
    }
}

/// Recomputes the prediction error for every 256-sample block touching `[start_frame,
/// end_frame)` (main channel): an order-10 Burg fit over each block and 128 samples either
/// side, the absolute errors stored per sample, and the reflection coefficients kept per
/// block for DeCrackle.
pub fn update_excitation(
    samples: &[i32],
    frame_count: isize,
    buffer_length: isize,
    mask: usize,
    coeff_length: usize,
    start_frame: isize,
    end_frame: isize,
    fwd_excit: &mut [f64],
    bck_excit: &mut [f64],
    block_refl: &mut [f64],
    fwd: &mut [f64],
    bck: &mut [f64],
    refl: &mut [f64],
) {
    if start_frame < frame_count - buffer_length + 256 {
        return;
    }
    // Block boundaries, with integer division toward zero.
    let block_start = ((start_frame - 128) / 256) * 256;
    let block_end = ((end_frame + 384 - 1) / 256) * 256;
    let num_coeff_blocks = (coeff_length / 10) as isize;

    let mut block_base = block_start;
    while block_base < block_end {
        burg_fit(samples, mask, 512, block_base - 128, 10, fwd, bck, refl);
        for j in 0..256 {
            let s = ((block_base + j as isize) as usize) & mask;
            // Window sample 128 + j is block sample j.
            fwd_excit[s] = fwd[127 + j].abs();
            bck_excit[s] = bck[128 + j].abs();
        }
        let slot = 10 * (block_base / 256).rem_euclid(num_coeff_blocks) as usize;
        block_refl[slot..slot + 10].copy_from_slice(&refl[..10]);
        block_base += 256;
    }
}

/// The subchannel form of `update_excitation`: the direct fit, no coefficients kept, and 1
/// added to every error as a floor.
pub fn update_excitation_floored(
    samples: &[i32],
    frame_count: isize,
    buffer_length: isize,
    mask: usize,
    start_frame: isize,
    end_frame: isize,
    fwd_excit: &mut [f64],
    bck_excit: &mut [f64],
    fwd: &mut [f64],
    bck: &mut [f64],
    refl: &mut [f64],
) {
    if start_frame < frame_count - buffer_length + 256 {
        return;
    }
    let block_start = ((start_frame - 128) / 256) * 256;
    let block_end = ((end_frame + 384 - 1) / 256) * 256;

    let mut block_base = block_start;
    while block_base < block_end {
        burg_fit_direct(samples, mask, 512, block_base - 128, 10, fwd, bck, refl);
        for j in 0..256 {
            let s = ((block_base + j as isize) as usize) & mask;
            fwd_excit[s] = fwd[127 + j].abs() + 1.0;
            bck_excit[s] = bck[128 + j].abs() + 1.0;
        }
        block_base += 256;
    }
}

/// The real-time form of `update_excitation`: the direct fit, no coefficients kept, block
/// boundaries rounded toward minus infinity.
pub fn update_excitation_rt(
    samples: &[i32],
    frame_count: isize,
    buffer_length: isize,
    mask: usize,
    start_frame: isize,
    end_frame: isize,
    fwd_excit: &mut [f64],
    bck_excit: &mut [f64],
    fwd: &mut [f64],
    bck: &mut [f64],
    refl: &mut [f64],
) {
    if start_frame < frame_count - buffer_length + 256 {
        return;
    }
    let block_start = (start_frame - 128).div_euclid(256) * 256;
    let block_end = (end_frame + 384 - 1).div_euclid(256) * 256;

    let mut block_base = block_start;
    while block_base < block_end {
        burg_fit_direct(samples, mask, 512, block_base - 128, 10, fwd, bck, refl);
        for j in 0..256 {
            let s = ((block_base + j as isize) as usize) & mask;
            fwd_excit[s] = fwd[127 + j].abs();
            bck_excit[s] = bck[128 + j].abs();
        }
        block_base += 256;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_fits_agree() {
        let mask = 1023;
        let mut samples = vec![0i32; 1024];
        for i in 0..1024 {
            let t = i as f64 * 0.1;
            samples[i] = ((t.sin() + 0.5 * (2.0 * t).cos()) * 1_000_000.0) as i32;
        }

        let (mut fwd1, mut bck1, mut refl1) = (vec![0.0; 512], vec![0.0; 512], vec![0.0; 10]);
        let (mut fwd2, mut bck2, mut refl2) = (vec![0.0; 512], vec![0.0; 512], vec![0.0; 10]);

        let err1 = burg_fit(&samples, mask, 512, 100, 10, &mut fwd1, &mut bck1, &mut refl1);
        let err2 = burg_fit_direct(&samples, mask, 512, 100, 10, &mut fwd2, &mut bck2, &mut refl2);

        assert!((err1 - err2).abs() / err1 < 1e-3);
        for m in 0..10 {
            assert!(refl1[m].abs() < 1.0);
            assert!((refl1[m] - refl2[m]).abs() < 1e-3);
        }
    }

    #[test]
    fn step_up_leaves_its_input_alone() {
        let refl = vec![0.5, -0.3, 0.2];
        let mut pred = vec![0.0; 3];
        reflection_to_predictor(3, &refl, &mut pred);
        assert_eq!(refl, vec![0.5, -0.3, 0.2]);
        // Stage 1: [0.5 - (-0.3)(0.5), -0.3] = [0.65, -0.3];
        // stage 2: [0.65 - 0.2(-0.3), -0.3 - 0.2(0.65), 0.2] = [0.71, -0.43, 0.2].
        let want = [0.71, -0.43, 0.2];
        for (p, w) in pred.iter().zip(want) {
            assert!((p - w).abs() < 1e-12, "{:?}", pred);
        }
    }
}
