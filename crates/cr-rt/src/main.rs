//! CleanGroove Real-Time Restoration CLI Runner

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_rt::backend::pipe_host::run_pipe_stream;
use cr_rt::live::{
    next_take_path, LevelSnapshot, LiveConfig, LiveEvent, LiveSession, TapeReport, TapeState,
    TapeStatus, DEFAULT_TEMPLATE,
};
use cr_rt::list_devices;
use cr_rt::tape::TapeRecorder;

#[derive(Parser, Debug)]
#[command(name = "cleangroove-rt", author, version, about = "CleanGroove Real-Time Restoration & Archiving Engine")]
struct Cli {
    /// List available audio input and output devices and exit
    #[arg(long)]
    list_devices: bool,

    /// Audio input device name (partial match, e.g. "USB", "Line")
    #[arg(short = 'i', long)]
    input: Option<String>,

    /// Audio output device name (partial match, e.g. "Headphones", "Speakers")
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Audio sample rate in Hz (44100, 48000, 88200, 96000, 192000)
    #[arg(short = 'r', long, default_value_t = 44100)]
    sample_rate: u32,

    /// DeClick sensitivity: 0..=9 stepped slider (default: 5) or 0..=100 fine slider
    #[arg(short = 's', long, default_value_t = 5)]
    sensitivity: u8,

    /// Restoration method: simple or wavelet
    #[arg(short = 'm', long, value_enum, default_value_t = MethodArg::Wavelet)]
    method: MethodArg,

    /// Live monitoring mode: sound-out (repaired), sound-in (bypass), or noise (removed clicks)
    #[arg(long, value_enum, default_value_t = ModeArg::SoundOut)]
    mode: ModeArg,

    /// Harmonic Pitch & Brass Protection
    #[arg(short = 'p', long, default_value_t = true)]
    pitch_protection: bool,

    /// Concurrently record pristine uncompressed raw audio to specified WAV path ("Tape Recorder")
    #[arg(long)]
    record: Option<PathBuf>,

    /// Stream raw 24-bit PCM over stdin and stdout (headless pipe mode)
    #[arg(long)]
    pipe: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum MethodArg {
    Simple,
    Wavelet,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ModeArg {
    SoundOut,
    SoundIn,
    Noise,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    if args.list_devices {
        list_devices()?;
        return Ok(());
    }

    let sens = if args.sensitivity <= 9 {
        RtSensitivity::Stepped(args.sensitivity)
    } else {
        RtSensitivity::Fine(args.sensitivity.min(100))
    };

    let method = match args.method {
        MethodArg::Simple => Method::Simple,
        MethodArg::Wavelet => Method::Wavelet,
    };

    let mode = match args.mode {
        ModeArg::SoundOut => RtSoundMode::SoundOut,
        ModeArg::SoundIn => RtSoundMode::SoundIn,
        ModeArg::Noise => RtSoundMode::Noise,
    };

    let params = RtParameters {
        sensitivity: sens,
        method,
        mode,
        detect_pitch: args.pitch_protection,
        reverse: false,
        parallel: true,
    };

    println!("============================================================");
    println!(" CleanGroove Real-Time Restoration Engine");
    println!(
        " Mode: {:?}, Method: {:?}, Sensitivity: {} (s = {})",
        params.mode,
        params.method,
        args.sensitivity,
        sens.effective_s()
    );
    println!(" Pitch Protection: {}", params.detect_pitch);
    println!(" Target Sample Rate: {} Hz", args.sample_rate);
    if let Some(ref rec) = args.record {
        println!(" Raw Tape Recording: {}", rec.display());
    }
    println!("============================================================");

    if args.pipe {
        let tape = match args.record {
            Some(ref rec_path) => Some(TapeRecorder::new(rec_path, args.sample_rate, 24, params.clone())?),
            None => None,
        };
        println!("Running in Stdin/Stdout Raw PCM Pipe Mode...");
        run_pipe_stream(params, args.sample_rate, tape)?;
        return Ok(());
    }

    let running = Arc::new(AtomicBool::new(true));
    let r_ctrlc = running.clone();
    ctrlc::set_handler(move || {
        r_ctrlc.store(false, Ordering::SeqCst);
    })
    .ok();

    // New takes ("n") go next to --record, named after it, or into the current folder.
    let take_dir = args
        .record
        .as_ref()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_default();
    let take_name = args
        .record
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("recording")
        .to_string();

    let session = LiveSession::start(LiveConfig {
        input_device: args.input,
        output_device: args.output,
        sample_rate: args.sample_rate,
        params,
        record: args.record,
        monitor_volume: 1.0,
    })?;
    println!("Keys (then Enter): p pause, r resume, s stop take, n new take, q quit");

    let (key_tx, keys) = crossbeam_channel::unbounded::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            if key_tx.send(line.trim().to_lowercase()).is_err() {
                break;
            }
            line.clear();
        }
    });
    let info = session.info();
    println!("Input Device:  {} ({})", info.input_device, info.input_format);
    println!("Output Device: {} ({})", info.output_device, info.output_format);
    println!(
        "Rate: {} Hz in, {} Hz out | Latency: {:.0} ms",
        info.plan.input_rate,
        info.plan.output_rate,
        info.latency_ms()
    );

    let mut levels = LevelSnapshot::default();
    let mut tape = TapeStatus::default();
    while running.load(Ordering::Relaxed) {
        match session.events().recv_timeout(Duration::from_millis(200)) {
            Ok(LiveEvent::Levels(l)) => levels = l,
            Ok(LiveEvent::Telemetry(telem)) => {
                let bars = |peak: f32| {
                    ((20.0 * peak.max(1e-3).log10() + 60.0) / 60.0 * 20.0).clamp(0.0, 20.0) as usize
                };
                let act_bars = (telem.click_activity * 20.0).clamp(0.0, 20.0) as usize;
                print!(
                    "\r[L: {:<20}] [R: {:<20}] Clicks: [{:<20}] Fixes: L={} R={}{} {}   ",
                    "=".repeat(bars(levels.input_peak[0])),
                    "=".repeat(bars(levels.input_peak[1])),
                    "#".repeat(act_bars),
                    telem.active_l_fixes,
                    telem.active_r_fixes,
                    if levels.input_clips != [0, 0] { " CLIP" } else { "" },
                    tape_label(&tape)
                );
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
            Ok(LiveEvent::LatencyChanged(ms)) => println!("\nLatency now {:.0} ms", ms),
            Ok(LiveEvent::Warning(w)) => println!("\nWarning: {}", w),
            Ok(LiveEvent::Error(e)) => eprintln!("\nError: {}", e),
            Ok(LiveEvent::TapeStopped(t)) => print_take(&t),
            Ok(LiveEvent::Tape(t)) => tape = t,
            Ok(LiveEvent::Wave(_)) => {}
            Err(_) => {}
        }

        while let Ok(key) = keys.try_recv() {
            let result = match key.as_str() {
                "p" => session.record_pause().map(|_| println!("\nPaused")),
                "r" => session.record_resume().map(|_| println!("\nResumed")),
                "s" => session.record_stop().map(|_| ()),
                "n" => {
                    let path = next_take_path(&take_dir, DEFAULT_TEMPLATE, &take_name, SystemTime::now());
                    println!("\nRecording to {}", path.display());
                    session.record_start(&path)
                }
                "q" => {
                    running.store(false, Ordering::SeqCst);
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                println!("\n{}", e);
            }
        }
    }

    let report = session.stop()?;
    if let Some(t) = report.tape {
        print_take(&t);
    }
    let l = report.levels;
    if l.dropped_input + l.dropped_tape + l.underruns > 0 {
        println!(
            "Dropped: {} input frames, {} recorded frames; {} output underruns",
            l.dropped_input, l.dropped_tape, l.underruns
        );
    }
    println!("\nReal-time stream stopped gracefully.");
    Ok(())
}

/// Recorder state for the status line: "REC 01:23", "PAUSED 01:23" or "not recording".
fn tape_label(t: &TapeStatus) -> String {
    let clock = format!("{:02}:{:02}", (t.seconds / 60.0) as u64, (t.seconds % 60.0) as u64);
    match t.state {
        TapeState::Recording => format!("| REC {}", clock),
        TapeState::Paused => format!("| PAUSED {}", clock),
        TapeState::Idle => "| not recording".to_string(),
    }
}

fn print_take(t: &TapeReport) {
    println!(
        "\n[Tape Recorder] Saved {} frames ({:.1} s) to {}",
        t.frames,
        t.frames as f64 / t.sample_rate as f64,
        t.wav_path.display()
    );
    if let Some(ref labels) = t.labels_path {
        println!("[Tape Recorder] {} pause(s), labels in {}", t.pauses.len(), labels.display());
    }
    if t.dropped > 0 {
        println!("[Tape Recorder] WARNING: {} frames were lost from this take", t.dropped);
    }
}
