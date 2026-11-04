use std::io::Cursor;
use cr_audio_io::AudioFileReader;
use cr_dsp::params::{
    scale_i24_to_internal, Method, RtParameters,
    RtSensitivity, RtSoundMode,
};
use cr_rt::backend::pipe_host::run_pipe_stream_io;
use cr_rt::tape::{TapeRecorder, TapeSidecarMetadata};

#[test]
fn test_tape_recorder_lifecycle_and_metadata() {
    let temp_dir = std::env::temp_dir();
    let wav_path = temp_dir.join(format!("cr_rt_test_tape_{}.wav", std::process::id()));
    let meta_path = TapeRecorder::sidecar_path(&wav_path);

    let mut params = RtParameters::default();
    params.method = Method::Wavelet;
    params.sensitivity = RtSensitivity::Stepped(6);
    params.detect_pitch = true;

    let total_frames = 20_000;
    let mut recorder = TapeRecorder::new(&wav_path, 44100, 24, params).expect("Failed to create TapeRecorder");

    // Feed frames: 440 Hz sine wave
    for f in 0..total_frames {
        let t = f as f64 / 44100.0;
        let l_sample = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8388607.0) as i32;
        let r_sample = (0.5 * (2.0 * std::f64::consts::PI * 880.0 * t).sin() * 8388607.0) as i32;

        let l_int = scale_i24_to_internal(l_sample);
        let r_int = scale_i24_to_internal(r_sample);

        // Keep pushing until accepted (in case worker hasn't caught up)
        while !recorder.push_frame(l_int, r_int) {
            std::thread::yield_now();
        }
    }

    let written = recorder.stop().expect("Failed to stop TapeRecorder");
    assert_eq!(written, total_frames, "TapeRecorder frame count mismatch");

    // Verify WAV file
    assert!(wav_path.exists(), "WAV file was not created");
    let reader = AudioFileReader::open(&wav_path).expect("Failed to open recorded WAV");
    let meta = reader.meta;
    assert_eq!(meta.channels, 2);
    assert_eq!(meta.sample_rate, 44100);
    assert_eq!(meta.bits_per_sample, 24);
    assert_eq!(meta.total_frames, total_frames);

    // Verify sidecar metadata JSON
    assert!(meta_path.exists(), "Sidecar metadata JSON was not created");
    let meta_str = std::fs::read_to_string(&meta_path).expect("Failed to read metadata file");
    let json_meta: TapeSidecarMetadata = serde_json::from_str(&meta_str).expect("Failed to parse TapeSidecarMetadata JSON");
    assert_eq!(json_meta.channels, 2);
    assert_eq!(json_meta.sample_rate, 44100);
    assert_eq!(json_meta.bits_per_sample, 24);
    assert_eq!(json_meta.total_frames_recorded, total_frames);
    assert_eq!(json_meta.rt_monitoring_settings.method, "Wavelet");
    assert!(json_meta.rt_monitoring_settings.pitch_protection);

    // Cleanup
    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_file(&meta_path);
}

#[test]
fn test_pipe_stream_io_synthetic_audio_click_repair() {
    let mut params = RtParameters::default();
    params.method = Method::Wavelet;
    params.sensitivity = RtSensitivity::Stepped(6);
    params.detect_pitch = true;
    params.mode = RtSoundMode::SoundOut;

    let total_frames = 4096;
    let mut raw_bytes = vec![0u8; total_frames * 6];

    // Generate clean tone with a huge impulsive click at frame 2000
    let click_frame = 2000;
    for f in 0..total_frames {
        let t = f as f64 / 44100.0;
        let mut l_val = (0.2 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8388607.0) as i32;
        let mut r_val = (0.2 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8388607.0) as i32;

        if f == click_frame {
            l_val = 7_000_000; // Impulsive click
            r_val = -7_000_000;
        }

        let offset = f * 6;
        raw_bytes[offset] = (l_val & 0xFF) as u8;
        raw_bytes[offset + 1] = ((l_val >> 8) & 0xFF) as u8;
        raw_bytes[offset + 2] = ((l_val >> 16) & 0xFF) as u8;

        raw_bytes[offset + 3] = (r_val & 0xFF) as u8;
        raw_bytes[offset + 4] = ((r_val >> 8) & 0xFF) as u8;
        raw_bytes[offset + 5] = ((r_val >> 16) & 0xFF) as u8;
    }

    let input_cursor = Cursor::new(raw_bytes);
    let mut output_bytes = Vec::new();

    run_pipe_stream_io(params, 44100, None, input_cursor, &mut output_bytes)
        .expect("run_pipe_stream_io failed");

    assert_eq!(output_bytes.len(), total_frames * 6);

    // Read back frame around click_frame
    let read_sample_i24 = |bytes: &[u8], frame: usize| -> (i32, i32) {
        let offset = frame * 6;
        let l = (bytes[offset] as i32)
            | ((bytes[offset + 1] as i32) << 8)
            | ((bytes[offset + 2] as i8 as i32) << 16);
        let r = (bytes[offset + 3] as i32)
            | ((bytes[offset + 4] as i32) << 8)
            | ((bytes[offset + 5] as i8 as i32) << 16);
        (l, r)
    };

    let (out_l, out_r) = read_sample_i24(&output_bytes, click_frame);
    // The original click was 7_000_000. It must be heavily suppressed by repair!
    assert!(out_l.abs() < 4_000_000, "Left click was not repaired: {}", out_l);
    assert!(out_r.abs() < 4_000_000, "Right click was not repaired: {}", out_r);
}

#[test]
fn test_pipe_stream_with_simultaneous_tape_recording() {
    let temp_dir = std::env::temp_dir();
    let wav_path = temp_dir.join(format!("cr_rt_test_pipe_tape_{}.wav", std::process::id()));
    let meta_path = TapeRecorder::sidecar_path(&wav_path);

    let params = RtParameters::default();
    let total_frames = 8192;
    let mut raw_bytes = vec![0u8; total_frames * 6];

    for f in 0..total_frames {
        let t = f as f64 / 44100.0;
        let l_val = (0.25 * (2.0 * std::f64::consts::PI * 300.0 * t).sin() * 8388607.0) as i32;
        let r_val = (0.25 * (2.0 * std::f64::consts::PI * 600.0 * t).sin() * 8388607.0) as i32;

        let offset = f * 6;
        raw_bytes[offset] = (l_val & 0xFF) as u8;
        raw_bytes[offset + 1] = ((l_val >> 8) & 0xFF) as u8;
        raw_bytes[offset + 2] = ((l_val >> 16) & 0xFF) as u8;

        raw_bytes[offset + 3] = (r_val & 0xFF) as u8;
        raw_bytes[offset + 4] = ((r_val >> 8) & 0xFF) as u8;
        raw_bytes[offset + 5] = ((r_val >> 16) & 0xFF) as u8;
    }

    let tape = TapeRecorder::new(&wav_path, 44100, 24, params.clone()).expect("Failed to create TapeRecorder");
    let input_cursor = Cursor::new(raw_bytes);
    let mut output_bytes = Vec::new();

    run_pipe_stream_io(params, 44100, Some(tape), input_cursor, &mut output_bytes)
        .expect("run_pipe_stream_io with tape failed");

    assert_eq!(output_bytes.len(), total_frames * 6);
    assert!(wav_path.exists());
    assert!(meta_path.exists());

    let reader = AudioFileReader::open(&wav_path).expect("Failed to open tape WAV");
    assert_eq!(reader.meta.total_frames, total_frames);

    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_file(&meta_path);
}
