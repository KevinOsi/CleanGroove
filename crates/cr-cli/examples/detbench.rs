//! Detection benchmark: real record noise on clean music, where every repair can be scored.
//!
//! The clean reference (default `scratch/test_reference_album.wav`, CD sources with no real
//! clicks) gets real vinyl noise added: the silence between tracks on the user's rips (0.25 s
//! blocks between -75 and -45 dBFS, interior blocks of a gap only, so no fade-out tails).
//! Each rip's noise is scaled by (reference music RMS / rip music RMS): the noise sits under
//! the reference music as it sat under that record's own music. `--noise-gain DB` raises it
//! further, standing in for a worse record (more clicks stand clear of the music).
//!
//! Truth: the engine run on the noise alone (Classic, DeClick 50, forward and reverse,
//! union) finds the clicks; in silence that is reliable. Clicks with a removed peak under
//! -40 dBFS (inaudible, as in the analysis) are not counted. The ideal output is the clean
//! music plus the declicked noise. This truth leans on Classic's own idea of a click, so it
//! ranks engines fairly against each other but cannot see clicks Classic never finds.
//!
//! Per setting, the excerpts run through the engine twice: with noise and clean.
//! - audible: share of the true clicks at or above 0 dB exposure repaired (the headline).
//! - found: share of all true clicks repaired (at least half their samples changed), overall
//!   and by exposure, the click's removed peak over the clean music's RMS 10 ms either side.
//!   Under 0 dB a click is masked by the music; over 20 dB it stands clear.
//! - clean false/s: repairs on the clean music alone, all false (both channels).
//! - false SNR: median SNR of those false repairs against the clean music (how much each one
//!   damages the music; low is worse).
//! - noisy false/s: repairs with noise that overlap no true click.
//! - music SNR: clean music energy over the error outside the true clicks (eaten music).
//! - click cut: error energy inside the true clicks, before over after (dB, higher is better).
//! - x rt: audio seconds per CPU second of one engine run (one thread).
//!
//! Rows: fwd (Classic), rev (Classic with Reverse), and two combinations of the fwd and rev
//! runs, prototypes of the Max idea (not engine code): "and" keeps forward repairs that the
//! reverse run also made; "and+exp" adds repairs only one direction made when they stand
//! 20 dB over the repaired audio 10 ms either side (an exposed click).
//!
//! usage: cargo run --release -p cr-cli --example detbench -- [REFERENCE.wav]
//!        [--rips DIR]... [--source TEXT] [--dcl 20,30,40,50] [--excerpts N] [--rebuild]
//!        [--engine classic|enhanced] [--noise-gain DB] [--save FILE]
//! The noise bank is cached in `scratch/bench/` (git-ignored); `--rebuild` cuts it again.
//! `--source TEXT` uses only the noise of rips whose name contains TEXT (case-insensitive).

use std::time::Instant;

use rayon::prelude::*;

use cr_audio_io::{AudioFileReader, AudioFileWriter, AudioMetadata};
use cr_dsp::params::{CrParameters, Engine, Method};
use cr_dsp::process::Processor;

type Frames = Vec<(i32, i32)>;

const FULL_SCALE: f64 = 16_777_216.0;
const GAP_MIN_DB: f64 = -75.0;
const GAP_MAX_DB: f64 = -45.0;
const MUSIC_MIN_DB: f64 = -40.0;
const AUDIBLE_DB: f64 = -40.0;
const TRUTH_DCL: i32 = 50;
/// Differences at or below this are wavelet round-off, not repairs.
const ROUNDOFF: i32 = 2;
const EXCERPT_S: usize = 20;
/// Warm-up at each excerpt end, left out of the scores.
const WARM_S: usize = 1;
const XFADE: usize = 256;
const EXPOSED_DB: f64 = 20.0;
const BINS: [(f64, f64, &str); 4] =
    [(f64::NEG_INFINITY, 0.0, "<0dB"), (0.0, 10.0, "0-10"), (10.0, 20.0, "10-20"), (20.0, f64::INFINITY, ">20")];
const BANK_WAV: &str = "scratch/bench/noise_bank.wav";
const BANK_TSV: &str = "scratch/bench/noise_bank.tsv";

fn ch(f: &(i32, i32), c: usize) -> i32 {
    if c == 0 { f.0 } else { f.1 }
}

fn set_ch(f: &mut (i32, i32), c: usize, v: i32) {
    if c == 0 { f.0 = v } else { f.1 = v }
}

fn mean_square(s: &[(i32, i32)]) -> f64 {
    s.iter().map(|f| (f.0 as f64).powi(2) + (f.1 as f64).powi(2)).sum::<f64>() / (2 * s.len().max(1)) as f64
}

fn db(ms: f64) -> f64 {
    10.0 * (ms.sqrt().max(1.0) / FULL_SCALE).log10() * 2.0
}

/// One stretch of interior gap seconds from one rip.
struct Segment {
    source: String,
    frames: Frames,
}

/// Streams a rip a quarter second at a time; returns its gap segments (unscaled) and music mean square.
fn cut_noise(path: &std::path::Path) -> Option<(Vec<Segment>, f64, u32)> {
    let mut reader = AudioFileReader::open(path).ok()?;
    let sr = reader.meta.sample_rate as usize;
    let block = sr / 4;
    let source = path.file_stem()?.to_string_lossy().to_string();
    let mut chunk = Vec::new();
    let mut blocks: Vec<(Frames, bool)> = Vec::new();
    let (mut music_ms, mut music_n) = (0.0, 0usize);
    let mut segs = Vec::new();
    let mut run: Frames = Vec::new();
    let mut idx = 0usize;
    // Keeps the last three blocks: the middle one is interior gap if all three are gap.
    loop {
        let n = reader.read_chunk(block, &mut chunk).ok()?;
        if n < block {
            break;
        }
        let ms = mean_square(&chunk);
        let d = db(ms);
        if d >= MUSIC_MIN_DB {
            music_ms += ms;
            music_n += 1;
        }
        blocks.push((chunk.clone(), idx >= 8 && d > GAP_MIN_DB && d < GAP_MAX_DB));
        idx += 1;
        if blocks.len() == 3 {
            if blocks.iter().all(|b| b.1) {
                run.extend_from_slice(&blocks[1].0);
            } else if !run.is_empty() {
                segs.push(Segment { source: source.clone(), frames: std::mem::take(&mut run) });
            }
            blocks.remove(0);
        }
    }
    if !run.is_empty() {
        segs.push(Segment { source: source.clone(), frames: run });
    }
    Some((segs, music_ms / music_n.max(1) as f64, sr as u32))
}

fn build_bank(rip_dirs: &[String], ref_music_ms: f64, sr: u32) -> Vec<Segment> {
    let mut files: Vec<std::path::PathBuf> = rip_dirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")))
        .filter(|p| !p.to_string_lossy().to_lowercase().contains("repaired"))
        .collect();
    files.sort();
    let cut: Vec<(Vec<Segment>, f64, u32)> = files
        .par_iter()
        .with_max_len(1)
        .filter_map(|p| {
            let r = cut_noise(p);
            eprintln!("  noise from {}: {}", p.display(), r.as_ref().map_or("unreadable".into(), |r| format!("{} s", r.0.iter().map(|s| s.frames.len()).sum::<usize>() / r.2 as usize)));
            r
        })
        .collect();
    let mut bank = Vec::new();
    for (segs, music_ms, rip_sr) in cut {
        if rip_sr != sr || music_ms <= 0.0 {
            continue;
        }
        let gain = (ref_music_ms / music_ms).sqrt();
        for mut s in segs {
            for f in &mut s.frames {
                f.0 = (f.0 as f64 * gain).round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32;
                f.1 = (f.1 as f64 * gain).round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32;
            }
            bank.push(s);
        }
    }
    let _ = std::fs::create_dir_all("scratch/bench");
    let meta = AudioMetadata { channels: 2, sample_rate: sr, bits_per_sample: 24, is_float: false, total_frames: 0 };
    let mut w = AudioFileWriter::create(BANK_WAV, meta).expect("write noise bank");
    let mut tsv = String::from("source\tframes\n");
    for s in &bank {
        w.write_chunk(&s.frames).unwrap();
        tsv += &format!("{}\t{}\n", s.source, s.frames.len());
    }
    w.finalize().unwrap();
    std::fs::write(BANK_TSV, tsv).unwrap();
    bank
}

fn load_bank() -> Option<Vec<Segment>> {
    let tsv = std::fs::read_to_string(BANK_TSV).ok()?;
    let all = AudioFileReader::open(BANK_WAV).ok()?.read_all_frames().ok()?;
    let mut at = 0;
    let mut bank = Vec::new();
    for line in tsv.lines().skip(1) {
        let (source, n) = line.split_once('\t')?;
        let n: usize = n.parse().ok()?;
        bank.push(Segment { source: source.to_string(), frames: all.get(at..at + n)?.to_vec() });
        at += n;
    }
    Some(bank)
}

/// `len` frames of noise from the bank, starting at segment `first`, joined with crossfades.
fn noise_for(bank: &[&Segment], first: usize, len: usize) -> Frames {
    let mut out: Frames = Vec::with_capacity(len + XFADE);
    let mut k = first;
    while out.len() < len {
        let seg = &bank[k % bank.len()].frames;
        k += 1;
        if seg.len() < 4 * XFADE {
            continue;
        }
        let fade = if out.len() >= XFADE { XFADE } else { 0 };
        let base = out.len() - fade;
        for (j, f) in seg.iter().enumerate() {
            if j < fade {
                let w = (j + 1) as f64 / (fade + 1) as f64;
                let o = &mut out[base + j];
                o.0 = (o.0 as f64 * (1.0 - w) + f.0 as f64 * w).round() as i32;
                o.1 = (o.1 as f64 * (1.0 - w) + f.1 as f64 * w).round() as i32;
            } else {
                out.push(*f);
            }
        }
    }
    out.truncate(len);
    out
}

/// Repaired spans [start, end) of one channel: runs of changed samples, bridging 2 unchanged.
fn spans(input: &[(i32, i32)], output: &[(i32, i32)], c: usize) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = Vec::new();
    for i in 0..input.len() {
        if (ch(&output[i], c) - ch(&input[i], c)).abs() > ROUNDOFF {
            match v.last_mut() {
                Some(last) if i <= last.1 + 2 => last.1 = i + 1,
                _ => v.push((i, i + 1)),
            }
        }
    }
    v
}

fn overlaps(a: (usize, usize), list: &[(usize, usize)], slack: usize) -> bool {
    // `list` is sorted by start.
    let i = list.partition_point(|s| s.1 + slack <= a.0);
    list.get(i).is_some_and(|s| s.0 < a.1 + slack)
}

fn merge(mut v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    v.sort();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for s in v {
        match out.last_mut() {
            Some(last) if s.0 <= last.1 => last.1 = last.1.max(s.1),
            _ => out.push(s),
        }
    }
    out
}

fn rms_around(x: &[(i32, i32)], c: usize, s: (usize, usize), w: usize) -> f64 {
    let lo = s.0.saturating_sub(w)..s.0;
    let hi = s.1..(s.1 + w).min(x.len());
    let n = lo.len() + hi.len();
    (lo.chain(hi).map(|i| (ch(&x[i], c) as f64).powi(2)).sum::<f64>() / n.max(1) as f64).sqrt()
}

fn peak_change(a: &[(i32, i32)], b: &[(i32, i32)], c: usize, s: (usize, usize)) -> f64 {
    (s.0..s.1).map(|i| (ch(&a[i], c) - ch(&b[i], c)).abs()).max().unwrap_or(0) as f64
}

/// The fwd/rev combinations: input with the kept spans copied from the run that made them.
fn combine(input: &[(i32, i32)], f: &[(i32, i32)], r: &[(i32, i32)], exposed: bool, sr: usize) -> Frames {
    let mut out = input.to_vec();
    let w = sr / 100;
    for c in 0..2 {
        let fs = spans(input, f, c);
        let rs = spans(input, r, c);
        let is_exposed = |run: &[(i32, i32)], s: (usize, usize)| {
            peak_change(input, run, c, s) >= rms_around(run, c, s, w).max(1.0) * 10f64.powf(EXPOSED_DB / 20.0)
        };
        let copy = |out: &mut Frames, run: &[(i32, i32)], s: (usize, usize)| {
            for i in s.0..s.1 {
                set_ch(&mut out[i], c, ch(&run[i], c));
            }
        };
        for &s in &fs {
            if overlaps(s, &rs, 4) || (exposed && is_exposed(f, s)) {
                copy(&mut out, f, s);
            }
        }
        if exposed {
            for &s in &rs {
                if !overlaps(s, &fs, 4) && is_exposed(r, s) {
                    copy(&mut out, r, s);
                }
            }
        }
    }
    out
}

#[derive(Default, Clone)]
struct Score {
    truth: [usize; 4],
    found: [usize; 4],
    clean_false: usize,
    false_snr: Vec<f64>,
    noisy_false: usize,
    music_sig: f64,
    music_err: f64,
    click_before: f64,
    click_after: f64,
    seconds: f64,
    cpu: f64,
    runs: usize,
}

impl Score {
    fn add(&mut self, o: &Score) {
        for b in 0..4 {
            self.truth[b] += o.truth[b];
            self.found[b] += o.found[b];
        }
        self.clean_false += o.clean_false;
        self.false_snr.extend_from_slice(&o.false_snr);
        self.noisy_false += o.noisy_false;
        self.music_sig += o.music_sig;
        self.music_err += o.music_err;
        self.click_before += o.click_before;
        self.click_after += o.click_after;
        self.seconds += o.seconds;
        self.cpu += o.cpu;
        self.runs += o.runs;
    }
}

struct Truth {
    /// Per channel: sorted spans, and each one's exposure bin.
    spans: [Vec<(usize, usize)>; 2],
    bins: [Vec<usize>; 2],
    /// Clean music plus declicked noise.
    target: Frames,
}

fn run(params: &CrParameters, sr: u32, x: &[(i32, i32)]) -> (Frames, f64) {
    let t = Instant::now();
    let (out, _) = Processor::process_slice_aligned(params.clone(), sr, x);
    (out, t.elapsed().as_secs_f64())
}

fn truth_for(clean: &[(i32, i32)], noise: &[(i32, i32)], sr: u32, base: &CrParameters) -> Truth {
    let mut p = base.clone();
    p.engine = Engine::Classic;
    p.dcl = TRUTH_DCL;
    p.reverse = false;
    let (nf, _) = run(&p, sr, noise);
    p.reverse = true;
    let (nr, _) = run(&p, sr, noise);
    // The declicked noise: forward repairs, plus the reverse ones forward did not make.
    let mut target_noise = noise.to_vec();
    let w = sr as usize / 100;
    let audible = FULL_SCALE * 10f64.powf(AUDIBLE_DB / 20.0);
    let mut spans_out: [Vec<(usize, usize)>; 2] = [Vec::new(), Vec::new()];
    let mut bins: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    for c in 0..2 {
        let fs = spans(noise, &nf, c);
        let rs = spans(noise, &nr, c);
        for &s in &fs {
            for i in s.0..s.1 {
                set_ch(&mut target_noise[i], c, ch(&nf[i], c));
            }
        }
        for &s in &rs {
            if !overlaps(s, &fs, 0) {
                for i in s.0..s.1 {
                    set_ch(&mut target_noise[i], c, ch(&nr[i], c));
                }
            }
        }
        let union = merge(fs.into_iter().chain(rs).collect());
        for s in union {
            let peak = peak_change(noise, &target_noise, c, s);
            if peak < audible {
                continue;
            }
            let exposure = 20.0 * (peak / rms_around(clean, c, s, w).max(1.0)).log10();
            spans_out[c].push(s);
            bins[c].push(BINS.iter().position(|b| exposure >= b.0 && exposure < b.1).unwrap());
        }
    }
    let target = clean
        .iter()
        .zip(&target_noise)
        .map(|(a, b)| (a.0.saturating_add(b.0), a.1.saturating_add(b.1)))
        .collect();
    Truth { spans: spans_out, bins, target }
}

/// Scores one output. `clean_out`: the same setting on the clean music alone.
fn score(clean: &[(i32, i32)], dirty: &[(i32, i32)], out: &[(i32, i32)], clean_out: &[(i32, i32)], truth: &Truth, lo: usize, hi: usize) -> Score {
    let mut s = Score { seconds: 0.0, ..Default::default() };
    let inside = |sp: &(usize, usize)| sp.0 >= lo && sp.1 <= hi;
    for c in 0..2 {
        let out_spans = spans(dirty, out, c);
        for (k, t) in truth.spans[c].iter().enumerate().filter(|(_, t)| inside(t)) {
            let b = truth.bins[c][k];
            s.truth[b] += 1;
            let changed = (t.0..t.1).filter(|&i| (ch(&out[i], c) - ch(&dirty[i], c)).abs() > ROUNDOFF).count();
            if 2 * changed >= t.1 - t.0 {
                s.found[b] += 1;
            }
        }
        for sp in out_spans.iter().filter(|sp| inside(sp)) {
            if !overlaps(*sp, &truth.spans[c], 8) {
                s.noisy_false += 1;
            }
        }
        for sp in spans(clean, clean_out, c).iter().filter(|sp| inside(sp)) {
            s.clean_false += 1;
            let sig: f64 = (sp.0..sp.1).map(|i| (ch(&clean[i], c) as f64).powi(2)).sum();
            let err: f64 = (sp.0..sp.1).map(|i| ((ch(&clean_out[i], c) - ch(&clean[i], c)) as f64).powi(2)).sum();
            s.false_snr.push(10.0 * (sig.max(1.0) / err.max(1.0)).log10());
        }
        let mut in_click = vec![false; hi - lo];
        for t in &truth.spans[c] {
            for i in t.0.max(lo)..t.1.min(hi) {
                in_click[i - lo] = true;
            }
        }
        for i in lo..hi {
            let e_out = (ch(&out[i], c) as f64 - ch(&truth.target[i], c) as f64).powi(2);
            if in_click[i - lo] {
                s.click_before += (ch(&dirty[i], c) as f64 - ch(&truth.target[i], c) as f64).powi(2);
                s.click_after += e_out;
            } else {
                s.music_sig += (ch(&clean[i], c) as f64).powi(2);
                s.music_err += e_out;
            }
        }
    }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = "scratch/test_reference_album.wav".to_string();
    let mut rip_dirs: Vec<String> = Vec::new();
    let mut source: Option<String> = None;
    let mut dcls = vec![20, 30, 40, 50];
    let mut excerpts: Option<usize> = None;
    let mut rebuild = false;
    let mut engine = Engine::Classic;
    let mut save: Option<String> = None;
    let mut noise_gain = 0.0f64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rips" => {
                rip_dirs.push(args[i + 1].clone());
                i += 1;
            }
            "--source" => {
                source = Some(args[i + 1].to_lowercase());
                i += 1;
            }
            "--dcl" => {
                dcls = args[i + 1].split(',').map(|v| v.parse().unwrap()).collect();
                i += 1;
            }
            "--excerpts" => {
                excerpts = Some(args[i + 1].parse().unwrap());
                i += 1;
            }
            "--engine" => {
                engine = Engine::parse(&args[i + 1]).expect("--engine classic|enhanced");
                i += 1;
            }
            "--save" => {
                save = Some(args[i + 1].clone());
                i += 1;
            }
            "--noise-gain" => {
                noise_gain = args[i + 1].parse().unwrap();
                i += 1;
            }
            "--rebuild" => rebuild = true,
            other => path = other.to_string(),
        }
        i += 1;
    }

    let mut reader = AudioFileReader::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let sr = reader.meta.sample_rate;
    let srz = sr as usize;
    let clean_all = reader.read_all_frames().unwrap();
    let (mut mms, mut mn) = (0.0, 0usize);
    for b in clean_all.chunks_exact(srz) {
        let ms = mean_square(b);
        if db(ms) >= MUSIC_MIN_DB {
            mms += ms;
            mn += 1;
        }
    }
    let ref_music_ms = mms / mn.max(1) as f64;

    let bank = match (rebuild, load_bank()) {
        (false, Some(b)) => b,
        _ => {
            eprintln!("Cutting the noise bank from {:?}...", rip_dirs);
            build_bank(&rip_dirs, ref_music_ms, sr)
        }
    };
    let bank: Vec<&Segment> = bank.iter().filter(|s| source.as_ref().is_none_or(|t| s.source.to_lowercase().contains(t))).collect();
    assert!(!bank.is_empty(), "no noise segments (check --rips / --source)");
    let mut sources: Vec<&str> = bank.iter().map(|s| s.source.as_str()).collect();
    sources.dedup();
    let bank_s = bank.iter().map(|s| s.frames.len()).sum::<usize>() / srz;

    // Excerpts tile the reference; their scored middles do not overlap.
    let len = EXCERPT_S * srz;
    let step = (EXCERPT_S - 2 * WARM_S) * srz;
    let mut starts: Vec<usize> = (0..).map(|k| srz + k * step).take_while(|s| s + len <= clean_all.len()).collect();
    if let Some(n) = excerpts {
        let all = starts.clone();
        starts = (0..n.min(all.len())).map(|k| all[k * all.len() / n.min(all.len())]).collect();
    }
    println!(
        "{path} ({sr} Hz): {} excerpts of {EXCERPT_S} s; noise: {bank_s} s from {} rips{}\n",
        starts.len(),
        sources.len(),
        source.as_ref().map_or(String::new(), |t| format!(" matching \"{t}\""))
    );

    let mut base = CrParameters::preset_stereo_lp();
    base.method = Method::Wavelet;
    base.engine = engine;
    let configs = ["fwd", "rev", "and", "and+exp"];
    // Per excerpt: scores for each (dcl, config).
    let per_excerpt: Vec<Vec<Score>> = starts
        .par_iter()
        .enumerate()
        .map(|(k, &st)| {
            let clean = &clean_all[st..st + len];
            let g = 10f64.powf(noise_gain / 20.0);
            let noise: Frames = noise_for(&bank, k * 7, len)
                .into_iter()
                .map(|f| ((f.0 as f64 * g).round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32, (f.1 as f64 * g).round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32))
                .collect();
            let dirty: Frames = clean.iter().zip(&noise).map(|(a, b)| (a.0.saturating_add(b.0).clamp(-16_777_216, 16_777_215), a.1.saturating_add(b.1).clamp(-16_777_216, 16_777_215))).collect();
            let truth = truth_for(clean, &noise, sr, &base);
            let (lo, hi) = (WARM_S * srz, len - WARM_S * srz);
            let mut row = Vec::new();
            for &dcl in &dcls {
                let mut p = base.clone();
                p.dcl = dcl;
                p.reverse = false;
                let (f, tf) = run(&p, sr, &dirty);
                let (fc, _) = run(&p, sr, clean);
                p.reverse = true;
                let (r, tr) = run(&p, sr, &dirty);
                let (rc, _) = run(&p, sr, clean);
                let outs = [
                    (f.clone(), fc.clone(), tf),
                    (r.clone(), rc.clone(), tr),
                    (combine(&dirty, &f, &r, false, srz), combine(clean, &fc, &rc, false, srz), tf + tr),
                    (combine(&dirty, &f, &r, true, srz), combine(clean, &fc, &rc, true, srz), tf + tr),
                ];
                for (out, cout, t) in outs {
                    let mut s = score(clean, &dirty, &out, &cout, &truth, lo, hi);
                    s.seconds = (hi - lo) as f64 / srz as f64;
                    s.cpu = t;
                    s.runs = 1;
                    row.push(s);
                }
            }
            eprint!(".");
            row
        })
        .collect();
    eprintln!();

    let mut header = format!("{:>4} {:<8} | {:>7} {:>6}", "dcl", "rows", "audible", "found");
    for b in BINS {
        header += &format!(" {:>6}", b.2);
    }
    header += &format!(" | {:>8} {:>7} | {:>8} {:>8} {:>8} | {:>6}", "clean f/s", "f SNR", "noisy f/s", "music dB", "click dB", "x rt");
    println!("{header}");
    let mut tsv = String::from("dcl\tconfig\tfound\tfound_lt0\tfound_0_10\tfound_10_20\tfound_gt20\tclean_false_s\tfalse_snr\tnoisy_false_s\tmusic_db\tclick_db\tx_rt\n");
    for (d, &dcl) in dcls.iter().enumerate() {
        for (ci, cname) in configs.iter().enumerate() {
            let mut tot = Score::default();
            for row in &per_excerpt {
                tot.add(&row[d * configs.len() + ci]);
            }
            let pct = |f: usize, t: usize| 100.0 * f as f64 / t.max(1) as f64;
            let all_t: usize = tot.truth.iter().sum();
            let all_f: usize = tot.found.iter().sum();
            let mut fs = tot.false_snr.clone();
            fs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let fsnr = fs.get(fs.len() / 2).copied().unwrap_or(f64::NAN);
            let music_db = 10.0 * (tot.music_sig / tot.music_err.max(1.0)).log10();
            let click_db = 10.0 * (tot.click_before / tot.click_after.max(1.0)).log10();
            // The combinations cost both runs.
            let x_rt = tot.seconds / tot.cpu.max(1e-9);
            let aud_t: usize = tot.truth[1..].iter().sum();
            let aud_f: usize = tot.found[1..].iter().sum();
            let mut line = format!("{:>4} {:<8} | {:>6.1}% {:>5.1}%", dcl, cname, pct(aud_f, aud_t), pct(all_f, all_t));
            for b in 0..4 {
                line += &format!(" {:>5.1}%", pct(tot.found[b], tot.truth[b]));
            }
            line += &format!(
                " | {:>8.2} {:>7.1} | {:>8.2} {:>8.1} {:>8.1} | {:>6.0}",
                tot.clean_false as f64 / tot.seconds,
                fsnr,
                tot.noisy_false as f64 / tot.seconds,
                music_db,
                click_db,
                x_rt
            );
            println!("{line}");
            tsv += &format!(
                "{dcl}\t{cname}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.3}\t{:.2}\t{:.3}\t{:.2}\t{:.2}\t{:.1}\n",
                pct(aud_f, aud_t),
                pct(all_f, all_t),
                pct(tot.found[0], tot.truth[0]),
                pct(tot.found[1], tot.truth[1]),
                pct(tot.found[2], tot.truth[2]),
                pct(tot.found[3], tot.truth[3]),
                tot.clean_false as f64 / tot.seconds,
                fsnr,
                tot.noisy_false as f64 / tot.seconds,
                music_db,
                click_db,
                x_rt
            );
            if d == 0 && ci == 0 {
                eprintln!("(true clicks per bin: {:?}, over {:.0} s)", tot.truth, tot.seconds);
            }
        }
        println!();
    }
    if let Some(f) = save {
        std::fs::write(&f, tsv).unwrap();
        println!("saved {f}");
    }
}
