//! Real-Time Harmonic Pitch & Brass Protection
//!
//! See docs/engine/REALTIME_ENGINE.md, section 9.

use crate::burg::burg_fit_direct;
use crate::flag;
use crate::detector::marked_run_around;
use crate::reconstruct::ScratchSpace;

/// Real-Time quasi-periodic brass and vocal glottal pulse detection with cached pitch state.
pub fn protect_pitched_repairs_rt(
    lo_samples: &mut [i32],
    lo_original: &[i32],
    hi_samples: &mut [i32],
    hi_original: &[i32],
    flags: &mut [u8],
    mask: usize,
    pitch_count: isize,
    min_pitch: usize,
    curr_pitch: &mut usize,
    is_wavelet: bool,
    scratch: &mut ScratchSpace,
) {
    if *curr_pitch == 0 {
        // Run Order-256 Burg on 768 samples of original unrepaired audio
        burg_fit_direct(
            lo_original,
            mask,
            768,
            pitch_count,
            256,
            &mut scratch.fwd_err,
            &mut scratch.bck_err,
            &mut scratch.refl,
        );

        let rms_k = (scratch.refl[0..256].iter().map(|k| k * k).sum::<f64>() / 256.0).sqrt();

        let mut max_k = 0.0;
        let mut best_p = min_pitch;
        for i in min_pitch..256 {
            let abs_k = scratch.refl[i].abs();
            if abs_k > max_k {
                max_k = abs_k;
                best_p = i;
            }
        }

        // Significance Gate
        if max_k * (best_p as f64).powf(0.33) < 13.0 * rms_k {
            *curr_pitch = 0;
            return;
        } else {
            *curr_pitch = best_p;
        }
    }

    if *curr_pitch > 0 {
        let mut pulse = [0isize; 2];
        marked_run_around(flags, mask, pitch_count, flag::REPAIRED_OR_PROTECTED, &mut pulse);
        if pulse[1] <= pulse[0] {
            return;
        }

        let cp = *curr_pitch as isize;
        let max_pulse_width = (cp / 3).min(cp / 6 + 10);
        if (pulse[1] - pulse[0]) > max_pulse_width {
            return;
        }

        let center = (pulse[0] + pulse[1]) / 2;
        let max_periods = ((768 / cp) as usize).min(11);

        let mut pitch_start = [0isize; 32];
        let mut pitch_end = [0isize; 32];
        let mut matched_count = 0;

        pitch_start[matched_count] = pulse[0];
        pitch_end[matched_count] = pulse[1];
        matched_count += 1;

        // Scan forward: +cp with adaptive drift tracking
        let mut missed = false;
        let mut last_matched_center = center;
        let mut local_p = cp;
        for _k in 1..=max_periods {
            let target = last_matched_center + local_p;
            let mut matched = [0isize; 2];
            marked_run_around(flags, mask, target, flag::REPAIRED_OR_PROTECTED, &mut matched);
            if matched[1] > matched[0] && (matched[1] - matched[0]) <= max_pulse_width {
                let matched_center = (matched[0] + matched[1]) / 2;
                let diff = matched_center - last_matched_center;
                if diff > local_p {
                    local_p += 1;
                } else if diff < local_p {
                    local_p -= 1;
                }
                last_matched_center = matched_center;
                pitch_start[matched_count] = matched[0];
                pitch_end[matched_count] = matched[1];
                matched_count += 1;
                if matched_count > 7 {
                    break;
                }
            } else if !missed {
                missed = true;
                last_matched_center = target;
            } else {
                break;
            }
        }

        // Scan backward: -cp with adaptive drift tracking
        missed = false;
        last_matched_center = center;
        local_p = cp;
        for _k in 1..=max_periods {
            let target = last_matched_center - local_p;
            let mut matched = [0isize; 2];
            marked_run_around(flags, mask, target, flag::REPAIRED_OR_PROTECTED, &mut matched);
            if matched[1] > matched[0] && (matched[1] - matched[0]) <= max_pulse_width {
                let matched_center = (matched[0] + matched[1]) / 2;
                let diff = last_matched_center - matched_center;
                if diff > local_p {
                    local_p += 1;
                } else if diff < local_p {
                    local_p -= 1;
                }
                last_matched_center = matched_center;
                pitch_start[matched_count] = matched[0];
                pitch_end[matched_count] = matched[1];
                matched_count += 1;
                if matched_count > 8 {
                    break;
                }
            } else if !missed {
                missed = true;
                last_matched_center = target;
            } else {
                break;
            }
        }

        if matched_count >= 5 {
            *curr_pitch = (local_p as usize).clamp(min_pitch, 255);
            for m in 0..matched_count {
                let start = pitch_start[m];
                let end = pitch_end[m];
                for pos in start..end {
                    let idx = (pos as usize) & mask;
                    if (flags[idx] & flag::REPAIRED) != 0 {
                        lo_samples[idx] = lo_original[idx];
                        if is_wavelet {
                            hi_samples[idx] = hi_original[idx];
                        }
                        flags[idx] |= flag::PROTECTED;
                        flags[idx] &= !flag::REPAIRED;
                    }
                }
            }
        } else {
            *curr_pitch = 0;
        }
    }
}
