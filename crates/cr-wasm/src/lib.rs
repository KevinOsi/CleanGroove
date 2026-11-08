//! CleanGroove's offline engine for the browser.
//!
//! A plain C-style interface, so the page needs no generated glue code: the page decodes the
//! file (the browser does WAV, FLAC and more), asks for buffers in this module's memory, copies
//! the two channels in, then runs the engine chunk by chunk so it can show progress.
//!
//! Samples cross the boundary as f32 in [-1, 1] and are converted with the `cr_dsp::params`
//! helpers. The engine itself is the same `Processor` the desktop programs use; nothing here
//! changes what it does.

use cr_dsp::params::{internal_to_f64, scale_f64_to_internal, CrParameters, Engine, Method};
use cr_dsp::process::Processor;

/// One file being restored: the engine, the whole input and output, and where the output is.
pub struct Job {
    processor: Processor,
    total: usize,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    written: usize,
    chunk: Vec<(i32, i32)>,
    out_chunk: Vec<(i32, i32)>,
}

impl Job {
    pub fn new(params: CrParameters, sample_rate: u32, total: usize) -> Self {
        Self {
            processor: Processor::new(params, sample_rate, Some(total)),
            total,
            in_l: vec![0.0; total],
            in_r: vec![0.0; total],
            out_l: vec![0.0; total],
            out_r: vec![0.0; total],
            written: 0,
            chunk: Vec::with_capacity(65_536),
            out_chunk: Vec::with_capacity(65_536),
        }
    }

    fn push_out(&mut self, frames: &[(i32, i32)]) {
        for &(l, r) in frames {
            if self.written >= self.total {
                break;
            }
            self.out_l[self.written] = internal_to_f64(l) as f32;
            self.out_r[self.written] = internal_to_f64(r) as f32;
            self.written += 1;
        }
    }

    /// Runs input frames `start..start + count` through the engine.
    pub fn process(&mut self, start: usize, count: usize) {
        let end = (start + count).min(self.total);
        self.chunk.clear();
        for i in start..end {
            self.chunk.push((
                scale_f64_to_internal(self.in_l[i] as f64),
                scale_f64_to_internal(self.in_r[i] as f64),
            ));
        }
        let mut out = std::mem::take(&mut self.out_chunk);
        self.processor.process_chunk(&self.chunk, &mut out);
        self.push_out(&out);
        self.out_chunk = out;
    }

    /// Drains the engine's delay; the output then has exactly as many frames as the input.
    pub fn finish(&mut self) -> usize {
        let rest = self.processor.flush();
        self.push_out(&rest);
        // Any frames the engine did not return stay silent, as in the command-line tool.
        self.written = self.total;
        self.total
    }

    pub fn output(&self) -> (&[f32], &[f32]) {
        (&self.out_l, &self.out_r)
    }

    pub fn input_mut(&mut self) -> (&mut [f32], &mut [f32]) {
        (&mut self.in_l, &mut self.in_r)
    }
}

fn params(preset: u32, dcl: i32, dcr: i32, method: u32, pitch: u32, reverse: u32, engine: u32) -> CrParameters {
    let mut p = match preset {
        1 => CrParameters::preset_mono_lp(),
        2 => CrParameters::preset_78_rpm(),
        _ => CrParameters::preset_stereo_lp(),
    };
    p.dcl = dcl.clamp(0, 100);
    p.dcr = dcr.clamp(0, 100);
    p.method = match method {
        0 => Method::Simple,
        2 => Method::WaveletX2,
        _ => Method::Wavelet,
    };
    p.detect_pitch = pitch != 0;
    p.reverse = reverse != 0;
    p.engine = if engine == 1 { Engine::Enhanced } else { Engine::Classic };
    p
}

/// Creates a job. preset: 0 stereo LP, 1 mono LP, 2 78 rpm. method: 0 Simple, 1 Wavelet,
/// 2 Wavelet x2. engine: 0 Classic, 1 Enhanced.
#[no_mangle]
pub extern "C" fn cg_job_new(
    sample_rate: u32,
    total: u32,
    preset: u32,
    dcl: i32,
    dcr: i32,
    method: u32,
    pitch: u32,
    reverse: u32,
    engine: u32,
) -> *mut Job {
    let p = params(preset, dcl, dcr, method, pitch, reverse, engine);
    Box::into_raw(Box::new(Job::new(p, sample_rate, total as usize)))
}

/// # Safety
/// `job` must come from `cg_job_new` and not be freed.
#[no_mangle]
pub unsafe extern "C" fn cg_job_input_l(job: *mut Job) -> *mut f32 {
    (*job).input_mut().0.as_mut_ptr()
}

/// # Safety
/// As `cg_job_input_l`.
#[no_mangle]
pub unsafe extern "C" fn cg_job_input_r(job: *mut Job) -> *mut f32 {
    (*job).input_mut().1.as_mut_ptr()
}

/// # Safety
/// As `cg_job_input_l`.
#[no_mangle]
pub unsafe extern "C" fn cg_job_output_l(job: *mut Job) -> *const f32 {
    (*job).output().0.as_ptr()
}

/// # Safety
/// As `cg_job_input_l`.
#[no_mangle]
pub unsafe extern "C" fn cg_job_output_r(job: *mut Job) -> *const f32 {
    (*job).output().1.as_ptr()
}

/// # Safety
/// As `cg_job_input_l`.
#[no_mangle]
pub unsafe extern "C" fn cg_job_process(job: *mut Job, start: u32, count: u32) {
    (*job).process(start as usize, count as usize);
}

/// # Safety
/// As `cg_job_input_l`.
#[no_mangle]
pub unsafe extern "C" fn cg_job_finish(job: *mut Job) -> u32 {
    (*job).finish() as u32
}

/// # Safety
/// As `cg_job_input_l`; the job must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn cg_job_free(job: *mut Job) {
    drop(Box::from_raw(job));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper returns what the engine returns, frame for frame, and keeps the length.
    #[test]
    fn job_matches_the_engine() {
        let sr = 44_100;
        let n = 100_000;
        let mut l = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let mut a = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin();
            if i % 7_919 == 100 {
                a += 0.5; // a click
            }
            l.push(a as f32);
            r.push((a * 0.8) as f32);
        }
        let p = params(0, 40, 0, 1, 1, 0, 0);

        let mut job = Job::new(p.clone(), sr, n);
        job.input_mut().0.copy_from_slice(&l);
        job.input_mut().1.copy_from_slice(&r);
        let mut s = 0;
        while s < n {
            job.process(s, 30_000);
            s += 30_000;
        }
        assert_eq!(job.finish(), n);

        let frames: Vec<(i32, i32)> = l
            .iter()
            .zip(&r)
            .map(|(&a, &b)| (scale_f64_to_internal(a as f64), scale_f64_to_internal(b as f64)))
            .collect();
        let (want, _) = Processor::process_slice_aligned(p, sr, &frames);
        let (ol, or) = job.output();
        for i in 0..n.min(want.len()) {
            assert_eq!(ol[i], internal_to_f64(want[i].0) as f32, "left frame {i}");
            assert_eq!(or[i], internal_to_f64(want[i].1) as f32, "right frame {i}");
        }
        assert!(ol.iter().zip(&l).any(|(a, b)| a != b), "the click was repaired");
    }
}
