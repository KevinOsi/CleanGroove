//! Audit regression: the stdin/stdout pipe host must tolerate reads that are not a multiple
//! of the 6-byte stereo frame size (pipes deliver arbitrary byte counts).
use cr_dsp::params::{Method, RtParameters, RtSensitivity};
use cr_rt::backend::pipe_host::run_pipe_stream_io;
use std::io::Read;
/// Reader that hands out data in deliberately awkward sizes (not multiples of 6).
struct Ragged { data: Vec<u8>, pos: usize, sizes: Vec<usize>, k: usize }
impl Read for Ragged {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() { return Ok(0); }
        let want = self.sizes[self.k % self.sizes.len()].min(buf.len()).min(self.data.len() - self.pos);
        self.k += 1;
        buf[..want].copy_from_slice(&self.data[self.pos..self.pos + want]);
        self.pos += want; Ok(want)
    }
}
fn input_bytes(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n * 6);
    for i in 0..n { let l = ((i as f64 * 0.03).sin() * 3e6) as i32; let r = ((i as f64 * 0.017).cos() * 2e6) as i32;
        for s in [l, r] { v.push((s & 0xFF) as u8); v.push(((s >> 8) & 0xFF) as u8); v.push(((s >> 16) & 0xFF) as u8); } }
    v
}
#[test]
fn ragged_reads_match_whole_reads() {
    let mut p = RtParameters::default(); p.method = Method::Simple; p.sensitivity = RtSensitivity::Stepped(5);
    let data = input_bytes(30_000);
    let mut ref_out = Vec::new();
    run_pipe_stream_io(p.clone(), 44100, None, Ragged { data: data.clone(), pos: 0, sizes: vec![6144], k: 0 }, &mut ref_out).unwrap();
    let mut ragged_out = Vec::new();
    run_pipe_stream_io(p, 44100, None, Ragged { data: data.clone(), pos: 0, sizes: vec![4096, 7, 1, 5, 4093, 13, 2], k: 0 }, &mut ragged_out).unwrap();
    assert_eq!(ref_out.len(), data.len());
    assert_eq!(ragged_out.len(), data.len(), "frames were dropped on ragged reads");
    assert!(ref_out == ragged_out, "output differs when reads are not frame-aligned");
}
