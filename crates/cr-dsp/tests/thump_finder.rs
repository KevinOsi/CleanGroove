//! Rotational thump finder (cr_dsp::thump) on synthetic signals.

use cr_dsp::thump::{find_thumps, repair_spans, repair_thumps, thump_spans, REPAIR_CONTEXT};

const SR: usize = 44_100;

fn noise(seed: &mut u64) -> f64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
}

/// A needle thump at sample `at`: a 1 ms burst, then a 60 Hz swing decaying over ~8 ms.
fn thump(i: usize, at: usize, seed: &mut u64) -> f64 {
    if i < at {
        return 0.0;
    }
    let t = (i - at) as f64 / SR as f64;
    if t > 0.03 {
        return 0.0;
    }
    let burst = if t < 0.001 { 6_000_000.0 * noise(seed) } else { 0.0 };
    burst + 5_000_000.0 * (-t / 0.004).exp() * (2.0 * std::f64::consts::PI * 60.0 * t).sin()
}

#[test]
fn test_rotational_thumps_are_found() {
    // A quiet tone with a thump every turn at 33 1/3 (1.8 s) from 1.0 s on: one thump-like
    // series at 1.8 s holding every thump.
    let mut seed = 3u64;
    let n = 16 * SR;
    let starts: Vec<usize> = (0..8).map(|k| SR + k * SR * 18 / 10).collect();
    let x: Vec<(i32, i32)> = (0..n)
        .map(|i| {
            let tone = 200_000.0 * (2.0 * std::f64::consts::PI * 330.0 * i as f64 / SR as f64).sin();
            let th: f64 = starts.iter().map(|&s| thump(i, s, &mut seed)).sum();
            let v = (tone + th + 2_000.0 * noise(&mut seed)) as i32;
            (v, v)
        })
        .collect();
    let (_, series) = find_thumps(&x, SR as u32);
    let thumps: Vec<_> = series.iter().filter(|s| s.is_thump_like()).collect();
    assert_eq!(thumps.len(), 1, "series: {series:?}");
    let s = thumps[0];
    assert_eq!(s.speed, "33");
    assert!((s.period_s - 1.8).abs() < 0.003, "period {}", s.period_s);
    assert_eq!(s.events.len(), 8);
    assert_eq!(s.beat_fraction(), 0.0);
}

#[test]
fn test_a_beat_on_the_rotation_period_is_not_a_thump() {
    // A kick drum on every beat at 133 1/3 bpm: a bar of four lasts exactly 1.8 s, one turn at
    // 33 1/3. The bar-to-bar chain has the other beats half and a quarter of a turn away, so it
    // must not be called a thump.
    let mut seed = 5u64;
    let beat = SR * 45 / 100; // 0.45 s
    let n = 16 * SR;
    let x: Vec<(i32, i32)> = (0..n)
        .map(|i| {
            let t = (i % beat) as f64 / SR as f64;
            let kick = 6_000_000.0 * (-t / 0.006).exp() * (2.0 * std::f64::consts::PI * 70.0 * t).sin();
            let pad = 150_000.0 * (2.0 * std::f64::consts::PI * 220.0 * i as f64 / SR as f64).sin();
            let v = (kick + pad + 2_000.0 * noise(&mut seed)) as i32;
            (v, v)
        })
        .collect();
    let (events, series) = find_thumps(&x, SR as u32);
    assert!(events.len() > 20, "the kicks should be loud transients: {}", events.len());
    assert!(series.iter().all(|s| !s.is_thump_like()), "series: {series:?}");
    assert!(series.iter().any(|s| s.beat_fraction() > 0.5), "the bar chain should look like a beat: {series:?}");
}

#[test]
fn test_long_ar_repair_removes_the_thumps() {
    // The same thumps on a tone: the repair replaces each one with the AR continuation of the
    // tone. Before: about 5,000,000 off the tone at the swing. After: within 1% of that.
    let mut seed = 3u64;
    let n = 16 * SR;
    let starts: Vec<usize> = (0..8).map(|k| SR + k * SR * 18 / 10).collect();
    let tone = |i: usize| 200_000.0 * (2.0 * std::f64::consts::PI * 330.0 * i as f64 / SR as f64).sin();
    let mut x: Vec<(i32, i32)> = (0..n)
        .map(|i| {
            let th: f64 = starts.iter().map(|&s| thump(i, s, &mut seed)).sum();
            let v = (tone(i) + th) as i32;
            (v, v)
        })
        .collect();
    let (_, series) = find_thumps(&x, SR as u32);
    let spans = repair_thumps(&mut x, SR as u32, &series);
    assert_eq!(spans.len(), 8, "spans {spans:?}");
    for &s in &starts {
        // Each thump lies inside a repaired span.
        assert!(spans.iter().any(|&(a, b)| a <= s && s + SR * 8 / 1000 <= b), "thump at {s} not covered: {spans:?}");
        let worst = (s..s + SR * 30 / 1000).map(|i| (x[i].0 as f64 - tone(i)).abs()).fold(0.0, f64::max);
        assert!(worst < 50_000.0, "thump at {s}: residual {worst:.0}");
    }
}

#[test]
fn test_block_repair_matches_whole_file() {
    // Preview repairs thumps block by block (1 s blocks with context either side); every frame
    // must come out exactly as in the whole-file repair the render uses.
    let mut seed = 5u64;
    let n = 16 * SR;
    let starts: Vec<usize> = (0..8).map(|k| SR + k * SR * 18 / 10 + 777).collect();
    let dirty: Vec<(i32, i32)> = (0..n)
        .map(|i| {
            let th: f64 = starts.iter().map(|&s| thump(i, s, &mut seed)).sum();
            let v = 100_000.0 * noise(&mut seed) + th;
            (v as i32, (0.8 * v) as i32)
        })
        .collect();
    let (_, series) = find_thumps(&dirty, SR as u32);
    let mut whole = dirty.clone();
    assert_eq!(repair_thumps(&mut whole, SR as u32, &series).len(), 8);

    let spans = thump_spans(&series, SR as u32);
    let mut blocked = dirty.clone();
    for keep_start in (0..n).step_by(SR) {
        let keep_end = (keep_start + SR).min(n);
        let (a, b) = (keep_start.saturating_sub(REPAIR_CONTEXT), (keep_end + REPAIR_CONTEXT).min(n));
        let mut block = dirty[a..b].to_vec();
        repair_spans(&mut block, a, &spans);
        blocked[keep_start..keep_end].copy_from_slice(&block[keep_start - a..keep_end - a]);
    }
    assert!(whole == blocked, "block-wise repair differs from the whole-file repair");
}
