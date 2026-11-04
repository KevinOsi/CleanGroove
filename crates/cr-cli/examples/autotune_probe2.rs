//! Measurement tool behind the auto-tuner's constants.
//! usage: cargo run --release -p cr-cli --example autotune_probe2 -- RAW.wav [REFERENCE_REPAIRED.wav]
//! Environment switches: GAPONLY=1 (gap/music table only), MERGE=1 (Merge mode),
//! REVERSE=1 / PITCH=1 (option on in the gap/music table).
//!
//! Gap/music table, per dcl: repairs per second in 1 s blocks of groove noise between tracks
//! (all, and with peak above -50/-40/-30 dBFS), music repairs per second over 16 x 2 s
//! windows, and the changed-sample rate in those windows.
//!
//! Full table (without GAPONLY), per dcl, over 16 x 2 s windows split into quiet/loud halves:
//!   - per-channel repair burst rates (quiet vs loud)
//!   - L/R coincidence: observed vs expected-if-independent
//!   - recall of injected synthetic one-channel clicks at -40/-32/-24 dBFS
//!   - optional agreement of changed samples with a reference repaired file

use cr_audio_io::AudioFileReader;
use cr_dsp::params::{ChannelMode, CrParameters, Method};
use cr_dsp::process::Processor;

const FS: f64 = 16_777_216.0;
const NEAR: usize = 16;

fn chan_bursts(d: &[i32]) -> Vec<(usize, usize)> {
    let n = d.len();
    let act = |k: usize| d[k].abs() > 2;
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if act(i) {
            let s = i;
            let mut e = i + 1;
            while e < n {
                if act(e) { e += 1 } else if e + 1 < n && act(e + 1) { e += 2 } else { break }
            }
            out.push((s, e));
            i = e;
        } else {
            i += 1;
        }
    }
    out
}

fn overlaps(b: (usize, usize), set: &[(usize, usize)], slack: usize) -> bool {
    set.iter().any(|&(s, e)| s < b.1 + slack && b.0 < e + slack)
}

fn click_at(set: &[(usize, usize)], pos: usize) -> bool {
    overlaps((pos.saturating_sub(2), pos + 6), set, 0)
}

struct Win {
    start: usize,
    quiet: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut r = AudioFileReader::open(&args[1]).unwrap();
    let sr = r.meta.sample_rate as usize;
    let raw = r.read_all_frames().unwrap();
    let reference = args.get(2).map(|p| AudioFileReader::open(p).unwrap().read_all_frames().unwrap());
    let pad = 8192;
    let win = 2 * sr;
    let nwin = 16;
    let lo = raw.len() / 10;
    let hi = raw.len() - raw.len() / 10 - win;
    let mut wins: Vec<(usize, f64)> = (0..nwin)
        .map(|k| {
            let s = lo + (hi - lo) * k / (nwin - 1);
            let e: f64 = raw[s..s + win].iter().map(|&(l, r)| 0.5 * ((l as f64).powi(2) + (r as f64).powi(2))).sum();
            (s, (e / win as f64).sqrt())
        })
        .collect();
    let mut rms_sorted: Vec<f64> = wins.iter().map(|w| w.1).collect();
    rms_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = rms_sorted[nwin / 2];
    let windows: Vec<Win> = wins.drain(..).map(|(s, r)| Win { start: s, quiet: r < median }).collect();
    println!("{}", args[1]);
    println!("window RMS dBFS: quiet<{:.1} (min {:.1}, max {:.1})",
        20.0 * (median / FS).log10(), 20.0 * (rms_sorted[0] / FS).log10(), 20.0 * (rms_sorted[nwin - 1] / FS).log10());

    // Injected clicks: one every 0.1 s, alternating channel and level.
    let shape = [0.6, 1.0, -0.5, 0.2];
    let levels_db = [-40.0, -32.0, -24.0];
    let step = sr / 10;

    let block = sr;
    let mut gaps: Vec<usize> = Vec::new();
    let mut b = 2 * sr;
    while b + block + pad < raw.len() {
        let e: f64 = raw[b..b + block].iter().map(|&(l, r)| 0.5 * ((l as f64).powi(2) + (r as f64).powi(2))).sum();
        let db = 20.0 * ((e / block as f64).sqrt() / FS).log10();
        if db < -45.0 && db > -75.0 {
            gaps.push(b);
        }
        b += block;
    }
    // spread: keep at most 24 gap seconds, evenly sampled
    let gaps: Vec<usize> = if gaps.len() > 24 { (0..24).map(|k| gaps[k * gaps.len() / 24]).collect() } else { gaps };
    println!("gap seconds found (RMS -75..-45 dBFS): using {}", gaps.len());
    println!("GAP {:>4} | {:>7} | {:>6} {:>6} {:>6} | music/s (all 16 win)", "dcl", "clk/s", ">-50dB", ">-40dB", ">-30dB");
    for dcl in [10, 15, 20, 25, 30, 35, 40, 50, 60] {
        let mut p = CrParameters::default();
        p.dcl = dcl; p.dcr = 0; p.detect_pitch = std::env::var("PITCH").is_ok(); p.reverse = std::env::var("REVERSE").is_ok(); p.method = Method::Wavelet; p.mode = if std::env::var("MERGE").is_ok() { ChannelMode::Merge } else { ChannelMode::Stereo };
        let mut n = 0usize;
        let mut lv = [0usize; 3];
        for &g in &gaps {
            let slice = &raw[g - pad..g + block];
            let (out, _) = Processor::process_slice_aligned(p.clone(), sr as u32, slice);
            for ch in 0..2 {
                let d: Vec<i32> = (0..block).map(|i| if ch == 0 { slice[pad + i].0 - out[pad + i].0 } else { slice[pad + i].1 - out[pad + i].1 }).collect();
                for (s0, e0) in chan_bursts(&d) {
                    n += 1;
                    let pk = d[s0..e0].iter().map(|v| v.abs()).max().unwrap() as f64;
                    let db = 20.0 * (pk / FS).log10();
                    for (k, t) in [-50.0, -40.0, -30.0].iter().enumerate() { if db > *t { lv[k] += 1 } }
                }
            }
        }
        let mut m = 0usize;
        let mut chg = 0usize;
        for w in &windows {
            let slice = &raw[w.start - pad..w.start + win];
            let (out, _) = Processor::process_slice_aligned(p.clone(), sr as u32, slice);
            for ch in 0..2 {
                let d: Vec<i32> = (0..win).map(|i| if ch == 0 { slice[pad + i].0 - out[pad + i].0 } else { slice[pad + i].1 - out[pad + i].1 }).collect();
                m += chan_bursts(&d).len();
                chg += d.iter().filter(|v| v.abs() > 2).count();
            }
        }
        let gs = gaps.len().max(1) as f64;
        println!("GAP {:>4} | {:>7.1} | {:>6.1} {:>6.1} {:>6.1} | {:>6.1}  chg 1 in {:.0}", dcl, n as f64 / gs, lv[0] as f64 / gs, lv[1] as f64 / gs, lv[2] as f64 / gs, m as f64 / (nwin * win / sr) as f64, (2 * nwin * win) as f64 / chg.max(1) as f64);
    }
    if std::env::var("GAPONLY").is_ok() { return; }
    println!("{:>4} | {:>7} {:>7} | {:>6} {:>6} {:>6} | {:>5} {:>5} {:>5} | {:>6} {:>6}",
        "dcl", "q /s", "loud/s", "coinc", "expect", "excess", "r-40", "r-32", "r-24", "ref P", "ref R");
    for dcl in [10, 15, 20, 25, 30, 35, 40, 50, 60] {
        let mut p = CrParameters::default();
        p.dcl = dcl;
        p.dcr = 0;
        p.detect_pitch = false;
        p.method = Method::Wavelet;
        p.mode = if std::env::var("MERGE").is_ok() { ChannelMode::Merge } else { ChannelMode::Stereo };
        let (mut nq, mut nl) = (0usize, 0usize);
        let (mut coinc, mut expect) = (0usize, 0.0f64);
        let mut hit = [0usize; 3];
        let mut tot = [0usize; 3];
        let (mut tp, mut ours_n, mut ref_n) = (0usize, 0usize, 0usize);
        for w in &windows {
            let slice = &raw[w.start - pad..w.start + win];
            let (out, _) = Processor::process_slice_aligned(p.clone(), sr as u32, slice);
            let dl: Vec<i32> = (0..win).map(|i| slice[pad + i].0 - out[pad + i].0).collect();
            let dr: Vec<i32> = (0..win).map(|i| slice[pad + i].1 - out[pad + i].1).collect();
            let bl = chan_bursts(&dl);
            let br = chan_bursts(&dr);
            let n = bl.len() + br.len();
            if w.quiet { nq += n } else { nl += n }
            coinc += bl.iter().filter(|&&b| overlaps(b, &br, NEAR)).count();
            let mean_len = |v: &[(usize, usize)]| v.iter().map(|b| b.1 - b.0).sum::<usize>() as f64 / v.len().max(1) as f64;
            let reach = 2.0 * NEAR as f64 + mean_len(&bl) + mean_len(&br);
            expect += bl.len() as f64 * (br.len() as f64 * reach / win as f64).min(1.0);

            // Injection run.
            let mut inj = slice.to_vec();
            let mut sites = Vec::new();
            let mut k = 0;
            let mut pos = step / 2;
            while pos + 8 < win {
                let li = k % 3;
                let left = (k / 3) % 2 == 0;
                let amp = FS * 10f64.powf(levels_db[li] / 20.0);
                for (j, &s) in shape.iter().enumerate() {
                    let v = (s * amp) as i32;
                    let f = &mut inj[pad + pos + j];
                    if left { f.0 = f.0.saturating_add(v) } else { f.1 = f.1.saturating_add(v) }
                }
                sites.push((pos, li, left));
                k += 1;
                pos += step;
            }
            let (out_i, _) = Processor::process_slice_aligned(p.clone(), sr as u32, &inj);
            let il: Vec<i32> = (0..win).map(|i| inj[pad + i].0 - out_i[pad + i].0).collect();
            let ir: Vec<i32> = (0..win).map(|i| inj[pad + i].1 - out_i[pad + i].1).collect();
            let ibl = chan_bursts(&il);
            let ibr = chan_bursts(&ir);
            for &(pos, li, left) in &sites {
                let base = if left { &bl } else { &br };
                if click_at(base, pos) {
                    continue; // a real click was already repaired here
                }
                tot[li] += 1;
                if click_at(if left { &ibl } else { &ibr }, pos) {
                    hit[li] += 1;
                }
            }

            if let Some(rf) = &reference {
                for i in 0..win {
                    let a = slice[pad + i];
                    let o = out[pad + i];
                    let q = rf[w.start + i];
                    for (raw_v, ours, refv) in [(a.0, o.0, q.0), (a.1, o.1, q.1)] {
                        let ch_o = (raw_v - ours).abs() > 2;
                        let ch_r = (raw_v - refv).abs() > 2;
                        ours_n += ch_o as usize;
                        ref_n += ch_r as usize;
                        tp += (ch_o && ch_r) as usize;
                    }
                }
            }
        }
        let half_secs = (nwin / 2 * win) as f64 / sr as f64;
        let rc = |i: usize| 100.0 * hit[i] as f64 / tot[i].max(1) as f64;
        let (rp, rr) = if reference.is_some() {
            (100.0 * tp as f64 / ours_n.max(1) as f64, 100.0 * tp as f64 / ref_n.max(1) as f64)
        } else {
            (f64::NAN, f64::NAN)
        };
        println!("{:>4} | {:>7.1} {:>7.1} | {:>6} {:>6.0} {:>6.0} | {:>4.0}% {:>4.0}% {:>4.0}% | {:>5.0}% {:>5.0}%",
            dcl, nq as f64 / half_secs, nl as f64 / half_secs, coinc, expect, coinc as f64 - expect,
            rc(0), rc(1), rc(2), rp, rr);
    }
}
