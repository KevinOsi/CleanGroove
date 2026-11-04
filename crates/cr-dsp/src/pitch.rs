//! Harmonic Pitch & Brass Protection Engine
//!
//! See docs/engine/OFFLINE_ENGINE.md, section 10.

use crate::burg::{burg_fit, update_excitation};
use crate::flag;
use crate::detector::marked_run_around;
use crate::reconstruct::ScratchSpace;

/// Detects quasi-periodic brass wavefronts and vocal glottal pulses, reverting false-positive repairs.
pub fn protect_pitched_repairs(
    lo_samples: &mut [i32],
    lo_original: &[i32],
    hi_samples: &mut [i32],
    hi_original: &[i32],
    flags: &mut [u8],
    mask: usize,
    buffer_length: isize,
    coeff_length: usize,
    frame_count: isize,
    pitch_count: &mut isize,
    repairs_pending: &mut usize,
    protected_total: &mut usize,
    min_pitch: usize,
    is_wavelet: bool,
    lo_fwd_excit: &mut [f64],
    lo_bck_excit: &mut [f64],
    lo_block_refls: &mut [f64],
    hi_fwd_excit: &mut [f64],
    hi_bck_excit: &mut [f64],
    hi_block_refls: &mut [f64],
    scratch: &mut ScratchSpace,
) {
    let mut pulse = [0isize; 2];
    marked_run_around(flags, mask, *pitch_count, flag::PITCH_CANDIDATE, &mut pulse);
    *pitch_count = pulse[1];

    if pulse[1] <= pulse[0] {
        return;
    }

    // Run Order-256 Burg on 768 samples of original unrepaired audio
    burg_fit(
        lo_original,
        mask,
        768,
        *pitch_count,
        256,
        &mut scratch.fwd_err,
        &mut scratch.bck_err,
        &mut scratch.refl,
    );

    let mut max_k = 0.0;
    let mut curr_pitch: isize = min_pitch as isize;
    for i in min_pitch..256 {
        let abs_k = scratch.refl[i].abs();
        if abs_k > max_k {
            curr_pitch = i as isize;
            max_k = abs_k;
        }
    }

    if curr_pitch <= 0 {
        return;
    }

    let max_pulse_width = (curr_pitch / 3).min(curr_pitch / 6 + 10);
    if (pulse[1] - pulse[0]) > max_pulse_width {
        return;
    }

    let center = (pulse[0] + pulse[1]) / 2;
    let max_periods = ((768 / curr_pitch) as usize).min(11);

    let mut pitch_start = [0isize; 32];
    let mut pitch_end = [0isize; 32];
    let mut matched_count = 0;

    pitch_start[matched_count] = pulse[0];
    pitch_end[matched_count] = pulse[1];
    matched_count += 1;

    // Scan forward: +curr_pitch with adaptive drift tracking
    let mut missed = false;
    let mut last_matched_center = center;
    for _k in 1..=max_periods {
        let target = last_matched_center + curr_pitch;
        let mut matched = [0isize; 2];
        marked_run_around(flags, mask, target, flag::PITCH_CANDIDATE, &mut matched);
        if matched[1] > matched[0] && (matched[1] - matched[0]) <= max_pulse_width {
            let matched_center = (matched[0] + matched[1]) / 2;
            let diff = matched_center - last_matched_center;
            if diff > curr_pitch {
                curr_pitch += 1;
            } else if diff < curr_pitch {
                curr_pitch -= 1;
            }
            last_matched_center = matched_center;
            pitch_start[matched_count] = matched[0];
            pitch_end[matched_count] = matched[1];
            matched_count += 1;
        } else if !missed {
            missed = true;
            last_matched_center = target;
        } else {
            break;
        }
    }

    // Scan backward: -curr_pitch with adaptive drift tracking
    missed = false;
    last_matched_center = center;
    for _k in 1..=max_periods {
        let target = last_matched_center - curr_pitch;
        let mut matched = [0isize; 2];
        marked_run_around(flags, mask, target, flag::PITCH_CANDIDATE, &mut matched);
        if matched[1] > matched[0] && (matched[1] - matched[0]) <= max_pulse_width {
            let matched_center = (matched[0] + matched[1]) / 2;
            let diff = last_matched_center - matched_center;
            if diff > curr_pitch {
                curr_pitch += 1;
            } else if diff < curr_pitch {
                curr_pitch -= 1;
            }
            last_matched_center = matched_center;
            pitch_start[matched_count] = matched[0];
            pitch_end[matched_count] = matched[1];
            matched_count += 1;
        } else if !missed {
            missed = true;
            last_matched_center = target;
        } else {
            break;
        }
    }

    // Revert repairs if at least 5 periodic repairs match
    if matched_count >= 5 {
        let mut min_restored = isize::MAX;
        let mut max_restored = isize::MIN;

        for m in 0..matched_count {
            let start = pitch_start[m];
            let end = pitch_end[m];
            min_restored = min_restored.min(start);
            max_restored = max_restored.max(end);

            let mut was_repaired_interval = false;
            for pos in start..end {
                let ring_idx = (pos as usize) & mask;
                if (flags[ring_idx] & flag::CHANGED) != 0 {
                    lo_samples[ring_idx] = lo_original[ring_idx];
                    if is_wavelet {
                        hi_samples[ring_idx] = hi_original[ring_idx];
                    }
                    flags[ring_idx] |= flag::PROTECTED;
                    flags[ring_idx] &= !flag::UNDONE_BY_PROTECTION;
                    *protected_total += 1;
                    was_repaired_interval = true;
                }
            }
            if was_repaired_interval {
                *repairs_pending = repairs_pending.saturating_sub(1);
            }
        }

        if min_restored < max_restored {
            update_excitation(
                lo_samples,
                frame_count,
                buffer_length,
                mask,
                coeff_length,
                min_restored,
                max_restored,
                lo_fwd_excit,
                lo_bck_excit,
                lo_block_refls,
                &mut scratch.fwd_err,
                &mut scratch.bck_err,
                &mut scratch.refl,
            );
            if is_wavelet {
                update_excitation(
                    hi_samples,
                    frame_count,
                    buffer_length,
                    mask,
                    coeff_length,
                    min_restored,
                    max_restored,
                    hi_fwd_excit,
                    hi_bck_excit,
                    hi_block_refls,
                    &mut scratch.fwd_err,
                    &mut scratch.bck_err,
                    &mut scratch.refl,
                );
            }
        }
    }
}
