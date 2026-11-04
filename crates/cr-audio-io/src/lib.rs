//! High-Fidelity Audio Streaming & Fixed-Point Conversion
//!
//! Sample conversion to the internal domain: docs/engine/OFFLINE_ENGINE.md, section 1.

use std::fs::File;
use std::path::Path;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use thiserror::Error;

use cr_dsp::params::{
    internal_to_i16, internal_to_i24_clamped, internal_to_i32, scale_f64_to_internal,
    scale_i16_to_internal, scale_i24_to_internal, scale_i32_to_internal,
};

#[derive(Error, Debug)]
pub enum AudioIoError {
    #[error("Hound WAV error: {0}")]
    Hound(#[from] hound::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Symphonia error: {0}")]
    Symphonia(String),
    #[error("Unsupported audio format or channel count: {0}")]
    Unsupported(String),
}

/// Metadata describing an opened audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMetadata {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub is_float: bool,
    pub total_frames: usize,
}

pub enum AudioBackend {
    Hound(WavReader<std::io::BufReader<File>>),
    Symphonia(SymphoniaDecoder),
}

pub struct SymphoniaDecoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    sample_buf: Vec<(i32, i32)>,
    sample_ptr: usize,
}

/// Streaming reader that yields stereo frames scaled to internal 25-bit signed integers.
pub struct AudioFileReader {
    backend: AudioBackend,
    pub meta: AudioMetadata,
    pub frames_read: usize,
}

impl AudioFileReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, AudioIoError> {
        let p = path.as_ref();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

        // 1. If WAV, prefer fast Hound reader
        if ext == "wav" {
            if let Ok(reader) = WavReader::open(p) {
                let spec = reader.spec();
                let is_float = spec.sample_format == SampleFormat::Float;
                let channels = spec.channels;
                if channels == 1 || channels == 2 {
                    // In hound, reader.duration() is already samples per channel (i.e. frames)
                    let total_frames = reader.duration() as usize;
                    let meta = AudioMetadata {
                        channels,
                        sample_rate: spec.sample_rate,
                        bits_per_sample: spec.bits_per_sample,
                        is_float,
                        total_frames,
                    };
                    return Ok(Self {
                        backend: AudioBackend::Hound(reader),
                        meta,
                        frames_read: 0,
                    });
                }
            }
        }

        // 2. Symphonia fallback for FLAC, AIFF, and non-standard WAV files
        Self::open_symphonia(p)
    }

    fn open_symphonia(path: &Path) -> Result<Self, AudioIoError> {
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;

        let src = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(src), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| AudioIoError::Symphonia(e.to_string()))?;

        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| AudioIoError::Unsupported("No audio tracks found in file".to_string()))?;

        let track_id = track.id;
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track.codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);
        let bits_per_sample = track.codec_params.bits_per_sample.unwrap_or(16) as u16;
        let total_frames = track.codec_params.n_frames.unwrap_or(0) as usize;

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioIoError::Symphonia(e.to_string()))?;

        let meta = AudioMetadata {
            channels,
            sample_rate,
            bits_per_sample,
            is_float: false,
            total_frames,
        };

        let sym_dec = SymphoniaDecoder {
            format,
            decoder,
            track_id,
            sample_buf: Vec::new(),
            sample_ptr: 0,
        };

        Ok(Self {
            backend: AudioBackend::Symphonia(sym_dec),
            meta,
            frames_read: 0,
        })
    }

    /// Reads up to max_frames into chunk buffer, returning the number of frames read.
    pub fn read_chunk(&mut self, max_frames: usize, chunk: &mut Vec<(i32, i32)>) -> Result<usize, AudioIoError> {
        chunk.clear();
        chunk.reserve(max_frames);

        match &mut self.backend {
            AudioBackend::Hound(reader) => {
                let spec = reader.spec();
                let is_mono = self.meta.channels == 1;

                match (spec.sample_format, spec.bits_per_sample) {
                    (SampleFormat::Float, 32) => {
                        let mut samples = reader.samples::<f32>();
                        if is_mono {
                            while chunk.len() < max_frames {
                                if let Some(s) = samples.next() {
                                    let v = scale_f64_to_internal(s? as f64);
                                    chunk.push((v, v));
                                } else {
                                    break;
                                }
                            }
                        } else {
                            while chunk.len() < max_frames {
                                if let (Some(l), Some(r)) = (samples.next(), samples.next()) {
                                    let lv = scale_f64_to_internal(l? as f64);
                                    let rv = scale_f64_to_internal(r? as f64);
                                    chunk.push((lv, rv));
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    (SampleFormat::Int, 16) => {
                        let mut samples = reader.samples::<i16>();
                        if is_mono {
                            while chunk.len() < max_frames {
                                if let Some(s) = samples.next() {
                                    let v = scale_i16_to_internal(s?);
                                    chunk.push((v, v));
                                } else {
                                    break;
                                }
                            }
                        } else {
                            while chunk.len() < max_frames {
                                if let (Some(l), Some(r)) = (samples.next(), samples.next()) {
                                    let lv = scale_i16_to_internal(l?);
                                    let rv = scale_i16_to_internal(r?);
                                    chunk.push((lv, rv));
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    (SampleFormat::Int, 24) => {
                        let mut samples = reader.samples::<i32>();
                        if is_mono {
                            while chunk.len() < max_frames {
                                if let Some(s) = samples.next() {
                                    let v = scale_i24_to_internal(s?);
                                    chunk.push((v, v));
                                } else {
                                    break;
                                }
                            }
                        } else {
                            while chunk.len() < max_frames {
                                if let (Some(l), Some(r)) = (samples.next(), samples.next()) {
                                    let lv = scale_i24_to_internal(l?);
                                    let rv = scale_i24_to_internal(r?);
                                    chunk.push((lv, rv));
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    (SampleFormat::Int, 32) => {
                        let mut samples = reader.samples::<i32>();
                        if is_mono {
                            while chunk.len() < max_frames {
                                if let Some(s) = samples.next() {
                                    let v = scale_i32_to_internal(s?);
                                    chunk.push((v, v));
                                } else {
                                    break;
                                }
                            }
                        } else {
                            while chunk.len() < max_frames {
                                if let (Some(l), Some(r)) = (samples.next(), samples.next()) {
                                    let lv = scale_i32_to_internal(l?);
                                    let rv = scale_i32_to_internal(r?);
                                    chunk.push((lv, rv));
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(AudioIoError::Unsupported(format!(
                            "Unsupported WAV format: {:?}, {} bits",
                            spec.sample_format, spec.bits_per_sample
                        )));
                    }
                }
            }
            AudioBackend::Symphonia(dec) => {
                while chunk.len() < max_frames {
                    if dec.sample_ptr < dec.sample_buf.len() {
                        chunk.push(dec.sample_buf[dec.sample_ptr]);
                        dec.sample_ptr += 1;
                    } else {
                        // Decode next packet
                        dec.sample_buf.clear();
                        dec.sample_ptr = 0;
                        match dec.format.next_packet() {
                            Ok(packet) => {
                                if packet.track_id() != dec.track_id {
                                    continue;
                                }
                                match dec.decoder.decode(&packet) {
                                    Ok(decoded) => {
                                        use symphonia::core::audio::AudioBufferRef;
                                        use symphonia::core::conv::IntoSample;
                                        // Symphonia left-justifies integer samples in their container
                                        // (24-bit FLAC arrives as S32 << 8), so each variant scales
                                        // from its container width. Every variant is handled: an
                                        // unmatched one used to drop its audio silently.
                                        let out = &mut dec.sample_buf;
                                        match decoded {
                                            AudioBufferRef::U8(buf) => push_stereo(&buf, out, |s| scale_i32_to_internal(s.into_sample())),
                                            AudioBufferRef::U16(buf) => push_stereo(&buf, out, |s| scale_i32_to_internal(s.into_sample())),
                                            AudioBufferRef::U24(buf) => push_stereo(&buf, out, |s| scale_i32_to_internal(s.into_sample())),
                                            AudioBufferRef::U32(buf) => push_stereo(&buf, out, |s| scale_i32_to_internal(s.into_sample())),
                                            AudioBufferRef::S8(buf) => push_stereo(&buf, out, |s| scale_i32_to_internal(s.into_sample())),
                                            AudioBufferRef::S16(buf) => push_stereo(&buf, out, scale_i16_to_internal),
                                            AudioBufferRef::S24(buf) => push_stereo(&buf, out, |s| scale_i24_to_internal(s.inner())),
                                            AudioBufferRef::S32(buf) => push_stereo(&buf, out, scale_i32_to_internal),
                                            AudioBufferRef::F32(buf) => push_stereo(&buf, out, |s| scale_f64_to_internal(s as f64)),
                                            AudioBufferRef::F64(buf) => push_stereo(&buf, out, scale_f64_to_internal),
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }

        let count = chunk.len();
        self.frames_read += count;
        Ok(count)
    }

    /// Reads all stereo frames into memory as internal 25-bit integers (left, right).
    pub fn read_all_frames(&mut self) -> Result<Vec<(i32, i32)>, AudioIoError> {
        let mut frames = Vec::new();
        let mut chunk = Vec::with_capacity(65536);
        while self.read_chunk(65536, &mut chunk)? > 0 {
            frames.extend_from_slice(&chunk);
        }
        Ok(frames)
    }
}

/// Appends a decoded Symphonia buffer as internal stereo frames. Mono is duplicated to both
/// channels; channels beyond the first two are ignored.
fn push_stereo<S: symphonia::core::sample::Sample>(
    buf: &symphonia::core::audio::AudioBuffer<S>,
    out: &mut Vec<(i32, i32)>,
    to_internal: impl Fn(S) -> i32,
) {
    let planes = buf.planes();
    let planes = planes.planes();
    let ch0 = planes[0];
    let ch1 = if planes.len() > 1 { planes[1] } else { ch0 };
    out.extend(ch0.iter().zip(ch1).map(|(&l, &r)| (to_internal(l), to_internal(r))));
}

/// Streaming writer that converts internal 25-bit signed integer frames back to target WAV PCM format.
pub struct AudioFileWriter {
    writer: WavWriter<std::io::BufWriter<File>>,
    pub meta: AudioMetadata,
}

impl AudioFileWriter {
    pub fn create<P: AsRef<Path>>(path: P, meta: AudioMetadata) -> Result<Self, AudioIoError> {
        let sample_format = if meta.is_float {
            SampleFormat::Float
        } else {
            SampleFormat::Int
        };
        let spec = WavSpec {
            channels: meta.channels,
            sample_rate: meta.sample_rate,
            bits_per_sample: meta.bits_per_sample,
            sample_format,
        };
        let writer = WavWriter::create(path, spec)?;
        Ok(Self { writer, meta })
    }

    /// Writes one restored stereo frame to the output WAV file.
    pub fn write_frame(&mut self, left_internal: i32, right_internal: i32) -> Result<(), AudioIoError> {
        match (self.meta.is_float, self.meta.bits_per_sample) {
            (true, 32) => {
                let l = (left_internal as f32) / 16_777_216.0;
                let r = (right_internal as f32) / 16_777_216.0;
                self.writer.write_sample(l)?;
                if self.meta.channels == 2 {
                    self.writer.write_sample(r)?;
                }
            }
            (false, 16) => {
                let l = internal_to_i16(left_internal);
                let r = internal_to_i16(right_internal);
                self.writer.write_sample(l)?;
                if self.meta.channels == 2 {
                    self.writer.write_sample(r)?;
                }
            }
            (false, 24) => {
                let l = internal_to_i24_clamped(left_internal);
                let r = internal_to_i24_clamped(right_internal);
                self.writer.write_sample(l)?;
                if self.meta.channels == 2 {
                    self.writer.write_sample(r)?;
                }
            }
            (false, 32) => {
                let l = internal_to_i32(left_internal);
                let r = internal_to_i32(right_internal);
                self.writer.write_sample(l)?;
                if self.meta.channels == 2 {
                    self.writer.write_sample(r)?;
                }
            }
            _ => {
                return Err(AudioIoError::Unsupported(format!(
                    "Unsupported output bits per sample: {}",
                    self.meta.bits_per_sample
                )));
            }
        }
        Ok(())
    }

    /// Writes a chunk of stereo frames to the output WAV file.
    pub fn write_chunk(&mut self, frames: &[(i32, i32)]) -> Result<(), AudioIoError> {
        for &(l, r) in frames {
            self.write_frame(l, r)?;
        }
        Ok(())
    }

    /// Flushes and finalizes the WAV header.
    pub fn finalize(self) -> Result<(), AudioIoError> {
        self.writer.finalize()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wav_write_and_read_roundtrip_16bit() {
        let temp_dir = std::env::temp_dir();
        let test_path = temp_dir.join("test_cr_roundtrip_16.wav");

        let meta = AudioMetadata {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            is_float: false,
            total_frames: 100,
        };

        let mut writer = AudioFileWriter::create(&test_path, meta).unwrap();
        let mut written_frames = Vec::new();
        for i in 0..100 {
            let s1 = scale_i16_to_internal((i * 200) as i16);
            let s2 = scale_i16_to_internal((-i * 200) as i16);
            writer.write_frame(s1, s2).unwrap();
            written_frames.push((s1, s2));
        }
        writer.finalize().unwrap();

        let mut reader = AudioFileReader::open(&test_path).unwrap();
        assert_eq!(reader.meta.sample_rate, 44100);
        assert_eq!(reader.meta.channels, 2);
        assert_eq!(reader.meta.total_frames, 100);
        let read_frames = reader.read_all_frames().unwrap();
        assert_eq!(read_frames.len(), 100);

        for i in 0..100 {
            assert!((read_frames[i].0 - written_frames[i].0).abs() <= 256);
            assert!((read_frames[i].1 - written_frames[i].1).abs() <= 256);
        }

        let _ = std::fs::remove_file(test_path);
    }
}
