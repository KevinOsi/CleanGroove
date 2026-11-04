//! First guess at the splits on a synthetic two-sided rip: lead-in, gaps of groove noise
//! between tracks, a long side change and run-out, with rumble loud enough to hide them from a
//! plain level meter.

use cr_dsp::params::scale_f64_to_internal;
use cr_tracks::{place_tracks, Envelope, ReleaseInfo, ReleaseTrack, Snap};

const SR: u32 = 8000;

enum Part {
    Quiet(f64),
    /// Music, seconds; optionally with a quiet passage (start, length) inside it.
    Music(f64, Option<(f64, f64)>),
    /// A needle drop or lift: a loud burst of noise, not music.
    Drop(f64),
    /// A quiet count-in (hi-hat ticks about 15 dB above the groove noise) that starts the
    /// next piece of music.
    Ticks(f64),
    /// The music fading out by 45 dB, seconds.
    Fade(f64),
}

#[derive(Clone, Copy)]
enum Sound {
    Silent,
    Burst,
    Ticks,
    Music(Option<(f64, f64)>),
    Fade(f64),
}

struct Rip {
    frames: Vec<(i32, i32)>,
    /// Frame where each piece of music starts.
    onsets: Vec<u64>,
}

fn render(parts: &[Part]) -> Rip {
    let mut seed = 12345u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f64 / (1u32 << 24) as f64 - 0.5
    };
    let mut frames = Vec::new();
    let mut onsets = Vec::new();
    let mut count_in = false;
    for p in parts {
        let (secs, sound) = match *p {
            Part::Quiet(s) => (s, Sound::Silent),
            Part::Drop(s) => (s, Sound::Burst),
            Part::Ticks(s) => (s, Sound::Ticks),
            Part::Music(s, hole) => (s, Sound::Music(hole)),
            Part::Fade(s) => (s, Sound::Fade(s)),
        };
        if matches!(sound, Sound::Ticks) || (matches!(sound, Sound::Music(_)) && !count_in) {
            onsets.push(frames.len() as u64);
        }
        count_in = matches!(sound, Sound::Ticks);
        let n = (secs * SR as f64) as usize;
        for i in 0..n {
            let t = frames.len() as f64 / SR as f64;
            let local = i as f64 / SR as f64;
            // Groove noise (about -60 dBFS) and 15 Hz rumble at -30 dBFS everywhere.
            let mut x = 0.002 * noise() + 0.03 * (2.0 * std::f64::consts::PI * 15.0 * t).sin();
            let gain = match sound {
                Sound::Silent => 0.0,
                Sound::Burst => {
                    x += 0.1 * noise();
                    0.0
                }
                Sound::Ticks => {
                    if local % 0.5 < 0.25 {
                        x += 0.014 * noise();
                    }
                    0.0
                }
                Sound::Music(Some((hs, hl))) if local >= hs && local < hs + hl => 0.0,
                Sound::Music(_) => 1.0,
                Sound::Fade(len) => 10f64.powf(-45.0 * local / len / 20.0),
            };
            if gain > 0.0 {
                let w = 2.0 * std::f64::consts::PI * t;
                x += gain * (0.15 * (w * 440.0).sin() + 0.1 * (w * 659.0).sin() + 0.05 * noise());
            }
            let v = scale_f64_to_internal(x);
            frames.push((v, v));
        }
    }
    Rip { frames, onsets }
}

fn track(position: &str, seconds: Option<f64>) -> ReleaseTrack {
    ReleaseTrack { position: position.into(), title: position.into(), artist: "X".into(), seconds }
}

fn assert_near(got: u64, want: u64, what: &str) {
    let d = (got as f64 - want as f64).abs() / SR as f64;
    assert!(d <= 0.1, "{what}: got {:.2} s, want {:.2} s", got as f64 / SR as f64, want as f64 / SR as f64);
}

#[test]
fn test_two_sides_snap_to_gaps_and_side_change() {
    use Part::*;
    let rip = render(&[
        Quiet(1.5),
        Drop(1.0),
        Quiet(2.5),
        Music(30.0, None),
        Quiet(2.0),
        Music(40.0, Some((20.0, 0.6))), // a short break inside the song, far from any boundary
        Quiet(1.5),
        Ticks(2.0), // count-in inside a gap
        Music(23.0, None),
        Fade(6.0),
        Quiet(8.0), // run-out, flip, needle drop, lead-in
        Drop(0.8),
        Quiet(3.0),
        Ticks(3.0), // count-in at the start of a side
        Music(32.0, None),
        Quiet(1.5),
        Music(30.0, None),
        Quiet(4.0),
        Drop(0.5), // needle lift
        Quiet(1.5),
    ]);
    // Lengths as a label prints them: rounded, a few seconds off, gaps included.
    let release = ReleaseInfo {
        tracks: vec![
            track("A1", Some(35.0)),
            track("A2", Some(40.0)),
            track("A3", Some(26.0)),
            track("B1", Some(34.0)),
            track("B2", Some(31.0)),
        ],
        ..Default::default()
    };
    let env = Envelope::from_frames(&rip.frames, SR);
    let p = place_tracks(&release, &env, rip.frames.len() as u64, "rip.wav");
    let t = &p.sheet.tracks;
    let pre = (0.3 * SR as f64) as u64;

    for (i, &on) in rip.onsets.iter().enumerate() {
        assert_near(t[i].start, on - pre, &format!("start of track {}", i + 1));
    }
    let linked: Vec<bool> = t.iter().map(|t| t.linked).collect();
    assert_eq!(linked, [false, true, true, false, true]);
    // The last track of each side ends 1.5 s after its music, a fade included.
    let fade_end = rip.onsets[2] + 31 * SR as u64;
    assert!(t[2].end > fade_end - SR as u64 && t[2].end < fade_end + 2 * SR as u64, "end of side A in the fade-out");
    assert_near(t[4].end, rip.onsets[4] + 30 * SR as u64 + 3 * SR as u64 / 2, "end of side B");
    let snaps: Vec<Snap> = p.notes.iter().map(|n| n.snap).collect();
    assert_eq!(snaps, [Snap::SideStart, Snap::Gap, Snap::Gap, Snap::SideStart, Snap::Gap]);
    assert!(p.sheet.check().is_empty(), "{:?}", p.sheet.check());
}

#[test]
fn test_missing_lengths_and_no_sides() {
    use Part::*;
    let rip = render(&[Quiet(3.0), Music(20.0, None), Quiet(2.0), Music(20.0, None), Quiet(2.0), Music(20.0, None), Quiet(3.0)]);
    let release = ReleaseInfo {
        tracks: vec![track("1", None), track("2", None), track("3", None)],
        ..Default::default()
    };
    let env = Envelope::from_frames(&rip.frames, SR);
    let p = place_tracks(&release, &env, rip.frames.len() as u64, "rip.wav");
    let pre = (0.3 * SR as f64) as u64;
    for (i, &on) in rip.onsets.iter().enumerate() {
        assert_near(p.sheet.tracks[i].start, on - pre, &format!("start of track {}", i + 1));
    }
}

#[test]
fn test_segue_falls_back_to_lengths() {
    use Part::*;
    // Two tracks run into each other with no quiet between them.
    let rip = render(&[Quiet(3.0), Music(50.0, None), Quiet(3.0)]);
    let release = ReleaseInfo { tracks: vec![track("A1", Some(20.0)), track("A2", Some(30.0))], ..Default::default() };
    let env = Envelope::from_frames(&rip.frames, SR);
    let p = place_tracks(&release, &env, rip.frames.len() as u64, "rip.wav");
    assert_eq!(p.notes[1].snap, Snap::Lengths);
    assert_near(p.sheet.tracks[1].start, rip.onsets[0] + 20 * SR as u64, "segue boundary");
    assert!(p.sheet.tracks[1].linked);
}
