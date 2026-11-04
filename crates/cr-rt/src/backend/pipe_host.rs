//! Standard I/O Pipe Streaming Backend
//!
//! Streams raw PCM frames over stdin/stdout for headless pipes, DAW virtual cables, or batch scripts.

use std::io::{stdin, stdout, Read, Write};
use anyhow::Result;
use cr_dsp::params::{internal_to_i24_clamped, scale_i24_to_internal, RtParameters};
use cr_dsp::rt::RtProcessor;

use crate::tape::TapeRecorder;

pub fn run_pipe_stream(params: RtParameters, sample_rate: u32, tape: Option<TapeRecorder>) -> Result<()> {
    run_pipe_stream_io(params, sample_rate, tape, stdin().lock(), stdout().lock())
}

pub fn run_pipe_stream_io<R: Read, W: Write>(
    params: RtParameters,
    sample_rate: u32,
    mut tape: Option<TapeRecorder>,
    mut reader: R,
    mut writer: W,
) -> Result<()> {
    let mut processor = RtProcessor::new(params, sample_rate);

    // 24-bit stereo frame = 6 bytes (3 bytes left + 3 bytes right)
    let chunk_frames = 1024;
    let mut raw_in = vec![0u8; chunk_frames * 6];
    let mut in_frames = Vec::with_capacity(chunk_frames);
    let mut out_frames = vec![(0i32, 0i32); chunk_frames];
    let mut raw_out = vec![0u8; chunk_frames * 6];
    // Bytes of an incomplete trailing frame carried over from the previous read.
    // (Audit fix: `read()` on a pipe may return any byte count. Previously the remainder of
    // `n_bytes % 6` was silently discarded, permanently shifting the byte alignment of every
    // following frame (L/R swap / garbage), and a short read of < 6 bytes ended the stream.)
    let mut carry = 0usize;

    loop {
        let n_bytes = match reader.read(&mut raw_in[carry..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };

        let available = carry + n_bytes;
        let frames_read = available / 6;
        if frames_read == 0 {
            carry = available;
            continue;
        }

        in_frames.clear();
        for f in 0..frames_read {
            let offset = f * 6;
            // Left sample 24-bit signed
            let l_raw = (raw_in[offset] as i32)
                | ((raw_in[offset + 1] as i32) << 8)
                | ((raw_in[offset + 2] as i8 as i32) << 16);
            // Right sample 24-bit signed
            let r_raw = (raw_in[offset + 3] as i32)
                | ((raw_in[offset + 4] as i32) << 8)
                | ((raw_in[offset + 5] as i8 as i32) << 16);

            let l_int = scale_i24_to_internal(l_raw);
            let r_int = scale_i24_to_internal(r_raw);

            if let Some(ref mut tr) = tape {
                tr.push_frame(l_int, r_int);
            }

            in_frames.push((l_int, r_int));
        }

        processor.process_frames(&in_frames, &mut out_frames[..frames_read]);

        for f in 0..frames_read {
            let offset = f * 6;
            let (ol, or) = out_frames[f];
            let ol_24 = internal_to_i24_clamped(ol);
            let or_24 = internal_to_i24_clamped(or);

            raw_out[offset] = (ol_24 & 0xFF) as u8;
            raw_out[offset + 1] = ((ol_24 >> 8) & 0xFF) as u8;
            raw_out[offset + 2] = ((ol_24 >> 16) & 0xFF) as u8;

            raw_out[offset + 3] = (or_24 & 0xFF) as u8;
            raw_out[offset + 4] = ((or_24 >> 8) & 0xFF) as u8;
            raw_out[offset + 5] = ((or_24 >> 16) & 0xFF) as u8;
        }

        writer.write_all(&raw_out[..frames_read * 6])?;

        // Move the partial frame (if any) to the front for the next read.
        let consumed = frames_read * 6;
        carry = available - consumed;
        raw_in.copy_within(consumed..available, 0);
    }

    if let Some(tr) = tape {
        let _ = tr.stop();
    }

    Ok(())
}
