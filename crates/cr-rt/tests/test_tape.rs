use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use cr_dsp::params::RtParameters;
use cr_rt::tape::{TapeRecorder, TapeSidecarMetadata};

#[test]
fn test_tape_recorder_split_writes_nonempty_wav() {
    let temp_dir = std::env::temp_dir().join(format!("cr_test_tape_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let wav_path: PathBuf = temp_dir.join("test_split.wav");

    // Clean up if existing
    let _ = fs::remove_file(&wav_path);
    let _ = fs::remove_file(TapeRecorder::sidecar_path(&wav_path));

    let params = RtParameters::default();
    let tape = TapeRecorder::new(&wav_path, 48000, 24, params)
        .expect("Failed to create TapeRecorder");

    let (mut producer, handle) = tape.split();

    // Push 10,000 stereo frames via TapeProducer
    for i in 0..10_000 {
        let l = ((i as f64 * 0.05).sin() * 5_000_000.0) as i32;
        let r = ((i as f64 * 0.05).cos() * 5_000_000.0) as i32;
        while !producer.push_frame(l, r) {
            thread::sleep(Duration::from_millis(1));
        }
    }

    // Stop recording and finalize WAV and JSON metadata
    let written = handle.stop().expect("Tape handle stop failed");

    assert_eq!(written, 10_000, "Expected exactly 10,000 frames written");

    // Assert WAV file size > 60,000 bytes (10,000 frames * 2 ch * 3 bytes + RIFF header)
    let meta = fs::metadata(&wav_path).expect("Failed to read WAV metadata");
    assert!(
        meta.len() >= 60_000,
        "WAV file size {} is smaller than 60,000 bytes",
        meta.len()
    );

    // Assert JSON sidecar parses cleanly
    let sidecar_path = TapeRecorder::sidecar_path(&wav_path);
    let sidecar_content = fs::read_to_string(&sidecar_path)
        .expect("Failed to read companion sidecar JSON file");
    let sidecar: TapeSidecarMetadata = serde_json::from_str(&sidecar_content)
        .expect("Failed to deserialize TapeSidecarMetadata");

    assert_eq!(sidecar.total_frames_recorded, 10_000);
    assert_eq!(sidecar.sample_rate, 48000);
    assert_eq!(sidecar.channels, 2);
    assert_eq!(sidecar.bits_per_sample, 24);
    assert!(!sidecar.created_at.is_empty());

    // Clean up
    let _ = fs::remove_file(&wav_path);
    let _ = fs::remove_file(&sidecar_path);
    let _ = fs::remove_dir(&temp_dir);
}
