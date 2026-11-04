//! Isolated test: does a higher-order prediction model make clicks stand out more from music?
//!
//! Not engine code. For each clip and each model (AR order, fit window) this computes the
//! detector's prediction error the way the engine does (a Burg fit per 256-sample block, the
//! window centred on the block; engine doc 4), then runs the forward detection rule on it
//! (engine doc 5) at the full rate with the Simple method's windows: a click is where the RMS
//! error in the peak window exceeds T times the mean error in the shoulder before it.
//!
//! - events/s: detections on the clip as it is. These are the record's real clicks plus
//!   music pickup; a better model should leave the real clicks and drop the pickup.
//! - hits: real click waveforms (cut from the noise between tracks on the rips:
//!   `scratch/bench/noise_bank.wav`, built by `detbench`) injected into the clip every
//!   ~0.25 s, scaled so their peak stands X dB over the music's RMS 10 ms either side.
//! - matched: the threshold is moved so the clip gives as many events as order 10 at that
//!   DeClick; the hit rate there is the fair comparison (more clicks for the same pickup).
//!
//! usage: cargo run --release -p cr-cli --example ordertest -- CLIP.wav [CLIP.wav ...]

use rayon::prelude::*;

use cr_audio_io::AudioFileReader;
use cr_dsp::burg::burg_fit;
use cr_dsp::params::CrParameters;
use cr_dsp::process::Processor;

const FULL_SCALE: f64 = 16_777_216.0;
/// (order, fit window): order 10 over 512 is the engine.
const MODELS: [(usize, usize); 6] = [(10, 512), (20, 512), (40, 512), (40, 2048), (80, 2048), (160, 4096)];
const LEVELS_DB: [f64; 4] = [0.0, 6.0, 12.0, 20.0];
const DCLS: [i32; 2] = [30, 45];

fn threshold(setting: i32) -> f64 {
    let u = (100 - setting) as f64;
    2.8 + 0.012 * u + 2.5e-4 * u * u + 3.5e-8 * u.powi(4)
}

/// |forward prediction error| per sample, from a Burg fit per 256-sample block.
fn excitation(x: &[i32], order: usize, win: usize) -> Vec<f64> {
    let off = (win - 256) / 2;
    let mut e = vec![0.0; x.len()];
    let blocks: Vec<usize> = (off..x.len().saturating_sub(win - off)).step_by(256).collect();
    let parts: Vec<(usize, Vec<f64>)> = blocks
        .par_iter()
        .map(|&b| {
            let (mut f, mut bk, mut refl) = (vec![0.0; win], vec![0.0; win], vec![0.0; order]);
            burg_fit(x, usize::MAX, win, (b - off) as isize, order, &mut f, &mut bk, &mut refl);
            // fwd[i] is the error of window sample i + 1; block sample j is window sample off + j.
            (b, (0..256).map(|j| f[off - 1 + j].abs()).collect())
        })
        .collect();
    for (b, v) in parts {
        e[b..b + 256].copy_from_slice(&v);
    }
    e
}

/// Detection ratio per sample: RMS error over the peak window ending at n, over the mean
/// error in the shoulder before it.
fn ratios(e: &[f64], peak: usize, shoulder: usize) -> Vec<f32> {
    let mut r = vec![0.0f32; e.len()];
    for n in peak + shoulder..e.len() {
        let pk: f64 = e[n - peak..n].iter().map(|v| v * v).sum();
        let pre: f64 = e[n - peak - shoulder..n - peak].iter().sum();
        r[n] = ((pk / peak as f64).sqrt() / (pre / shoulder as f64).max(1.0)) as f32;
    }
    r
}

/// Detections: runs of ratio > t, runs within 64 samples merged.
fn events(r: &[f32], t: f64, lo: usize, hi: usize) -> usize {
    let mut n = 0;
    let mut last: Option<usize> = None;
    for (i, &v) in r.iter().enumerate().take(hi).skip(lo) {
        if v as f64 > t {
            if last.is_none_or(|l| i > l + 64) {
                n += 1;
            }
            last = Some(i);
        }
    }
    n
}

fn windows(dcl: i32) -> (usize, usize) {
    (6 + dcl as usize / 15, 100 + dcl as usize / 3)
}

/// Click waveforms (what the engine removed) from the noise bank, peak above -40 dBFS.
fn harvest_clicks() -> Vec<Vec<f64>> {
    let mut r = AudioFileReader::open("scratch/bench/noise_bank.wav").expect("run detbench first to build the noise bank");
    let sr = r.meta.sample_rate;
    let frames = r.read_all_frames().unwrap();
    let mut p = CrParameters::preset_stereo_lp();
    p.dcl = 50;
    let (out, _) = Processor::process_slice_aligned(p, sr, &frames);
    let audible = FULL_SCALE * 0.01;
    let mut clicks = Vec::new();
    for c in 0..2 {
        let d: Vec<f64> = frames.iter().zip(&out).map(|(a, b)| if c == 0 { (a.0 - b.0) as f64 } else { (a.1 - b.1) as f64 }).collect();
        let mut i = 0;
        while i < d.len() {
            if d[i].abs() > 2.0 {
                let s = i;
                while i < d.len() && (d[i].abs() > 2.0 || d[i..(i + 3).min(d.len())].iter().any(|v| v.abs() > 2.0)) {
                    i += 1;
                }
                let w = d[s..i].to_vec();
                if w.iter().fold(0.0f64, |m, v| m.max(v.abs())) > audible && w.len() <= 256 {
                    clicks.push(w);
                }
            }
            i += 1;
        }
    }
    clicks
}

fn main() {
    let clips: Vec<String> = std::env::args().skip(1).collect();
    let clicks = harvest_clicks();
    println!("{} real click waveforms from the noise bank (median {} samples)\n", clicks.len(), {
        let mut l: Vec<usize> = clicks.iter().map(|c| c.len()).collect();
        l.sort();
        l[l.len() / 2]
    });
    for path in clips {
        let mut rd = AudioFileReader::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let sr = rd.meta.sample_rate as usize;
        let frames = rd.read_all_frames().unwrap();
        let lo = 8192;
        let hi = frames.len() - 8192;
        let secs = 2.0 * (hi - lo) as f64 / sr as f64;
        println!("{path}: {:.1} s x 2 channels", secs / 2.0);
        print!("{:>5} {:>6} |", "order", "window");
        for dcl in DCLS {
            print!(" dcl{dcl} ev/s");
            for l in LEVELS_DB {
                print!(" {:>5}", format!("+{l}"));
            }
            print!(" | match");
            print!(" |");
        }
        println!();
        // Injection positions (shared by all models): every ~0.25 s, a click from the bank.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut inject: Vec<(usize, usize, usize, usize)> = Vec::new(); // (channel, pos, click, level)
        let mut pos = lo + sr / 8;
        while pos + 512 < hi {
            for c in 0..2 {
                inject.push((c, pos + (rnd() % 2000) as usize, (rnd() as usize) % clicks.len(), (rnd() as usize) % LEVELS_DB.len()));
            }
            pos += sr / 4;
        }
        let chans: Vec<Vec<i32>> = (0..2).map(|c| frames.iter().map(|f| if c == 0 { f.0 } else { f.1 }).collect()).collect();
        let dirty: Vec<Vec<i32>> = (0..2)
            .map(|c| {
                let mut x = chans[c].clone();
                for &(ic, p, k, l) in inject.iter().filter(|i| i.0 == c) {
                    let w = &clicks[k];
                    let ctx = sr / 100;
                    let ms = (p - ctx..p).chain(p + w.len()..p + w.len() + ctx).map(|i| (chans[ic][i] as f64).powi(2)).sum::<f64>() / (2 * ctx) as f64;
                    let peak = w.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                    let g = ms.sqrt().max(1.0) * 10f64.powf(LEVELS_DB[l] / 20.0) / peak;
                    for (j, v) in w.iter().enumerate() {
                        x[p + j] = (x[p + j] as f64 + v * g).clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32;
                    }
                }
                x
            })
            .collect();
        let mut reference: Vec<f64> = Vec::new();
        for (mi, &(order, win)) in MODELS.iter().enumerate() {
            let clean_r: Vec<Vec<Vec<f32>>> = DCLS.iter().map(|&d| {
                let (pk, sh) = windows(d);
                (0..2).map(|c| ratios(&excitation(&chans[c], order, win), pk, sh)).collect()
            }).collect();
            let dirty_r: Vec<Vec<Vec<f32>>> = DCLS.iter().map(|&d| {
                let (pk, sh) = windows(d);
                (0..2).map(|c| ratios(&excitation(&dirty[c], order, win), pk, sh)).collect()
            }).collect();
            print!("{:>5} {:>6} |", order, win);
            for (di, &dcl) in DCLS.iter().enumerate() {
                let t = threshold(dcl);
                let ev: usize = (0..2).map(|c| events(&clean_r[di][c], t, lo, hi)).sum();
                let hit_at = |t: f64, l: usize| {
                    let sel: Vec<_> = inject.iter().filter(|i| i.3 == l).collect();
                    let (pk, _) = windows(dcl);
                    let h = sel.iter().filter(|i| {
                        let w = clicks[i.2].len();
                        dirty_r[di][i.0][i.1..i.1 + w + pk].iter().any(|&v| v as f64 > t)
                    }).count();
                    100.0 * h as f64 / sel.len().max(1) as f64
                };
                print!(" {:>9.2}", ev as f64 / secs);
                for l in 0..LEVELS_DB.len() {
                    print!(" {:>4.0}%", hit_at(t, l));
                }
                // Matched threshold: as many events as order 10 at this DeClick.
                if mi == 0 {
                    reference.push(ev as f64);
                }
                let target = reference[di];
                let (mut a, mut b) = (1.0f64, 40.0f64);
                for _ in 0..40 {
                    let m = 0.5 * (a + b);
                    let e: usize = (0..2).map(|c| events(&clean_r[di][c], m, lo, hi)).sum();
                    if (e as f64) > target { a = m } else { b = m }
                }
                let avg: f64 = (0..LEVELS_DB.len()).map(|l| hit_at(b, l)).sum::<f64>() / LEVELS_DB.len() as f64;
                print!(" | {:>4.0}%", avg);
                print!(" |");
            }
            println!();
        }
        println!();
    }
}
