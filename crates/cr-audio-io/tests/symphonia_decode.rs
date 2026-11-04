//! Regression tests for the Symphonia (FLAC/AIFF) decode path.
//!
//! Symphonia hands back S24 / S32 sample buffers for 24-bit sources. These used to be
//! silently dropped, so a 24-bit FLAC decoded to zero frames. The test files are built
//! by hand (verbatim FLAC subframes, plain AIFF) so no external encoder is needed.

use std::path::PathBuf;

use cr_audio_io::AudioFileReader;
use cr_dsp::params::scale_i24_to_internal;

const FRAMES: usize = 1000;
const BLOCK: usize = 256;

/// Deterministic stereo 24-bit test signal, including both full-scale extremes.
fn signal_24() -> Vec<(i32, i32)> {
    (0..FRAMES)
        .map(|i| {
            let l = match i {
                0 => 8_388_607,
                1 => -8_388_608,
                _ => ((i as i32 * 7919) % 16_000_000) - 8_000_000,
            };
            let r = -(l / 3) + (i as i32 & 0xFF);
            (l, r)
        })
        .collect()
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("cr_symphonia_{}_{}", std::process::id(), name))
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

fn push_i24_be(out: &mut Vec<u8>, s: i32) {
    out.extend_from_slice(&s.to_be_bytes()[1..]);
}

/// Minimal 24-bit stereo FLAC writer: STREAMINFO plus fixed-blocksize frames of
/// verbatim subframes. MD5 is left zero ("unknown") so decoders skip that check.
fn write_flac_24(path: &PathBuf, frames: &[(i32, i32)], sample_rate: u32) {
    let mut out = b"fLaC".to_vec();

    // STREAMINFO, last metadata block, 34 bytes.
    out.push(0x80);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    out.extend_from_slice(&(BLOCK as u16).to_be_bytes());
    out.extend_from_slice(&[0; 6]); // min/max frame size unknown
    // 20 bits rate | 3 bits (channels - 1) | 5 bits (bps - 1) | 36 bits total samples
    let packed: u64 = ((sample_rate as u64) << 44) | (1u64 << 41) | (23u64 << 36) | frames.len() as u64;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0; 16]);

    for (n, block) in frames.chunks(BLOCK).enumerate() {
        assert!(n < 128, "frame number must fit a one-byte UTF-8 code");
        let start = out.len();
        out.extend_from_slice(&[0xFF, 0xF8]); // sync, fixed blocksize
        out.push(0x60); // blocksize: 8-bit (n-1) at end of header; rate from STREAMINFO
        out.push(0x1C); // independent stereo, 24 bits per sample
        out.push(n as u8);
        out.push((block.len() - 1) as u8);
        let crc = crc8(&out[start..]);
        out.push(crc);
        for ch in 0..2 {
            out.push(0x02); // verbatim subframe, no wasted bits
            for &(l, r) in block {
                push_i24_be(&mut out, if ch == 0 { l } else { r });
            }
        }
        let crc = crc16(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    std::fs::write(path, out).unwrap();
}

/// 80-bit IEEE extended encoding of an integer sample rate, as AIFF COMM requires.
fn extended_80(rate: u32) -> [u8; 10] {
    let exp = 31 - rate.leading_zeros();
    let mantissa = (rate as u64) << (63 - exp);
    let mut out = [0u8; 10];
    out[..2].copy_from_slice(&((16383 + exp) as u16).to_be_bytes());
    out[2..].copy_from_slice(&mantissa.to_be_bytes());
    out
}

fn write_aiff_24(path: &PathBuf, frames: &[(i32, i32)], sample_rate: u32) {
    let mut comm = Vec::new();
    comm.extend_from_slice(&2u16.to_be_bytes());
    comm.extend_from_slice(&(frames.len() as u32).to_be_bytes());
    comm.extend_from_slice(&24u16.to_be_bytes());
    comm.extend_from_slice(&extended_80(sample_rate));

    let mut ssnd = vec![0u8; 8]; // offset, block size
    for &(l, r) in frames {
        push_i24_be(&mut ssnd, l);
        push_i24_be(&mut ssnd, r);
    }

    let mut body = b"AIFF".to_vec();
    body.extend_from_slice(b"COMM");
    body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
    body.extend_from_slice(&comm);
    body.extend_from_slice(b"SSND");
    body.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
    body.extend_from_slice(&ssnd);

    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    std::fs::write(path, out).unwrap();
}

fn assert_decodes_exactly(path: &PathBuf, expected: &[(i32, i32)]) {
    let mut reader = AudioFileReader::open(path).unwrap();
    assert_eq!(reader.meta.channels, 2);
    assert_eq!(reader.meta.sample_rate, 44100);
    assert_eq!(reader.meta.bits_per_sample, 24);
    let frames = reader.read_all_frames().unwrap();
    assert_eq!(frames.len(), expected.len(), "decoded frame count");
    for (i, (&got, &(l, r))) in frames.iter().zip(expected).enumerate() {
        let want = (scale_i24_to_internal(l), scale_i24_to_internal(r));
        assert_eq!(got, want, "frame {i}");
    }
}

#[test]
fn test_flac_24bit_decodes_bit_exact() {
    let path = temp_path("24.flac");
    let signal = signal_24();
    write_flac_24(&path, &signal, 44100);
    assert_decodes_exactly(&path, &signal);
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_aiff_24bit_decodes_bit_exact() {
    let path = temp_path("24.aiff");
    let signal = signal_24();
    write_aiff_24(&path, &signal, 44100);
    assert_decodes_exactly(&path, &signal);
    let _ = std::fs::remove_file(path);
}
