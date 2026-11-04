//! Sliding-Window Click Detection State Machine & Edge Localization
//!
//! See docs/engine/OFFLINE_ENGINE.md, sections 5 and 6.

use crate::burg::{burg_fit, reflection_to_predictor};
use crate::flag;
use crate::reconstruct::{fill_gap, fill_gap_subchannel, ScratchSpace};

/// Disjoint subband buffer state avoiding borrow checker conflicts on Channel/SubChannel.
#[derive(Debug, Clone)]
pub struct BandState {
    pub samples: Vec<i32>,
    pub original: Vec<i32>,
    pub filtered: Vec<i32>,
    pub fwd_excit: Vec<f64>,
    pub bck_excit: Vec<f64>,
    pub block_refls: Vec<f64>,
    pub fixed: Vec<i32>,
    pub gap: [isize; 2],
    pub pre_sum: f64,
    pub post_sum: f64,
    pub peak: f64,
    /// Statistics only (they do not affect processing): defects whose repair is cancelled
    /// for being longer than `max_auto_repair`, their total length in this band's samples, and
    /// the last cancelled interval (the scan meets the same defect again on rescans).
    pub cancelled_long: usize,
    pub cancelled_long_len: usize,
    pub last_cancelled: [isize; 2],
}

impl BandState {
    pub fn new(buffer_length: usize, coeff_length: usize) -> Self {
        Self {
            samples: vec![0; buffer_length],
            original: vec![0; buffer_length],
            filtered: vec![0; buffer_length],
            fwd_excit: vec![0.0; buffer_length],
            bck_excit: vec![0.0; buffer_length],
            block_refls: vec![0.0; coeff_length],
            fixed: vec![0; 4096],
            gap: [0, 0],
            pre_sum: 0.0,
            post_sum: 0.0,
            peak: 0.0,
            cancelled_long: 0,
            cancelled_long_len: 0,
            last_cancelled: [0, 0],
        }
    }
}

/// Lowest AR order the edge search uses on the main channel, and on a subchannel.
const MIN_EDGE_ORDER: usize = 20;
const MIN_SUBCHANNEL_EDGE_ORDER: usize = 8;

/// Locates contiguous spans of flagged samples around a given center position.
pub fn marked_run_around(flags: &[u8], mask: usize, center: isize, flag_mask: u8, gap: &mut [isize; 2]) {
    gap[0] = center;
    gap[1] = center;
    let ring_c = (center as usize) & mask;
    if (flags[ring_c] & flag_mask) != 0 {
        let mut p = center;
        while (flags[(p as usize) & mask] & flag_mask) != 0 {
            gap[0] -= 1;
            p -= 1;
        }
        gap[0] += 1;
        p = center;
        while (flags[(p as usize) & mask] & flag_mask) != 0 {
            gap[1] += 1;
            p += 1;
        }
    } else {
        for offset in 1..6 {
            let left_idx = ((center - offset) as usize) & mask;
            if (flags[left_idx] & flag_mask) != 0 {
                gap[0] = center - offset;
                gap[1] = gap[0] + 1;
                let mut p = gap[0];
                while (flags[(p as usize) & mask] & flag_mask) != 0 {
                    gap[0] -= 1;
                    p -= 1;
                }
                gap[0] += 1;
                break;
            }
            let right_idx = ((center + offset) as usize) & mask;
            if (flags[right_idx] & flag_mask) != 0 {
                gap[0] = center + offset;
                gap[1] = gap[0];
                let mut p = gap[1];
                while (flags[(p as usize) & mask] & flag_mask) != 0 {
                    gap[1] += 1;
                    p += 1;
                }
                break;
            }
        }
    }
}

/// Pinpoints the first bad sample of a click transient stepping inward with local AR model.
pub fn find_defect_start(
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
    let ms_err = burg_fit(
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

/// Pinpoints the first good sample after a click transient stepping backward with local AR model.
pub fn find_defect_end(
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
    let ms_err = burg_fit(
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

/// Pinpoints exact first bad and good samples and executes the False-Positive Rejection Gate.
pub fn find_defect_edges(
    samples: &[i32],
    mask: usize,
    span_start: isize,
    span_end: isize,
    peak_len: isize,
    threshold: f64,
    reverse: bool,
    max_auto_repair: usize,
    gap: &mut [isize; 2],
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) {
    let raw_width = span_end - span_start;
    let margin = (raw_width / 3).clamp(peak_len, 3 * peak_len);
    let search_start = span_start - margin;
    let search_end = span_end + margin;
    let ar_order = (raw_width as usize).clamp(MIN_EDGE_ORDER, 512);
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

    if reverse {
        gap[1] = find_defect_end(
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
            gap[0] = find_defect_start(
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
    } else {
        gap[0] = find_defect_start(
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
        if gap[0] < search_end {
            gap[1] = find_defect_end(
                samples,
                mask,
                gap[0],
                search_end,
                lookahead,
                ar_order,
                burg_window,
                threshold,
                scratch,
            );
        } else {
            gap[1] = gap[0];
        }
    }

    // False-Positive Rejection Gate
    if gap[1] > gap[0] && ((gap[1] - gap[0]) as usize) <= max_auto_repair {
        let rms = fill_gap(samples, mask, *gap, fixed, scratch);
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

/// Pinpoints exact first bad and good samples for SubChannel and executes False-Positive Rejection Gate.
pub fn find_defect_edges_subchannel(
    samples: &[i32],
    mask: usize,
    span_start: isize,
    span_end: isize,
    peak_len: isize,
    threshold: f64,
    reverse: bool,
    max_auto_repair: usize,
    gap: &mut [isize; 2],
    fixed: &mut [i32],
    scratch: &mut ScratchSpace,
) {
    let raw_width = span_end - span_start;
    let margin = (raw_width / 3).clamp(peak_len, 3 * peak_len);
    let search_start = span_start - margin;
    let search_end = span_end + margin;
    let ar_order = (raw_width as usize).clamp(MIN_SUBCHANNEL_EDGE_ORDER, 512);
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

    if reverse {
        gap[1] = find_defect_end(
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
            gap[0] = find_defect_start(
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
    } else {
        gap[0] = find_defect_start(
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
        if gap[0] < search_end {
            gap[1] = find_defect_end(
                samples,
                mask,
                gap[0],
                search_end,
                lookahead,
                ar_order,
                burg_window,
                threshold,
                scratch,
            );
        } else {
            gap[1] = gap[0];
        }
    }

    // False-Positive Rejection Gate using fill_gap_subchannel
    if gap[1] > gap[0] && ((gap[1] - gap[0]) as usize) <= max_auto_repair {
        let rms = fill_gap_subchannel(samples, mask, *gap, fixed, scratch);
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

/// High-level bad sample analysis for primary Channel, including adjacent repair merging.
pub fn refine_click(
    band: &mut BandState,
    flags: &[u8],
    mask: usize,
    start: isize,
    end: isize,
    peak_len: isize,
    normal_threshold: f64,
    lower_threshold: f64,
    is_wavelet: bool,
    is_reverse: bool,
    max_auto_repair: usize,
    scratch: &mut ScratchSpace,
) {
    let mut hint_span = [0isize; 2];
    marked_run_around(flags, mask, (start + end) / 2, flag::HINT, &mut hint_span);

    let threshold = if hint_span[1] != hint_span[0] {
        lower_threshold
    } else {
        normal_threshold
    };

    find_defect_edges(
        &band.samples,
        mask,
        start,
        end,
        peak_len,
        threshold,
        is_reverse,
        max_auto_repair,
        &mut band.gap,
        &mut band.fixed,
        scratch,
    );

    if band.gap[1] <= band.gap[0] {
        return;
    }

    let adj_mask = flag::repaired_mask(is_wavelet);
    let mut merged = false;

    // Check adjacent repairs within 1-2 samples
    if (flags[((band.gap[0] - 1) as usize) & mask] & adj_mask) != 0 {
        merged = true;
        let mut p = band.gap[0] - 1;
        while (flags[(p as usize) & mask] & adj_mask) != 0 {
            band.gap[0] -= 1;
            p -= 1;
        }
    } else if (flags[((band.gap[0] - 2) as usize) & mask] & adj_mask) != 0 {
        merged = true;
        band.gap[0] -= 1;
        let mut p = band.gap[0] - 1;
        while (flags[(p as usize) & mask] & adj_mask) != 0 {
            band.gap[0] -= 1;
            p -= 1;
        }
    }

    if (flags[(band.gap[1] as usize) & mask] & adj_mask) != 0 {
        merged = true;
        let mut p = band.gap[1];
        while (flags[(p as usize) & mask] & adj_mask) != 0 {
            band.gap[1] += 1;
            p += 1;
        }
    } else if (flags[((band.gap[1] + 1) as usize) & mask] & adj_mask) != 0 {
        merged = true;
        band.gap[1] += 1;
        let mut p = band.gap[1];
        while (flags[(p as usize) & mask] & adj_mask) != 0 {
            band.gap[1] += 1;
            p += 1;
        }
    }

    if hint_span[1] != hint_span[0] {
        let min_lookback = hint_span[0].max(2 * band.gap[0] - band.gap[1]);
        let mut p = band.gap[0] - 1;
        while p >= min_lookback {
            if (flags[(p as usize) & mask] & adj_mask) != 0 {
                merged = true;
                band.gap[0] = p;
            }
            p -= 1;
        }
    }

    if ((band.gap[1] - band.gap[0]) as usize) > max_auto_repair {
        // Statistics only: count each over-long defect once, however many rescans meet it.
        let [s, e] = band.gap;
        if e <= band.last_cancelled[0] || s >= band.last_cancelled[1] {
            band.cancelled_long += 1;
            band.cancelled_long_len += (e - s) as usize;
        }
        band.last_cancelled = [s, e];
        band.gap[1] = band.gap[0];
    } else if merged {
        fill_gap(&band.samples, mask, band.gap, &mut band.fixed, scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_marked_run_around() {
        let mask = 1023;
        let mut flags = vec![0u8; 1024];
        for i in 100..110 {
            flags[i] = flag::REPAIRED;
        }

        let mut gap = [0isize; 2];
        marked_run_around(&flags, mask, 105, flag::REPAIRED, &mut gap);
        assert_eq!(gap, [100, 110]);

        let mut info_offset = [0isize; 2];
        marked_run_around(&flags, mask, 112, flag::REPAIRED, &mut info_offset);
        assert_eq!(info_offset, [100, 110]);
    }
}
