//! Real-Time Click Detection & Edge Localization
//!
//! See docs/engine/REALTIME_ENGINE.md, section 5.

use crate::burg::{reflection_to_predictor, burg_fit_direct};
use crate::reconstruct::{fill_gap_bounded, ScratchSpace};

pub const CHANNEL_BUFFER_LEN: usize = 4096;
pub const CHANNEL_BUFFER_MASK: usize = 4095;
pub const SUPER_BUFFER_LEN: usize = 32768;
pub const SUPER_BUFFER_MASK: usize = 32767;
pub const MAX_REPAIR: usize = 128;
pub const MAX_PRED: usize = 256;
/// Lowest AR order the edge search uses.
const MIN_EDGE_ORDER: usize = 20;

/// Pinpoints the first bad sample of a click transient stepping inward using `burg_fit_direct`.
pub fn find_defect_start_rt(
    samples: &[i32],
    mask: usize,
    span_start: isize,
    span_end: isize,
    lookahead: isize,
    ar_order: usize,
    burg_window: usize,
    threshold: f64,
    scratch: &mut ScratchSpace,
) -> isize {
    let ms_err = burg_fit_direct(
        samples,
        mask,
        burg_window,
        span_start - (burg_window as isize),
        ar_order,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl,
    );
    let limit = threshold * ms_err.sqrt().max(1e-12);
    reflection_to_predictor(ar_order, &mut scratch.refl, &mut scratch.fwd_coeffs);

    let mut prev_res = 0.0;
    let mut first_bad = span_end;

    for i in (span_start - lookahead)..=span_end {
        let target_idx = ((i + lookahead) as usize) & mask;
        let target_val = samples[target_idx] as f64;
        let mut pred = 0.0;
        let mut tap_idx = target_idx;
        for j in 0..ar_order {
            tap_idx = (tap_idx.wrapping_sub(1)) & mask;
            pred += scratch.fwd_coeffs[j] * (samples[tap_idx] as f64);
        }
        let curr_res = (target_val - pred).abs();
        if prev_res + curr_res > 1.7 * limit {
            if prev_res > limit {
                first_bad -= 1;
                break;
            }
            if curr_res > limit {
                break;
            }
        }
        prev_res = curr_res;
        first_bad = i + 1;
    }
    first_bad.max(span_start)
}

/// Pinpoints the first good sample after a click transient stepping backward using `burg_fit_direct`.
pub fn find_defect_end_rt(
    samples: &[i32],
    mask: usize,
    span_start: isize,
    span_end: isize,
    lookahead: isize,
    ar_order: usize,
    burg_window: usize,
    threshold: f64,
    scratch: &mut ScratchSpace,
) -> isize {
    let ms_err = burg_fit_direct(
        samples,
        mask,
        burg_window,
        span_end,
        ar_order,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl,
    );
    let limit = threshold * ms_err.sqrt().max(1e-12);
    reflection_to_predictor(ar_order, &mut scratch.refl, &mut scratch.bck_coeffs);

    let mut prev_res = 0.0;
    let mut next_good = span_start;

    let mut i = span_end + lookahead;
    while i >= span_start + lookahead {
        let target_idx = ((i - lookahead) as usize) & mask;
        let target_val = samples[target_idx] as f64;
        let mut pred = 0.0;
        let mut tap_idx = target_idx;
        for j in 0..ar_order {
            tap_idx = (tap_idx + 1) & mask;
            pred += scratch.bck_coeffs[j] * (samples[tap_idx] as f64);
        }
        let curr_res = (target_val - pred).abs();
        if prev_res + curr_res > 1.7 * limit {
            if prev_res > limit {
                next_good += 1;
                break;
            }
            if curr_res > limit {
                break;
            }
        }
        prev_res = curr_res;
        next_good = i - 1;
        i -= 1;
    }
    next_good = (next_good + 1).min(span_end);
    if next_good <= span_start {
        span_start
    } else {
        next_good
    }
}

/// Pinpoints exact first bad and good samples for Real-Time channel and executes False-Positive Rejection Gate.
pub fn find_defect_edges_rt(
    samples: &[i32],
    mask: usize,
    span_start: isize,
    span_end: isize,
    peak_len: isize,
    threshold: f64,
    max_repair: usize,
    gap: &mut [isize; 2],
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) {
    let raw_width = span_end - span_start;
    let margin = (raw_width / 3).clamp(peak_len, 3 * peak_len);
    let search_start = span_start - margin;
    let search_end = span_end + margin;
    let ar_order = (raw_width as usize).clamp(MIN_EDGE_ORDER, MAX_PRED);
    let burg_window = 2 * ar_order + 256;

    let lookahead: isize = if raw_width < 4 {
        1
    } else if raw_width < 19 {
        2
    } else if raw_width < 40 {
        3
    } else {
        4
    };

    // In RT (backward scan), find_defect_end runs first on [search_start, search_end]
    gap[1] = find_defect_end_rt(
        samples,
        mask,
        search_start,
        search_end,
        lookahead,
        ar_order,
        burg_window,
        threshold,
        scratch,
    );

    if gap[1] > search_start {
        gap[0] = find_defect_start_rt(
            samples,
            mask,
            search_start,
            gap[1],
            lookahead,
            ar_order,
            burg_window,
            threshold,
            scratch,
        );
    } else {
        gap[0] = gap[1];
    }

    // Mandatory RT range guard
    if gap[0] > span_end || gap[1] < span_start {
        gap[0] = span_start;
        gap[1] = span_start;
    }

    // False-Positive Rejection Gate
    if gap[1] > gap[0] && ((gap[1] - gap[0]) as usize) <= max_repair {
        let rms = fill_gap_bounded(
            samples,
            mask,
            *gap,
            max_repair,
            MAX_PRED,
            true,
            fixed,
            scratch,
        );
        let min_diff = threshold * rms;
        let gap_len = (gap[1] - gap[0]) as usize;
        let mut ring_idx = (gap[0] as usize) & mask;
        let mut significant = false;
        for i in 0..gap_len {
            if ((samples[ring_idx] - fixed[i]) as f64).abs() > min_diff {
                significant = true;
                break;
            }
            ring_idx = (ring_idx + 1) & mask;
        }
        if !significant {
            gap[1] = gap[0];
        }
    } else {
        gap[1] = gap[0];
    }
}
