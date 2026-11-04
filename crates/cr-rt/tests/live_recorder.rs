//! Recorder controls of a live session (Rec / Pause / Resume / Stop), driven through the
//! engine's input half and capture side without audio hardware.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cr_audio_io::AudioFileReader;
use cr_dsp::params::RtParameters;
use cr_rt::live::engine::{Engine, StreamPlan, CAPTURE_RING_FRAMES};
use cr_rt::live::recorder::{labels_path, next_take_path, TapeState};
use cr_rt::tape::{TapeRecorder, TapeSidecarMetadata};

fn engine() -> Engine {
    let plan = StreamPlan {
        requested_rate: 44_100,
        input_rate: 44_100,
        output_rate: 44_100,
        input_channels: 2,
        output_channels: 2,
    };
    Engine::build(&plan, RtParameters::default(), 1.0, Some("Test Device".into())).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cr_rec_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Frames numbered from `start`, so every frame in a file can be traced to where it came from
/// (even values: 24-bit PCM << 1, stored exactly).
fn ramp(start: i32, frames: usize) -> Vec<i32> {
    (0..frames as i32)
        .flat_map(|i| [(start + i) * 2, -(start + i) * 2])
        .collect()
}

fn push(e: &mut Engine, data: &[i32]) {
    e.input.push_interleaved(data, 2, |x| x);
}

fn read(path: &Path) -> Vec<i32> {
    let mut r = AudioFileReader::open(path).unwrap();
    r.read_all_frames()
        .unwrap()
        .into_iter()
        .flat_map(|(l, r)| [l, r])
        .collect()
}

fn sidecar(path: &Path) -> TapeSidecarMetadata {
    serde_json::from_str(&fs::read_to_string(TapeRecorder::sidecar_path(path)).unwrap()).unwrap()
}

#[test]
fn pause_skips_audio_and_marks_the_join() {
    let dir = temp_dir("pause");
    let wav = dir.join("side_a.wav");
    let mut e = engine();

    e.capture.start(&wav, RtParameters::default()).unwrap();
    push(&mut e, &ramp(0, 10_000));
    e.capture.pump().unwrap();
    // Audio queued but not yet written when Pause is pressed still belongs to the take.
    push(&mut e, &ramp(10_000, 2_000));
    e.capture.pause().unwrap();
    assert_eq!(e.capture.status().state, TapeState::Paused);

    // While paused: some audio is read and ignored, some is still queued at Resume.
    push(&mut e, &ramp(50_000, 3_000));
    e.capture.pump().unwrap();
    push(&mut e, &ramp(60_000, 3_000));
    e.capture.resume().unwrap();

    push(&mut e, &ramp(90_000, 7_000));
    let report = e.capture.stop().unwrap();

    let mut expected = ramp(0, 12_000);
    expected.extend(ramp(90_000, 7_000));
    assert_eq!(read(&wav), expected);
    assert_eq!(report.frames, 19_000);
    assert_eq!(report.pauses.len(), 1);
    assert_eq!(report.pauses[0].at_frame, 12_000);
    assert_eq!(report.dropped, 0);

    let labels = report.labels_path.clone().unwrap();
    assert_eq!(labels, labels_path(&wav));
    assert_eq!(labels.file_name().unwrap(), "side_a_labels.txt");
    let at = 12_000.0 / 44_100.0;
    assert_eq!(
        fs::read_to_string(&labels).unwrap(),
        format!("{:.6}\t{:.6}\tPause 1\n", at, at)
    );

    let meta = sidecar(&wav);
    assert_eq!(meta.total_frames_recorded, 19_000);
    assert_eq!(meta.pauses, report.pauses);
    assert_eq!(meta.input_device.as_deref(), Some("Test Device"));
    assert_eq!(meta.dropped_frames, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn audio_before_rec_is_not_recorded_and_no_labels_without_pause() {
    let dir = temp_dir("prerec");
    let wav = dir.join("take.wav");
    let mut e = engine();

    // Monitoring runs before Rec: the capture side reads and discards.
    push(&mut e, &ramp(0, 4_000));
    e.capture.pump().unwrap();
    push(&mut e, &ramp(4_000, 1_000));
    e.capture.start(&wav, RtParameters::default()).unwrap();
    push(&mut e, &ramp(20_000, 5_000));
    let report = e.capture.stop().unwrap();

    assert_eq!(read(&wav), ramp(20_000, 5_000));
    assert!(report.labels_path.is_none());
    assert!(!labels_path(&wav).exists());
    assert_eq!(e.capture.status().state, TapeState::Idle);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn takes_follow_one_another_and_never_overwrite() {
    let dir = temp_dir("takes");
    let mut e = engine();
    let t0 = SystemTime::now();

    let first = next_take_path(&dir, "{name}_side{n}", "Kind of Blue", t0);
    assert_eq!(first, dir.join("Kind of Blue_side1.wav"));
    e.capture.start(&first, RtParameters::default()).unwrap();
    assert!(e.capture.start(&first, RtParameters::default()).is_err(), "one take at a time");
    push(&mut e, &ramp(0, 1_000));
    e.capture.stop().unwrap();

    let second = next_take_path(&dir, "{name}_side{n}", "Kind of Blue", t0);
    assert_eq!(second, dir.join("Kind of Blue_side2.wav"));
    // Starting on an existing file is refused, and the file is left alone.
    let err = e.capture.start(&first, RtParameters::default()).unwrap_err();
    assert!(err.to_string().contains("already exists"));
    assert_eq!(read(&first), ramp(0, 1_000));

    e.capture.start(&second, RtParameters::default()).unwrap();
    push(&mut e, &ramp(5_000, 500));
    assert_eq!(e.capture.stop().unwrap().frames, 500);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn controls_out_of_order_are_errors() {
    let dir = temp_dir("order");
    let mut e = engine();
    assert!(e.capture.pause().is_err());
    assert!(e.capture.resume().is_err());
    assert!(e.capture.stop().is_err());

    e.capture.start(&dir.join("x.wav"), RtParameters::default()).unwrap();
    assert!(e.capture.resume().is_err(), "not paused");
    e.capture.pause().unwrap();
    assert!(e.capture.pause().is_err(), "already paused");
    // Resume straight after pause, with nothing recorded in between, marks no join.
    e.capture.resume().unwrap();
    assert!(e.capture.stop().unwrap().pauses.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn finish_closes_an_open_take() {
    let dir = temp_dir("finish");
    let wav = dir.join("open.wav");
    let mut e = engine();
    assert!(e.capture.finish().unwrap().is_none());

    e.capture.start(&wav, RtParameters::default()).unwrap();
    push(&mut e, &ramp(0, 70_000)); // more than one pump's worth
    let report = e.capture.finish().unwrap().unwrap();
    assert_eq!(report.frames, 70_000);
    assert_eq!(read(&wav), ramp(0, 70_000));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn frames_lost_from_a_take_are_counted_in_the_take() {
    let dir = temp_dir("lost");
    let wav = dir.join("lost.wav");
    let mut e = engine();
    e.capture.start(&wav, RtParameters::default()).unwrap();
    // The capture side stalls (slow disk): the queue overflows by 50 frames.
    push(&mut e, &vec![0i32; (CAPTURE_RING_FRAMES + 50) * 2]);
    assert_eq!(e.capture.status().dropped, 50);
    let report = e.capture.stop().unwrap();
    assert_eq!(report.dropped, 50);
    assert_eq!(report.frames, CAPTURE_RING_FRAMES as u64);
    assert_eq!(sidecar(&wav).dropped_frames, 50);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn name_template_tokens() {
    let dir = temp_dir("template");
    // 2026-10-30 21:05:09 UTC
    let t = UNIX_EPOCH + Duration::from_secs(1_793_394_309);

    assert_eq!(
        next_take_path(&dir, "{date}_{time}_{name}", "AC/DC: Back in Black?", t),
        dir.join("2026-10-30_21-05-09_AC_DC_ Back in Black_.wav")
    );
    assert_eq!(next_take_path(&dir, "{name}.wav", "  ", t), dir.join("recording.wav"));

    // Without {n}, an existing name gets a number added.
    fs::write(dir.join("rip.wav"), b"x").unwrap();
    fs::write(dir.join("rip_2.wav"), b"x").unwrap();
    assert_eq!(next_take_path(&dir, "{name}", "rip", t), dir.join("rip_3.wav"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sidecars_from_before_live_takes_still_load() {
    let old = r#"{
        "version": "1.0",
        "created_at": "2026-10-01T10:00:00Z",
        "sample_rate": 48000,
        "channels": 2,
        "bits_per_sample": 24,
        "total_frames_recorded": 1000,
        "rt_monitoring_settings": {
            "sensitivity": 5, "effective_s": 40, "method": "Wavelet",
            "pitch_protection": true, "monitoring_mode": "SoundOut"
        },
        "recommended_offline_command": "cleangroove-cli"
    }"#;
    let m: TapeSidecarMetadata = serde_json::from_str(old).unwrap();
    assert!(m.pauses.is_empty());
    assert_eq!(m.input_device, None);
    assert_eq!(m.dropped_frames, 0);
}
