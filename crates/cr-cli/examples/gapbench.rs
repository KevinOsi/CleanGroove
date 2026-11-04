//! Repair-quality benchmark on clean audio, where the truth is known.
//!
//! Source: a clean digital recording (default `scratch/test_reference_album.wav`, git-ignored;
//! CD rips, no real clicks). Positions are spread evenly over its music (one-second RMS at
//! least -40 dBFS).
//!
//! Part A, interpolation alone: a gap of N samples is cut from the left channel and filled by
//! the engine's repair routine (`fill_gap`, the Classic interpolator); the
//! score is the gap SNR, 10*log10(sum(original^2) / sum((fill - original)^2)), median and
//! 10th percentile over positions. 0 dB is no better than silence.
//!
//! Part B, end to end: a defect is added to the left channel of the clean audio (a 4-sample
//! click, or a noise burst of the given length), peak -12 dBFS, and the whole Classic engine
//! runs (Wavelet, DeClick 45). Scores: how often the defect was repaired (at least half its
//! samples changed), and the SNR over the defect against the clean original, repaired versus
//! left as it was.
//!
//! Part A also runs a prototype least-squares interpolator (LSAR) for comparison. Classic
//! baseline (200 positions): 29.8 dB median at 4 samples, 7.5 at 32, about 3-4 from 64 up;
//! the best LSAR settings gain only about 2 dB, and only at 32-64 samples.
//!
//! `--engine enhanced` runs part B through the Enhanced engine (longer reach) instead, and adds
//! the long-repair counts: long repairs made, and long candidates its music gate refused.
//! `--above DB` sets each defect's level relative to the music around it instead of a fixed
//! -12 dBFS peak: its RMS stands DB above the clean audio's RMS in the 10 ms either side (peak
//! capped at -1 dBFS). The fixed level puts most defects inside loud music; `--above` shows how
//! the repair rate depends on how clearly a defect stands out, which is what Enhanced's
//! long-repair gate looks at. Where the music is too loud for that (the cap), the defect is
//! counted apart: "clear" is the repair rate over the defects that reached the full DB.
//!
//! usage: cargo run --release -p cr-cli --example gapbench -- [REFERENCE.wav] [--positions N]
//!        [--engine classic|enhanced] [--above DB] [--skip-a]

use rayon::prelude::*;

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{CrParameters, Engine, Method};
use cr_dsp::process::Processor;
use cr_dsp::reconstruct::{fill_gap, ScratchSpace};

const GAPS: [usize; 9] = [4, 16, 32, 64, 128, 256, 512, 1024, 2048];
/// Defects for part B, milliseconds (0 = the 4-sample click).
const DEFECTS_MS: [f64; 10] = [0.0, 0.5, 1.0, 2.0, 4.0, 6.0, 10.0, 15.0, 20.0, 30.0];
const RING: usize = 16384;

fn snr_db(truth: &[f64], got: &[f64]) -> f64 {
    let sig: f64 = truth.iter().map(|v| v * v).sum();
    let err: f64 = truth.iter().zip(got).map(|(a, b)| (a - b) * (a - b)).sum();
    10.0 * (sig.max(1.0) / err.max(1e-9)).log10()
}

/// Prototype least-squares AR interpolation (LSAR, Janssen et al. 1986) for comparison only:
/// not engine code. An AR(order) model is fitted (autocorrelation method, Hann-windowed) to the
/// `context` samples on both sides of the gap; the gap is then the values that minimise the
/// model's total prediction error, a symmetric Toeplitz system solved by Levinson recursion.
fn lsar_fill(x: &[f64], start: usize, gap: usize, order: usize, context: usize) -> Vec<f64> {
    // Autocorrelation over both contexts.
    let mut r = vec![0.0f64; order + 1];
    for seg in [&x[start - context..start], &x[start + gap..start + gap + context]] {
        let n = seg.len();
        let w: Vec<f64> = seg
            .iter()
            .enumerate()
            .map(|(i, v)| v * (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos()))
            .collect();
        for (lag, rl) in r.iter_mut().enumerate() {
            *rl += (lag..n).map(|i| w[i] * w[i - lag]).sum::<f64>();
        }
    }
    r[0] *= 1.0 + 1e-9;
    // Levinson-Durbin: prediction-error filter a[0] = 1, a[1..=order].
    let mut a = vec![0.0f64; order + 1];
    a[0] = 1.0;
    let mut err = r[0];
    for m in 1..=order {
        let k = -(0..m).map(|j| a[j] * r[m - j]).sum::<f64>() / err;
        let prev = a.clone();
        for j in 1..m {
            a[j] = prev[j] + k * prev[m - j];
        }
        a[m] = k;
        err *= 1.0 - k * k;
    }
    // c[d] = sum_i a[i] a[i + d]: the Toeplitz matrix of the error energy in the unknowns.
    let c: Vec<f64> = (0..=order).map(|d| (0..=order - d).map(|i| a[i] * a[i + d]).sum()).collect();
    let tcol = |d: usize| if d <= order { c[d] } else { 0.0 };
    // Right-hand side: the known samples within `order` of the gap.
    let rhs: Vec<f64> = (0..gap)
        .map(|u| {
            let mut s = 0.0;
            for d in 1..=order {
                if u + 1 >= d && start + u >= d && u < d {
                    s += c[d] * x[start + u - d];
                } else if u < d {
                    s += c[d] * x[start + u - d];
                }
                if u + d >= gap {
                    s += c[d] * x[start + u + d];
                }
            }
            -s
        })
        .collect();
    levinson_solve(&tcol, &rhs)
}

/// Solves T x = y for symmetric Toeplitz T (first column `t(k)`) by Levinson recursion.
fn levinson_solve(t: &dyn Fn(usize) -> f64, y: &[f64]) -> Vec<f64> {
    let n = y.len();
    let mut f = vec![1.0 / t(0)];
    let mut x = vec![y[0] / t(0)];
    for m in 1..n {
        let ef: f64 = (0..m).map(|i| t(m - i) * f[i]).sum();
        let denom = 1.0 - ef * ef;
        let mut nf = vec![0.0; m + 1];
        for i in 0..=m {
            let fi = if i < m { f[i] } else { 0.0 };
            let bi = if i > 0 { f[m - i] } else { 0.0 };
            nf[i] = (fi - ef * bi) / denom;
        }
        f = nf;
        let ex: f64 = (0..m).map(|i| t(m - i) * x[i]).sum();
        x.push(0.0);
        let corr = y[m] - ex;
        for i in 0..=m {
            x[i] += corr * f[m - i];
        }
    }
    x
}

fn quantiles(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    (v[v.len() / 2], v[v.len() / 10])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = "scratch/test_reference_album.wav".to_string();
    let mut positions = 60usize;
    let mut engine = Engine::Classic;
    let mut skip_a = false;
    let mut above: Option<f64> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--positions" {
            positions = args[i + 1].parse().unwrap();
            i += 1;
        } else if args[i] == "--engine" {
            engine = Engine::parse(&args[i + 1]).expect("--engine classic|enhanced");
            i += 1;
        } else if args[i] == "--above" {
            above = Some(args[i + 1].parse().unwrap());
            i += 1;
        } else if args[i] == "--skip-a" {
            skip_a = true;
        } else {
            path = args[i].clone();
        }
        i += 1;
    }
    let mut reader = AudioFileReader::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let sr = reader.meta.sample_rate;
    let frames = reader.read_all_frames().unwrap();
    let srz = sr as usize;

    // Music seconds (RMS >= -40 dBFS), away from the file edges, spread evenly.
    let music: Vec<usize> = (5..frames.len() / srz - 5)
        .filter(|&b| {
            let s = &frames[b * srz..(b + 1) * srz];
            let ms = s.iter().map(|f| (f.0 as f64).powi(2)).sum::<f64>() / srz as f64;
            20.0 * (ms.sqrt() / 16_777_216.0).log10() >= -40.0
        })
        .collect();
    let picks: Vec<usize> = (0..positions).map(|k| music[k * music.len() / positions] * srz + srz / 2).collect();
    println!("{} ({} Hz), {} positions in its music\n", path, sr, picks.len());

    // Part A: interpolation alone.
    println!("A. Interpolation alone, gap SNR in dB: Classic repair routine vs a prototype LSAR (not engine code)");
    println!("{:>7} {:>8} | {:>8} {:>8} | {:>11} {:>8}", "gap", "ms", "Classic", "p10", "LSAR proto", "p10");
    for &gap in if skip_a { &GAPS[..0] } else { &GAPS[..] } {
        let scores: Vec<(f64, f64)> = picks
            .par_iter()
            .map(|&pos| {
                let mut ring = vec![0i32; RING];
                let base = pos - RING / 2;
                for (k, slot) in ring.iter_mut().enumerate() {
                    *slot = frames[base + k].0;
                }
                let start = (RING / 2 - gap / 2) as isize;
                let info = [start, start + gap as isize];
                let truth: Vec<f64> = (info[0]..info[1]).map(|k| ring[k as usize] as f64).collect();
                let mut fixed = vec![0i32; 4096];
                let mut scratch = ScratchSpace::new();
                fill_gap(&ring, RING - 1, info, &mut fixed, &mut scratch);
                let got: Vec<f64> = fixed[..gap].iter().map(|&v| v as f64).collect();
                let classic = snr_db(&truth, &got);
                // Prototype: AR order 200, 4096 samples of context each side (the best of a
                // sweep over orders 40-400 and contexts 1024-6000); LSAR_ORDER / LSAR_CTX override.
                let x: Vec<f64> = ring.iter().map(|&v| v as f64).collect();
                let order = std::env::var("LSAR_ORDER").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
                let ctx = std::env::var("LSAR_CTX").ok().and_then(|v| v.parse().ok()).unwrap_or(4096);
                let lsar = lsar_fill(&x, start as usize, gap, order, ctx);
                (classic, snr_db(&truth, &lsar))
            })
            .collect();
        let (med, p10) = quantiles(scores.iter().map(|s| s.0).collect());
        let (lmed, lp10) = quantiles(scores.iter().map(|s| s.1).collect());
        println!(
            "{:>7} {:>8.2} | {:>8.1} {:>8.1} | {:>11.1} {:>8.1}",
            gap,
            gap as f64 * 1000.0 / sr as f64,
            med,
            p10,
            lmed,
            lp10
        );
    }

    // Part B: end to end through the engine.
    let mut params = CrParameters::preset_stereo_lp();
    params.dcl = 45;
    params.method = Method::Wavelet;
    params.engine = engine;
    let level = match above {
        Some(db) => format!("defect RMS {db} dB above the music around it"),
        None => "defect peak -12 dBFS".to_string(),
    };
    println!("\nB. End to end ({}, Wavelet, DeClick 45), {level}", engine.name());
    println!(
        "{:>9} {:>7} | {:>9} {:>12} {:>12} | {:>5} {:>5} | {:>13}",
        "defect", "samples", "repaired", "SNR fixed", "SNR left", "long", "gated", "clear: rep/n"
    );
    let amp = 16_777_216.0 * 10f64.powf(-12.0 / 20.0);
    for ms in DEFECTS_MS {
        let len = if ms == 0.0 { 4 } else { (ms / 1000.0 * sr as f64) as usize };
        let rows: Vec<(bool, f64, f64, usize, usize, bool)> = picks
            .par_iter()
            .enumerate()
            .map(|(k, &pos)| {
                let half = 3 * srz / 2;
                let clean = &frames[pos - half..pos + half];
                let mut dirty = clean.to_vec();
                let cap = 16_777_216.0 * 10f64.powf(-1.0 / 20.0);
                let amp = match above {
                    None => amp,
                    Some(db) => {
                        let local = (sr as usize / 100).max(1);
                        let around = (half - local..half).chain(half + len..half + len + local);
                        let msq = around.map(|i| (clean[i].0 as f64).powi(2)).sum::<f64>() / (2 * local) as f64;
                        // Uniform noise of peak `amp` has RMS amp / sqrt(3); the click's RMS is about 0.6 amp.
                        let crest = if ms == 0.0 { 1.0 / 0.6 } else { 3f64.sqrt() };
                        (msq.sqrt().max(1.0) * 10f64.powf(db / 20.0) * crest).min(cap)
                    }
                };
                let mut seed = 0x9E3779B97F4A7C15u64 ^ k as u64;
                for j in 0..len {
                    let v = if ms == 0.0 {
                        [0.6, 1.0, -0.5, 0.2][j] * amp
                    } else {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        (((seed >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0) * amp
                    };
                    dirty[half + j].0 = (dirty[half + j].0 as f64 + v).clamp(-16_777_216.0, 16_777_215.0) as i32;
                }
                let (out, stats) = Processor::process_slice_aligned(params.clone(), sr, &dirty);
                let region = half..half + len;
                let truth: Vec<f64> = region.clone().map(|i| clean[i].0 as f64).collect();
                let fixed: Vec<f64> = region.clone().map(|i| out[i].0 as f64).collect();
                let left: Vec<f64> = region.clone().map(|i| dirty[i].0 as f64).collect();
                let changed = region.clone().filter(|&i| (out[i].0 - dirty[i].0).abs() > 2).count();
                let clear = above.is_some() && amp < cap;
                (2 * changed >= len, snr_db(&truth, &fixed), snr_db(&truth, &left), stats.long_repairs, stats.long_gated, clear)
            })
            .collect();
        let repaired = rows.iter().filter(|r| r.0).count();
        let (fixed_med, _) = quantiles(rows.iter().map(|r| r.1).collect());
        let (left_med, _) = quantiles(rows.iter().map(|r| r.2).collect());
        let label = if ms == 0.0 { "click".to_string() } else { format!("{ms} ms") };
        println!(
            "{:>9} {:>7} | {:>8.0}% {:>12.1} {:>12.1} | {:>5} {:>5} | {:>13}",
            label,
            len,
            100.0 * repaired as f64 / rows.len() as f64,
            fixed_med,
            left_med,
            rows.iter().map(|r| r.3).sum::<usize>(),
            rows.iter().map(|r| r.4).sum::<usize>(),
            if above.is_some() {
                let n = rows.iter().filter(|r| r.5).count();
                let rep = rows.iter().filter(|r| r.5 && r.0).count();
                format!("{:.0}% of {}", 100.0 * rep as f64 / n.max(1) as f64, n)
            } else {
                "-".to_string()
            }
        );
    }
}
