//! The FLAC encoder: streams decode (with Symphonia, as CleanGroove reads FLAC) to exactly the
//! samples given, across the cases that pick different codings.

use cr_tracks::flac::{encode, BLOCK};

/// Encodes, decodes, and returns the decoded samples (interleaved, at `bits`) and the size.
fn round_trip(samples: &[i32], channels: usize, bits: u32) -> (Vec<i32>, usize) {
    let bytes = encode(samples, channels, bits, 44_100);
    let path = std::env::temp_dir().join(format!("cr-flac-{}-{}-{}.flac", std::process::id(), samples.len(), bits));
    std::fs::write(&path, &bytes).unwrap();
    let mut r = cr_audio_io::AudioFileReader::open(&path).unwrap();
    assert_eq!((r.meta.channels as usize, r.meta.bits_per_sample as u32, r.meta.sample_rate), (channels, bits, 44_100));
    let frames = r.read_all_frames().unwrap();
    std::fs::remove_file(&path).unwrap();
    // The reader gives the internal domain: 24-bit << 1, 16-bit << 9.
    let shift = if bits == 24 { 1 } else { 9 };
    let out = frames.iter().flat_map(|&(l, r)| if channels == 2 { vec![l >> shift, r >> shift] } else { vec![l >> shift] }).collect();
    (out, bytes.len())
}

fn noise(seed: &mut u32, amp: i32) -> i32 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    ((*seed >> 8) as i64 * 2 * amp as i64 / (1 << 24) - amp as i64) as i32
}

#[test]
fn test_music_like_stereo_24_and_16_bit() {
    let mut seed = 1;
    for bits in [24u32, 16] {
        let amp = if bits == 24 { 3_000_000.0 } else { 12_000.0 };
        let n = BLOCK * 5 + 123; // a short last block
        let s: Vec<i32> = (0..n)
            .flat_map(|i| {
                let t = i as f64 / 44_100.0;
                let l = amp * (t * 440.0 * std::f64::consts::TAU).sin() + noise(&mut seed, (amp / 300.0) as i32) as f64;
                let r = amp * 0.7 * (t * 660.0 * std::f64::consts::TAU).sin() + 0.3 * l;
                [l as i32, r as i32]
            })
            .collect();
        let (back, size) = round_trip(&s, 2, bits);
        assert_eq!(back, s, "{bits} bit");
        // Compresses: the noise added is about 14 bits at 24 bit, 6 at 16.
        assert!(size * 10 < s.len() * bits as usize / 8 * 7, "{bits} bit: {size} bytes");
    }
}

#[test]
fn test_noise_mono_silence_and_extremes() {
    let mut seed = 9;
    // Full-scale noise (verbatim or close), mono.
    let s: Vec<i32> = (0..BLOCK + 7).map(|_| noise(&mut seed, 8_388_607)).collect();
    assert_eq!(round_trip(&s, 1, 24).0, s);
    // Digital silence and a constant (constant subframes).
    let s = vec![0i32; BLOCK * 2];
    let (back, size) = round_trip(&s, 2, 24);
    assert_eq!(back, s);
    assert!(size < 200, "{size}");
    let s: Vec<i32> = (0..BLOCK * 2).flat_map(|_| [-8_388_608, 8_388_607]).collect();
    // Opposite full-scale channels: the side channel needs 25 bits.
    assert_eq!(round_trip(&s, 2, 24).0, s);
    let s: Vec<i32> = (0..BLOCK).flat_map(|i| if i % 2 == 0 { [32_767, -32_768] } else { [-32_768, 32_767] }).collect();
    assert_eq!(round_trip(&s, 2, 16).0, s);
}

#[test]
fn test_tiny_streams() {
    for n in [1usize, 2, 3, 5, 17, BLOCK - 1, BLOCK + 1] {
        let s: Vec<i32> = (0..n as i32).flat_map(|i| [i * 1000 - 7, -i * 3]).collect();
        assert_eq!(round_trip(&s, 2, 24).0, s, "{n} frames");
    }
}
