//! Live waveform columns: aligned with the input, repairs marked where the clicks are.

use cr_dsp::params::{Method, RtParameters, RtSensitivity, RtSoundMode};
use cr_rt::live::engine::{Engine, StreamPlan};
use cr_rt::live::{WaveColumn, COLUMNS_PER_SEC};

fn engine(rate: u32, params: RtParameters) -> Engine {
    let plan = StreamPlan {
        requested_rate: rate,
        input_rate: rate,
        output_rate: rate,
        input_channels: 2,
        output_channels: 2,
    };
    Engine::build(&plan, params, 1.0, None).unwrap()
}

fn params(method: Method, mode: RtSoundMode) -> RtParameters {
    RtParameters {
        sensitivity: RtSensitivity::Stepped(6),
        method,
        mode,
        detect_pitch: true,
        reverse: false,
        parallel: true,
    }
}

/// Interleaved internal-domain tone (24-bit PCM << 1) with single-sample clicks.
fn tone_with_clicks(rate: u32, frames: usize, clicks: &[usize]) -> Vec<i32> {
    let mut v = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let t = f as f64 / rate as f64;
        let mut l = (0.2 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8_388_607.0) as i32;
        let mut r = (0.2 * (2.0 * std::f64::consts::PI * 550.0 * t).sin() * 8_388_607.0) as i32;
        if clicks.contains(&f) {
            l = 7_000_000;
            r = -7_000_000;
        }
        v.push(l * 2);
        v.push(r * 2);
    }
    v
}

/// Runs everything through the engine like a live stream and returns the columns.
fn run(e: &mut Engine, data: &[i32]) -> Vec<WaveColumn> {
    let mut cols = Vec::new();
    let mut sink = vec![0i32; 2048];
    for chunk in data.chunks(2048) {
        e.input.push_interleaved(chunk, 2, |x| x);
        while e.worker.step() > 0 {}
        cols.extend(e.worker.take_wave());
        e.output.fill_interleaved(&mut sink, 2, |x| x);
    }
    cols
}

fn check_clicks_marked(rate: u32, method: Method) {
    let frames = rate as usize * 4;
    let clicks: Vec<usize> = (1..8).map(|k| k * rate as usize / 2 + 123).collect();
    let mut e = engine(rate, params(method, RtSoundMode::SoundOut));
    let latency = e.worker.latency_frames();
    let cols = run(&mut e, &tone_with_clicks(rate, frames, &clicks));

    let per_col = (rate / COLUMNS_PER_SEC) as usize;
    // Everything up to (frames - latency) has come out of the pipeline.
    let complete = (frames - latency) / per_col - 1;
    assert!(cols.len() >= complete, "{} columns, expected {}", cols.len(), complete);
    for (k, c) in cols.iter().enumerate() {
        assert_eq!(c.index, k as u64, "columns are contiguous from 0");
    }

    let near_click = |idx: u64| clicks.iter().any(|&f| (f / per_col) as u64 == idx);
    for &f in clicks.iter().filter(|&&f| f < frames - latency) {
        let c = &cols[f / per_col];
        assert!(
            c.repaired[0] > 0 && c.repaired[1] > 0,
            "{} Hz {:?}: click at frame {} not marked in column {}: {:?}",
            rate,
            method,
            f,
            f / per_col,
            c
        );
    }
    let stray: Vec<u64> = cols
        .iter()
        .filter(|c| (c.repaired[0] + c.repaired[1]) > 0 && !near_click(c.index))
        .map(|c| c.index)
        .collect();
    assert!(stray.is_empty(), "{} Hz {:?}: repairs marked away from any click: {:?}", rate, method, stray);
}

#[test]
fn repairs_are_marked_on_the_click_columns() {
    check_clicks_marked(44_100, Method::Wavelet);
    check_clicks_marked(44_100, Method::Simple);
    check_clicks_marked(48_000, Method::Wavelet);
}

#[test]
fn repairs_are_marked_at_96k_too() {
    // Decimated processing: each processor step covers two hardware frames.
    check_clicks_marked(96_000, Method::Wavelet);
}

#[test]
fn sound_in_columns_line_up_exactly() {
    // In Sound In the output is the input, delayed: aligned columns have equal peaks.
    let rate = 48_000;
    let mut e = engine(rate, params(Method::Wavelet, RtSoundMode::SoundIn));
    let cols = run(&mut e, &tone_with_clicks(rate, rate as usize * 2, &[30_000]));
    assert!(cols.len() > 100);
    for c in &cols {
        assert_eq!(c.orig_peak, c.out_peak, "column {}", c.index);
        assert_eq!(c.mode, RtSoundMode::SoundIn);
    }
}

#[test]
fn noise_mode_output_is_the_removed_click() {
    let rate = 48_000;
    let click = 40_000;
    let mut e = engine(rate, params(Method::Wavelet, RtSoundMode::Noise));
    let cols = run(&mut e, &tone_with_clicks(rate, rate as usize * 2, &[click]));
    let per_col = (rate / COLUMNS_PER_SEC) as usize;
    for c in &cols {
        if c.index == (click / per_col) as u64 {
            assert!(c.out_peak[0] > 0.3, "removed click shows in Noise: {:?}", c);
        } else if c.repaired == [0, 0] {
            assert_eq!(c.out_peak, [0.0, 0.0], "nothing removed in column {}", c.index);
        }
    }
}

#[test]
fn clipped_input_is_counted_per_column() {
    let rate = 44_100;
    let mut data = tone_with_clicks(rate, rate as usize, &[]);
    let per_col = (rate / COLUMNS_PER_SEC) as usize;
    let at = 50 * per_col + 7;
    for k in 0..3 {
        data[2 * (at + k)] = 16_777_214;
    }
    let mut e = engine(rate, params(Method::Simple, RtSoundMode::SoundOut));
    let cols = run(&mut e, &data);
    assert_eq!(cols[50].clipped, [3, 0]);
    assert!(cols.iter().filter(|c| c.index != 50).all(|c| c.clipped == [0, 0]));
}

#[test]
fn method_change_keeps_columns_in_input_time() {
    let rate = 48_000;
    let mut e = engine(rate, params(Method::Wavelet, RtSoundMode::SoundOut));
    let first = run(&mut e, &tone_with_clicks(rate, rate as usize, &[]));
    let last_before = first.last().unwrap().index;

    assert!(e.worker.set_params(params(Method::Simple, RtSoundMode::SoundOut)));
    let latency = e.worker.latency_frames();
    let clicks = [rate as usize / 2];
    let after = run(&mut e, &tone_with_clicks(rate, rate as usize, &clicks));
    // The frames inside the old pipeline are lost: the columns jump once, by about the old
    // latency, and stay in input time.
    let per_col = (rate / COLUMNS_PER_SEC) as usize;
    assert!(after[0].index > last_before);
    let gaps: Vec<u64> = after
        .windows(2)
        .filter(|w| w[1].index != w[0].index + 1)
        .map(|w| w[1].index - w[0].index)
        .collect();
    assert_eq!(gaps.len(), 1, "one gap at the restart: {:?}", gaps);
    let old_latency_cols = (10_240 / per_col) as u64;
    assert!(gaps[0].abs_diff(old_latency_cols) <= 2, "gap {} columns", gaps[0]);
    // A click half a second into the new block lands in its input column.
    let click_col = ((rate as usize + clicks[0]) / per_col) as u64;
    if (rate as usize / 2) < rate as usize - latency {
        let c = after.iter().find(|c| c.index == click_col).expect("click column present");
        assert!(c.repaired[0] > 0, "{:?}", c);
    }
}
