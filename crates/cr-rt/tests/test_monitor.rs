use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_rt::live::{LevelSnapshot, LiveConfig, LiveEvent, LiveSession};

#[test]
#[ignore = "Interactive live hardware monitor; run manually with cargo test -- --ignored --nocapture"]
fn test_live_stream_monitor() {
    let input = std::env::var("CR_INPUT").ok().or_else(|| Some("USB".to_string()));
    let output = std::env::var("CR_OUTPUT").ok().or_else(|| Some("Realtek".to_string()));
    let record_path = std::env::var("CR_RECORD").ok().or_else(|| Some(r"C:\VinylRips\album_side_a.wav".to_string())).map(PathBuf::from);
    let sample_rate: u32 = std::env::var("CR_RATE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48000); // 48 kHz native USB CODEC rate
    let sensitivity: u8 = std::env::var("CR_SENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let sens = if sensitivity <= 9 {
        RtSensitivity::Stepped(sensitivity)
    } else {
        RtSensitivity::Fine(sensitivity.min(100))
    };

    let params = RtParameters {
        sensitivity: sens,
        method: Method::Wavelet,
        mode: RtSoundMode::SoundOut,
        detect_pitch: true,
        reverse: false,
        parallel: true,
    };

    println!("============================================================");
    println!(" CleanGroove Real-Time Restoration Engine");
    println!(" Mode: SoundOut, Method: Wavelet, Sensitivity: {} (s = {})", sensitivity, sens.effective_s());
    println!(" Input Device Filter: {:?}", input);
    println!(" Output Device Filter: {:?}", output);
    println!(" Target Sample Rate: {} Hz", sample_rate);
    if let Some(ref rec) = record_path {
        println!(" Raw Tape Recording: {}", rec.display());
        if let Some(parent) = rec.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    println!("============================================================");

    let running = Arc::new(AtomicBool::new(true));
    let r_ctrlc = running.clone();
    let _ = ctrlc::set_handler(move || {
        println!("\nStopping monitor...");
        r_ctrlc.store(false, Ordering::SeqCst);
    });

    let stop_input = running.clone();
    thread::spawn(move || {
        let mut line = String::new();
        while stop_input.load(Ordering::Relaxed) {
            if std::io::stdin().read_line(&mut line).is_ok() {
                println!("\nUser pressed Enter. Stopping monitor gracefully...");
                stop_input.store(false, Ordering::SeqCst);
                break;
            }
        }
    });

    let session = match LiveSession::start(LiveConfig {
        input_device: input,
        output_device: output,
        sample_rate,
        params,
        record: record_path,
        monitor_volume: 1.0,
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nStream error: {}", e);
            return;
        }
    };
    println!("Latency: {:.0} ms", session.info().latency_ms());

    let mut levels = LevelSnapshot::default();
    while running.load(Ordering::Relaxed) {
        match session.events().recv_timeout(Duration::from_millis(200)) {
            Ok(LiveEvent::Levels(l)) => levels = l,
            Ok(LiveEvent::Telemetry(telem)) => {
                let bars = |p: f32| ((20.0 * p.max(1e-3).log10() + 60.0) / 60.0 * 20.0).clamp(0.0, 20.0) as usize;
                let act_bars = (telem.click_activity * 20.0).clamp(0.0, 20.0) as usize;
                print!(
                    "\r[L: {:<20}] [R: {:<20}] Clicks: [{:<20}] Fixes: L={} R={}",
                    "=".repeat(bars(levels.input_peak[0])),
                    "=".repeat(bars(levels.input_peak[1])),
                    "#".repeat(act_bars),
                    telem.active_l_fixes,
                    telem.active_r_fixes
                );
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
            Ok(LiveEvent::Warning(w)) => println!("\nWarning: {}", w),
            Ok(LiveEvent::Error(e)) => eprintln!("\nError: {}", e),
            _ => {}
        }
    }

    match session.stop() {
        Ok(report) => println!("\nReal-time stream stopped gracefully. Tape frames: {:?}", report.tape.map(|t| t.frames)),
        Err(e) => eprintln!("\nStop error: {}", e),
    }
}
