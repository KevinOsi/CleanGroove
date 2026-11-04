//! CleanGroove Command-Line Batch Audio Restoration Utility

use std::path::PathBuf;
use std::time::Instant;
use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};

use cr_audio_io::{AudioFileReader, AudioFileWriter};
use cr_dsp::params::{ChannelMode, CrParameters, Method};
use cr_dsp::post::{to_internal, LevelMeter, LevelReport, PostSettings};
use cr_dsp::process::Processor;

#[derive(Parser, Debug)]
#[command(
    name = "cleangroove-cli",
    author = "CleanGroove Contributors",
    version = "0.1.0",
    about = "Open-source audio click and crackle restoration pipeline"
)]
struct Cli {
    /// Input WAV / FLAC / AIFF audio file path
    #[arg(short, long)]
    input: PathBuf,

    /// Output repaired WAV audio file path
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Optional click-difference (noise-only: original - repaired) WAV file path
    #[arg(long)]
    diff_output: Option<PathBuf>,

    /// Automatically tune restoration parameters using physics-guided diagnostic analysis
    #[arg(long)]
    auto: bool,

    /// Run auto-tuner analysis, display diagnostic report and recommended command, then exit
    #[arg(long)]
    analyze_only: bool,

    /// Built-in factory preset
    #[arg(long, value_enum)]
    preset: Option<PresetArg>,

    /// DeClick sensitivity (0..=100, 0 = disabled)
    #[arg(long)]
    dcl: Option<i32>,

    /// DeCrackle sensitivity (0..=100, 0 = disabled)
    #[arg(long)]
    dcr: Option<i32>,

    /// Restoration method
    #[arg(long, value_enum)]
    method: Option<MethodArg>,

    /// Channel operating mode
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,

    /// Enable harmonic pitch protection (brass & vocals)
    #[arg(long)]
    pitch_protection: Option<bool>,

    /// Run detection state machine in reverse time
    #[arg(long)]
    reverse: Option<bool>,

    /// Skip the post-declick chain (corrections and level check): declick only
    #[arg(long)]
    no_post: bool,

    /// Post chain: subsonic high-pass cutoff in Hz, or "off" (default 18)
    #[arg(long)]
    subsonic: Option<String>,

    /// Post chain: elliptical EQ (Side high-pass) cutoff in Hz, or "off" (default 100; stereo only)
    #[arg(long)]
    elliptical: Option<String>,

    /// Post chain: remove this delay of the right channel behind the left, microseconds
    #[arg(long, allow_hyphen_values = true)]
    azimuth_us: Option<f64>,

    /// Post chain: remove this left-minus-right level difference, dB
    #[arg(long, allow_hyphen_values = true)]
    balance_db: Option<f64>,

    /// Post chain: flip the right channel's polarity
    #[arg(long)]
    flip_polarity: Option<bool>,

    /// Post chain: sum to mono after the corrections (mono records captured in stereo)
    #[arg(long)]
    mono_sum: Option<bool>,

    /// Repair needle thumps: loud thumps repeating once per turn of the record (debris
    /// throwing the needle). Run after declicking, before the corrections. --analyze-only
    /// reports whether a record has any; listen there before switching this on.
    #[arg(long)]
    dethump: bool,
}

/// Parses a cutoff flag: a frequency in Hz, or "off".
fn parse_cutoff(flag: &str, value: &str) -> Result<Option<f64>> {
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let hz: f64 = value.parse().with_context(|| format!("--{flag} expects a frequency in Hz or \"off\", got {value:?}"))?;
    anyhow::ensure!(hz > 0.0, "--{flag} must be above 0 Hz");
    Ok(Some(hz))
}

fn describe_post(p: &PostSettings) -> String {
    let mut parts = Vec::new();
    if p.flip_right_polarity {
        parts.push("flip right polarity".to_string());
    }
    if p.balance_db != 0.0 {
        parts.push(format!("balance {:+.2} dB", p.balance_db));
    }
    if p.azimuth_us != 0.0 {
        parts.push(format!("azimuth {:+.2} us", p.azimuth_us));
    }
    if let Some(f) = p.subsonic_hz {
        parts.push(format!("subsonic {f:.0} Hz"));
    }
    if p.mono_sum {
        parts.push("mono sum".to_string());
    } else if let Some(f) = p.elliptical_hz {
        parts.push(format!("elliptical {f:.0} Hz"));
    }
    if parts.is_empty() { "off".to_string() } else { parts.join(", ") }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum PresetArg {
    StereoLp,
    StereoBrass,
    StereoPercussion,
    MonoLp,
    Shellac78,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum MethodArg {
    Simple,
    Wavelet,
    WaveletX2,
    WaveletX3,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum ModeArg {
    Stereo,
    Merge,
    Mono,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    // 1. Open input audio file and inspect format
    let mut reader = AudioFileReader::open(&args.input)
        .with_context(|| format!("Failed to open input audio file: {}", args.input.display()))?;
    let meta = reader.meta;

    println!(
        "Audio Format: {} Hz, {} channels, {}-bit {}",
        meta.sample_rate,
        meta.channels,
        meta.bits_per_sample,
        if meta.is_float { "float" } else { "PCM" }
    );

    // 2. Configure base parameters (either via Auto-Tuner or Preset / CLI flags)
    let (mut params, analysis) = if args.auto || args.analyze_only {
        println!("Analyzing audio with the Auto-Tuner...");
        let frames = reader
            .read_all_frames()
            .with_context(|| format!("Failed to read audio frames for auto-tuning: {}", args.input.display()))?;

        let analysis_start = Instant::now();
        let analysis = cr_dsp::analysis::analyze_album(
            &frames,
            meta.sample_rate,
            meta.channels,
            |_, _| {},
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("analysis is never cancelled here");
        let analysis_secs = analysis_start.elapsed().as_secs_f64();

        let report = cr_dsp::autotune::autotune_audio_with_analysis(
            &frames,
            meta.sample_rate,
            meta.channels,
            Some(&analysis),
            |frac, msg| {
                println!("[{:.0}%] {}", frac * 100.0, msg);
            },
        );

        print_album_analysis(&analysis, analysis_secs);

        println!("\n================================================================================");
        println!(" CleanGroove Auto-Tuner Report");
        println!("================================================================================");
        println!(
            " Click Condition:          {}{}",
            report.condition,
            if report.condition_estimated { " (estimated, no silence between tracks)" } else { "" }
        );
        println!(
            " Clicks Between Tracks:    {:.2} audible/s, {:.2} loud pops/s ({:.0} s measured)",
            report.audible_clicks_per_sec, report.loud_pops_per_sec, report.gap_seconds_analyzed
        );
        println!(
            " Groove Mode:              {:?} (Midband Corr: {:.4}, Side/Mid Ratio: {:.4})",
            report.recommended.mode, report.lr_midband_correlation, report.side_to_mid_ratio
        );
        println!(
            " Suggested Start:          dcl = {}, {:?}, Reverse: {}, Pitch Protection: {}",
            report.recommended.dcl,
            report.recommended.method,
            if report.recommended.reverse { "on" } else { "off" },
            if report.recommended.detect_pitch { "on" } else { "off" }
        );
        println!(
            " Repair Rate (estimate):   roughly 1 in {:.0} samples (over 1 in 200 is suspect)",
            1.0 / report.repair_fraction.max(1e-9)
        );
        match report.music_pickup_dcl {
            Some(d) => println!(" Music Pickup:             repairs start catching music above dcl ~{}", d),
            None => println!(" Music Pickup:             already present at the lowest swept dcl"),
        }
        println!("--------------------------------------------------------------------------------");
        println!(" Sweep ({:.0} s sampled across the file):", report.sampled_seconds);
        println!("  dcl | Music Repairs/s | Repair Rate");
        println!(" -----+-----------------+-------------");
        for row in &report.sweep_table {
            println!(
                "  {:3} | {:15.1} | 1 in {:7.0}",
                row.dcl,
                row.music_repairs_per_sec,
                1.0 / row.repair_fraction.max(1e-9)
            );
        }
        println!("--------------------------------------------------------------------------------");
        println!(" Rationale:");
        for r in &report.rationale {
            println!("  - {}", r);
        }
        println!("================================================================================\n");

        if args.analyze_only {
            let rec_method_flag = match report.recommended.method {
                Method::Simple => "simple",
                Method::Wavelet => "wavelet",
                Method::WaveletX2 => "wavelet-x2",
                Method::WaveletX3 => "wavelet-x3",
            };
            // Mono records go through the post chain: declick in stereo, then align and sum.
            let rec_mode_flag = "stereo";
            let post = analysis.suggested_post();
            let mut post_flags = String::new();
            if post.flip_right_polarity {
                post_flags.push_str(" --flip-polarity true");
            }
            if post.balance_db != 0.0 {
                post_flags.push_str(&format!(" --balance-db {:.2}", post.balance_db));
            }
            if post.azimuth_us != 0.0 {
                post_flags.push_str(&format!(" --azimuth-us {:.2}", post.azimuth_us));
            }
            if post.mono_sum {
                post_flags.push_str(" --mono-sum true");
            } else if let Some(f) = post.elliptical_hz {
                post_flags.push_str(&format!(" --elliptical {f:.0}"));
            }
            let rev_flag = if report.recommended.reverse {
                " --reverse true"
            } else {
                ""
            };
            let pitch_flag = if report.recommended.detect_pitch {
                " --pitch-protection true"
            } else {
                ""
            };
            let input_str = args.input.display().to_string();
            let output_str = args
                .output
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| {
                    let stem = args.input.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
                    format!("{}_repaired.wav", stem)
                });

            println!(
                " Recommended Command:\n   cleangroove-cli --input \"{}\" --output \"{}\" --dcl {} --method {} --mode {}{}{}{}\n",
                input_str, output_str, report.recommended.dcl, rec_method_flag, rec_mode_flag, rev_flag, pitch_flag, post_flags
            );
            return Ok(());
        }

        (report.recommended, Some(analysis))
    } else {
        let preset = match args.preset {
            Some(PresetArg::StereoLp) | None => CrParameters::preset_stereo_lp(),
            Some(PresetArg::StereoBrass) => CrParameters::preset_stereo_brass(),
            Some(PresetArg::StereoPercussion) => CrParameters::preset_stereo_percussion(),
            Some(PresetArg::MonoLp) => CrParameters::preset_mono_lp(),
            Some(PresetArg::Shellac78) => CrParameters::preset_78_rpm(),
        };
        (preset, None)
    };

    // Post-declick chain: the analysis' suggestion with --auto, otherwise the defaults (18 Hz
    // subsonic, 100 Hz elliptical); the flags below override either.
    let mut post = match &analysis {
        Some(a) => a.suggested_post(),
        None => PostSettings {
            subsonic_hz: Some(cr_dsp::analysis::SUBSONIC_HZ),
            elliptical_hz: Some(cr_dsp::analysis::ELLIPTICAL_HZ),
            ..Default::default()
        },
    };
    if post.mono_sum && args.mode.is_none() {
        // Mono record: declick both channels separately; the post chain aligns and sums them.
        params.mode = ChannelMode::Stereo;
    }

    // User CLI flags override individual parameter fields
    if let Some(dcl) = args.dcl {
        params.dcl = dcl.clamp(0, 100);
    }
    if let Some(dcr) = args.dcr {
        params.dcr = dcr.clamp(0, 100);
    }
    if let Some(m) = args.method {
        params.method = match m {
            MethodArg::Simple => Method::Simple,
            MethodArg::Wavelet => Method::Wavelet,
            MethodArg::WaveletX2 => Method::WaveletX2,
            MethodArg::WaveletX3 => Method::WaveletX3,
        };
    }
    if let Some(m) = args.mode {
        params.mode = match m {
            ModeArg::Stereo => ChannelMode::Stereo,
            ModeArg::Merge => ChannelMode::Merge,
            ModeArg::Mono => ChannelMode::Mono,
        };
    }
    if let Some(pp) = args.pitch_protection {
        params.detect_pitch = pp;
    }
    if let Some(rev) = args.reverse {
        params.reverse = rev;
    }
    if let Some(v) = &args.subsonic {
        post.subsonic_hz = parse_cutoff("subsonic", v)?;
    }
    if let Some(v) = &args.elliptical {
        post.elliptical_hz = parse_cutoff("elliptical", v)?;
    }
    if let Some(v) = args.azimuth_us {
        post.azimuth_us = v;
    }
    if let Some(v) = args.balance_db {
        post.balance_db = v;
    }
    if let Some(v) = args.flip_polarity {
        post.flip_right_polarity = v;
    }
    if let Some(v) = args.mono_sum {
        post.mono_sum = v;
    }
    if args.no_post {
        post = PostSettings::default();
    }
    let use_post = !post.is_identity();

    let output_path = args
        .output
        .as_ref()
        .context("Output file path (--output / -o) is required for audio restoration")?;

    // Needle thumps: the series come from the analysis (--auto) or a pass over the input. The
    // original is kept: the repair is measured, and the noise diff written, after it.
    let mut reader = reader;
    let dethump = if args.dethump {
        let original = if args.auto {
            AudioFileReader::open(&args.input)?.read_all_frames()?
        } else {
            let frames = reader.read_all_frames()?;
            reader = AudioFileReader::open(&args.input)?;
            frames
        };
        let series = match &analysis {
            Some(a) => a.thumps.clone(),
            None => cr_dsp::thump::find_thumps(&original, meta.sample_rate).1.into_iter().filter(|s| s.is_thump_like()).collect(),
        };
        Some((original, series))
    } else {
        None
    };

    println!("============================================================");
    println!(" CleanGroove DSP Restoration Engine");
    println!(
        " Mode: {:?}, Method: {:?}, DeClick: {}, DeCrackle: {}",
        params.mode, params.method, params.dcl, params.dcr
    );
    println!(" Pitch Protection: {}, Reverse: {}", params.detect_pitch, params.reverse);
    println!(" Post-declick:     {}", describe_post(&post));
    if let Some((_, series)) = &dethump {
        println!(
            " Needle thumps:    repair {} in {} series",
            series.iter().map(|s| s.events.len()).sum::<usize>(),
            series.len()
        );
    }
    println!(" Input:  {}", args.input.display());
    println!(" Output: {}", output_path.display());
    println!("============================================================");

    // If we ran auto-tuning above, re-open reader for streaming processing
    if args.auto {
        reader = AudioFileReader::open(&args.input)?;
    }

    // 3. Initialize audio writers
    let mut writer = AudioFileWriter::create(output_path, meta)
        .with_context(|| format!("Failed to create output audio file: {}", output_path.display()))?;

    let mut diff_writer = if let Some(ref diff_path) = args.diff_output {
        println!("Noise Diff: {}", diff_path.display());
        Some(AudioFileWriter::create(diff_path, meta)?)
    } else {
        None
    };

    // 4. Initialize DSP processor with known total frames
    let total_frames = if meta.total_frames > 0 {
        Some(meta.total_frames)
    } else {
        None
    };
    let mut processor = Processor::new(params, meta.sample_rate, total_frames);

    let pb = ProgressBar::new(meta.total_frames as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );

    // 5. Stream processing in 65,536-frame chunks
    let start_time = Instant::now();
    let chunk_size = 65536;
    let mut chunk = Vec::with_capacity(chunk_size);
    let mut out_chunk = Vec::with_capacity(chunk_size);
    let mut written_count = 0;

    // Ring buffer to compute time-aligned diff output (orig_delayed - repaired_out), also used
    // to count changed samples for the final repair rate.
    let diff_buf_size = 131072;
    let diff_mask = diff_buf_size - 1;
    let mut orig_ring_l = vec![0i32; diff_buf_size];
    let mut orig_ring_r = vec![0i32; diff_buf_size];
    let mut in_frame_idx: usize = 0;
    let mut out_frame_idx: usize = 0;
    // Samples changed by more than wavelet round-off, both channels.
    let mut changed_samples: usize = 0;
    // With the post chain or the thump repair, the declicked audio is kept in memory: the
    // thumps are repaired over the whole of it, and the level check measures the chain's output
    // before anything is written, so one album gain can be applied.
    let buffered = use_post || dethump.is_some();
    // With the thump repair the diff and the repair rate are taken afterwards, from the original.
    let stream_diff = dethump.is_none();
    let mut declicked: Vec<(i32, i32)> = Vec::with_capacity(if buffered { meta.total_frames } else { 0 });

    while let Ok(read_count) = reader.read_chunk(chunk_size, &mut chunk) {
        if read_count == 0 {
            break;
        }

        for &(l, r) in &chunk {
            orig_ring_l[in_frame_idx & diff_mask] = l;
            orig_ring_r[in_frame_idx & diff_mask] = r;
            in_frame_idx += 1;
        }

        processor.process_chunk(&chunk, &mut out_chunk);
        if buffered {
            declicked.extend_from_slice(&out_chunk);
        } else {
            writer.write_chunk(&out_chunk)?;
        }

        for &(out_l, out_r) in out_chunk.iter().filter(|_| stream_diff) {
            let orig_l = orig_ring_l[out_frame_idx & diff_mask];
            let orig_r = orig_ring_r[out_frame_idx & diff_mask];
            changed_samples += ((orig_l - out_l).abs() > 2) as usize + ((orig_r - out_r).abs() > 2) as usize;
            if let Some(ref mut dw) = diff_writer {
                dw.write_frame(orig_l - out_l, orig_r - out_r)?;
            }
            out_frame_idx += 1;
        }
        written_count += out_chunk.len();

        // Progress bar updated ONCE per 65,536 chunk
        pb.inc(read_count as u64);
    }

    // Flush remaining pipeline delay
    let flushed = processor.flush();
    for (out_l, out_r) in flushed {
        let should_write = if let Some(tf) = total_frames {
            written_count < tf
        } else {
            true
        };
        if should_write {
            if buffered {
                declicked.push((out_l, out_r));
            } else {
                writer.write_frame(out_l, out_r)?;
            }
            written_count += 1;
            if !stream_diff {
                continue;
            }
            let orig_l = orig_ring_l[out_frame_idx & diff_mask];
            let orig_r = orig_ring_r[out_frame_idx & diff_mask];
            changed_samples += ((orig_l - out_l).abs() > 2) as usize + ((orig_r - out_r).abs() > 2) as usize;
            if let Some(ref mut dw) = diff_writer {
                dw.write_frame(orig_l - out_l, orig_r - out_r)?;
            }
            out_frame_idx += 1;
        }
    }

    // Safety: ensure exact 1:1 frame count parity if total_frames is known
    if let Some(tf) = total_frames {
        while written_count < tf {
            if buffered {
                declicked.push((0, 0));
            } else {
                writer.write_frame(0, 0)?;
            }
            if let Some(ref mut dw) = diff_writer.as_mut().filter(|_| stream_diff) {
                let orig_l = orig_ring_l[out_frame_idx & diff_mask];
                let orig_r = orig_ring_r[out_frame_idx & diff_mask];
                dw.write_frame(orig_l, orig_r)?;
            }
            out_frame_idx += 1;
            written_count += 1;
        }
    }

    pb.finish_with_message("Done");

    // Needle thumps, then the repair rate and noise diff of declick and thump repair together.
    let mut thump_report = None;
    if let Some((original, series)) = &dethump {
        let repaired = cr_dsp::thump::repair_thumps(&mut declicked, meta.sample_rate, series);
        thump_report = Some((repaired.len(), repaired.iter().map(|(a, b)| b - a).sum::<usize>()));
        // The repair rate stays the declicker's (its "1 in 200 is suspect" rule is for clicks).
        let mut in_thump = vec![false; declicked.len()];
        for &(a, b) in &repaired {
            in_thump[a..b.min(declicked.len())].fill(true);
        }
        for (i, (&(ol, or), &(l, r))) in original.iter().zip(&declicked).enumerate() {
            if !in_thump[i] {
                changed_samples += ((ol - l).abs() > 2) as usize + ((or - r).abs() > 2) as usize;
            }
            if let Some(ref mut dw) = diff_writer {
                dw.write_frame(ol - l, or - r)?;
            }
        }
        if !use_post {
            writer.write_chunk(&declicked)?;
        }
    }

    // Post-declick chain: write its output while measuring it. If it pushed peaks over full
    // scale, write it again with one album gain (no limiting or compression).
    let write_post = |writer: &mut AudioFileWriter, settings: &PostSettings| -> Result<LevelReport> {
        let mut meter = LevelMeter::new(meta.sample_rate);
        let mut result = Ok(());
        cr_dsp::post::run_post(&declicked, meta.sample_rate, settings, |block| {
            meter.feed(block);
            for &(l, r) in block {
                if result.is_ok() {
                    result = writer.write_frame(to_internal(l), to_internal(r));
                }
            }
        });
        result?;
        Ok(meter.finish())
    };
    let levels = if use_post {
        let mut report = write_post(&mut writer, &post)?;
        let gain_db = cr_dsp::post::album_gain_db(&report);
        if gain_db < 0.0 {
            drop(writer);
            writer = AudioFileWriter::create(output_path, meta)?;
            let mut with_gain = post.clone();
            with_gain.gain_db = gain_db;
            report = write_post(&mut writer, &with_gain)?;
        }
        Some((gain_db, report))
    } else {
        None
    };
    writer.finalize()?;
    if let Some(dw) = diff_writer {
        dw.finalize()?;
    }

    let elapsed = start_time.elapsed().as_secs_f64();
    let audio_duration = (written_count as f64) / (meta.sample_rate as f64);
    let speedup = if elapsed > 0.0 { audio_duration / elapsed } else { 0.0 };

    println!("============================================================");
    println!(" Restoration Complete!");
    println!(" Frames Processed: {}", written_count);
    println!(" Elapsed Time:     {:.2} s", elapsed);
    println!(" Audio Duration:   {:.2} s", audio_duration);
    println!(" Processing Speed: {:.1}x real-time", speedup);
    if changed_samples > 0 {
        println!(
            " Repair Rate:      1 in {:.0} samples changed (over 1 in 200 is suspect for vinyl)",
            (2 * written_count) as f64 / changed_samples as f64
        );
    } else {
        println!(" Repair Rate:      no samples changed");
    }
    if let Some((spans, frames)) = thump_report {
        println!(
            " Needle Thumps:    {} repaired, {:.0} ms in all",
            spans,
            frames as f64 * 1000.0 / meta.sample_rate as f64
        );
    }
    if let Some((gain_db, r)) = levels {
        println!(
            " Output Levels:    peak {:.2} dBFS, true peak {:.2} dBTP, loudness {:.1} LUFS (ReplayGain album {:+.2} dB)",
            r.sample_peak_dbfs, r.true_peak_dbtp, r.loudness_lufs, r.replaygain_db()
        );
        if gain_db < 0.0 {
            println!(
                " Album Gain:       {:.2} dB applied: the post chain pushed peaks over full scale",
                gain_db
            );
        }
    }
    println!("============================================================");

    Ok(())
}

/// Prints the album analysis: click grade, levels, channels and the worst spots.
fn print_album_analysis(a: &cr_dsp::analysis::AlbumAnalysis, secs: f64) {
    let sr = a.sample_rate as f64;
    let clock = |frame: usize| {
        let t = frame as f64 / sr;
        format!("{:02}:{:04.1}", (t / 60.0).floor() as u32, t % 60.0)
    };
    println!("\n================================================================================");
    println!(" Record Analysis ({:.1} s)", secs);
    println!("================================================================================");
    println!(
        " Click Condition:          {}{} - {:.2} audible clicks/s, {:.2} loud pops/s ({:.0} s measured)",
        a.condition,
        if a.condition_estimated { " (estimated)" } else { "" },
        a.audible_clicks_per_sec,
        a.loud_pops_per_sec,
        a.grade_seconds
    );
    println!(
        " Repairs at dcl {}:        1 in {:.0} samples over the whole file, {} longer than 2 ms",
        cr_dsp::analysis::ANALYSIS_DCL,
        1.0 / a.repair_fraction.max(1e-12),
        a.long_repairs
    );
    println!(
        " Levels:                   peak {:.1} dBFS, music average {:.1} dBFS RMS, {} clipped samples",
        a.peak_dbfs, a.music_rms_db, a.clipped_samples
    );
    println!(
        " Channels:                 {:?} (correlation {:.3}, side/mid {:.4})",
        a.mode, a.lr_midband_correlation, a.side_to_mid_ratio
    );
    let c = &a.channels;
    println!(
        " Balance (L-R above 150 Hz): {:+.2} dB (spread {:.2} dB)",
        c.balance_db, c.balance_spread_db
    );
    println!(
        " Azimuth (R behind L):     {:+.2} us +- {:.2} (spread {:.2} us, halves {:+.2} / {:+.2} us, {} windows)",
        c.azimuth_us, c.azimuth_se_us, c.azimuth_spread_us, c.azimuth_halves_us.0, c.azimuth_halves_us.1, c.azimuth_windows
    );
    println!(
        " Rumble below 150 Hz:      Side {:.1} / Mid {:.1} dBFS between tracks; Side in music {:.1} dBFS; below 18 Hz in music {:.1} dBFS",
        c.side_rumble_gap_db, c.mid_rumble_gap_db, c.side_lf_music_db, c.subsonic_music_db
    );
    let p = a.suggested_post();
    println!(
        " Suggested Corrections:    polarity {}, balance {}, azimuth {}, subsonic {}, elliptical {}, mono sum {}",
        if p.flip_right_polarity { "flip right" } else { "ok" },
        if p.balance_db != 0.0 { format!("{:+.2} dB", p.balance_db) } else { "none".to_string() },
        if p.azimuth_us != 0.0 { format!("{:+.2} us", p.azimuth_us) } else { "none".to_string() },
        p.subsonic_hz.map_or("off".to_string(), |f| format!("{f:.0} Hz")),
        p.elliptical_hz.map_or("off".to_string(), |f| format!("{f:.0} Hz")),
        if p.mono_sum { "on" } else { "off" }
    );
    if a.thumps.is_empty() {
        println!(" Needle Thumps:            none found");
    } else {
        println!(
            " Needle Thumps:            {} series repeating once per turn (debris throwing the needle). Listen there;",
            a.thumps.len()
        );
        println!("                           if you hear thumps, add --dethump to repair them:");
        for t in &a.thumps {
            println!(
                "   {} - {}  {} thumps, {:.0} dB above the music",
                clock(t.events[0].frame),
                clock(t.events[t.events.len() - 1].frame),
                t.events.len(),
                t.events.iter().map(|e| e.above_db).sum::<f64>() / t.events.len() as f64
            );
        }
    }
    println!(" Problem Spots:");
    for s in &a.problem_spots {
        println!(
            "   {}  {:>3} pops {:>4} exposed clicks  music {:.0} dBFS{}",
            clock(s.start_frame),
            s.pops,
            s.clicks,
            s.level_db,
            format!(
                "{}{}",
                if s.has_long_repair() { format!("  long repair {:.1} ms", s.longest_repair_ms) } else { String::new() },
                if s.near_silence { "  (next to silence: needle drop or run-out?)" } else { "" }
            )
        );
    }
}
