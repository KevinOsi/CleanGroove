//! A small FLAC encoder, written from the FLAC format specification (RFC 9639).
//!
//! Fixed-size blocks of 4096 samples. Each channel of a block is coded as a constant, with the
//! best of the fixed predictors (orders 0 to 4) and partitioned Rice coding of the residual,
//! or verbatim, whichever is smallest; stereo blocks also try left/side, side/right and
//! mid/side. Rice parameters above 14 use the 5-bit form (RICE2): 24-bit vinyl leaves
//! residuals that large, and an encoder without it barely compresses such audio. The
//! STREAMINFO block carries the MD5 of the audio, so `flac -t` can check the files.
//!
//! The output is a bare stream (STREAMINFO only); tags and pictures are added after.

use md5::{Digest, Md5};

/// Samples per block.
pub const BLOCK: usize = 4096;
/// Largest Rice parameter of the 4-bit form; the 5-bit form goes to 30 (31 is the escape).
const RICE_MAX: u32 = 14;
const RICE2_MAX: u32 = 30;
/// Finest residual partitioning tried: 2^8 partitions.
const MAX_PARTITION_ORDER: u32 = 8;

/// Encodes interleaved samples (`channels` 1 or 2, `bits` 16 or 24, values within that many
/// bits) as a FLAC stream.
pub fn encode(samples: &[i32], channels: usize, bits: u32, sample_rate: u32) -> Vec<u8> {
    assert!((1..=2).contains(&channels) && (bits == 16 || bits == 24));
    let frames = samples.len() / channels;
    let blocks: Vec<(usize, usize)> = (0..frames).step_by(BLOCK).map(|s| (s, (s + BLOCK).min(frames))).collect();

    // Blocks are independent: encode them on all cores, in order.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
    let per = blocks.len().div_ceil(threads).max(1);
    let encoded: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let handles: Vec<_> = blocks
            .chunks(per)
            .enumerate()
            .map(|(c, chunk)| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(k, &(s, e))| encode_frame(&samples[s * channels..e * channels], channels, bits, (c * per + k) as u64))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });

    let mut md5 = Md5::new();
    let bytes = (bits / 8) as usize;
    let mut buf = Vec::with_capacity(samples.len() * bytes);
    for &s in samples {
        buf.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    md5.update(&buf);

    let (min_frame, max_frame) = encoded.iter().fold((u32::MAX, 0u32), |(lo, hi), f| (lo.min(f.len() as u32), hi.max(f.len() as u32)));
    let mut w = BitWriter::default();
    w.put(16, BLOCK as u64);
    w.put(16, BLOCK as u64);
    w.put(24, if encoded.is_empty() { 0 } else { min_frame as u64 });
    w.put(24, max_frame as u64);
    w.put(20, sample_rate as u64);
    w.put(3, channels as u64 - 1);
    w.put(5, bits as u64 - 1);
    w.put(36, frames as u64);
    let info = w.finish();

    let mut out = Vec::with_capacity(8 + 34 + 16 + encoded.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&[0x80, 0, 0, 34]); // last metadata block, STREAMINFO, 34 bytes
    out.extend_from_slice(&info);
    out.extend_from_slice(&md5.finalize());
    for f in encoded {
        out.extend_from_slice(&f);
    }
    out
}

#[derive(Default)]
struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    /// Writes the low `bits` (at most 32) of `v`.
    fn put(&mut self, bits: u32, v: u64) {
        if bits > 32 {
            self.put(bits - 32, v >> 32);
            self.put(32, v & 0xFFFF_FFFF);
            return;
        }
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (v & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1u64 << self.n) - 1;
    }

    fn put_signed(&mut self, bits: u32, v: i64) {
        self.put(bits, v as u64);
    }

    /// `q` zeros, then a one.
    fn unary(&mut self, mut q: u64) {
        while q >= 32 {
            self.put(32, 0);
            q -= 32;
        }
        self.put(q as u32 + 1, 1);
    }

    /// Pads to a byte boundary with zeros and returns the bytes.
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.put(8 - self.n, 0);
        }
        self.buf
    }
}

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut c, &b| {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
        c
    })
}

fn crc16_table() -> &'static [u16; 256] {
    static TABLE: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = (i as u16) << 8;
            for _ in 0..8 {
                c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            }
            *e = c;
        }
        t
    })
}

fn crc16(data: &[u8]) -> u16 {
    let t = crc16_table();
    data.iter().fold(0u16, |c, &b| (c << 8) ^ t[((c >> 8) as u8 ^ b) as usize])
}

/// The frame number in FLAC's UTF-8-like coding.
fn put_coded_number(w: &mut BitWriter, n: u64) {
    if n < 0x80 {
        w.put(8, n);
        return;
    }
    let len = match n {
        _ if n < 0x800 => 2,
        _ if n < 0x1_0000 => 3,
        _ if n < 0x20_0000 => 4,
        _ if n < 0x400_0000 => 5,
        _ => 6,
    };
    let lead = (0xFF00u64 >> len) & 0xFF;
    w.put(8, lead | (n >> (6 * (len - 1))));
    for k in (0..len - 1).rev() {
        w.put(8, 0x80 | ((n >> (6 * k)) & 0x3F));
    }
}

/// How one channel of a block is coded.
enum Sub {
    Constant(i64),
    Verbatim,
    Fixed { order: usize, residual: Vec<i64>, partition_order: u32, params: Vec<u32> },
}

fn encode_frame(block: &[i32], channels: usize, bits: u32, number: u64) -> Vec<u8> {
    let n = block.len() / channels;
    let chan = |c: usize| -> Vec<i64> { (0..n).map(|i| block[i * channels + c] as i64).collect() };

    // (channel assignment code, [(samples, bits)]) of the cheapest coding.
    let (assignment, subs): (u64, Vec<(Sub, u32)>) = if channels == 1 {
        (0, vec![(plan(&chan(0), bits).1, bits)])
    } else {
        let (l, r) = (chan(0), chan(1));
        let side: Vec<i64> = l.iter().zip(&r).map(|(a, b)| a - b).collect();
        let mid: Vec<i64> = l.iter().zip(&r).map(|(a, b)| (a + b) >> 1).collect();
        let (pl, pr, ps, pm) = (plan(&l, bits), plan(&r, bits), plan(&side, bits + 1), plan(&mid, bits));
        let costs = [pl.0 + pr.0, pl.0 + ps.0, ps.0 + pr.0, pm.0 + ps.0];
        let best = (0..4).min_by_key(|&i| costs[i]).unwrap();
        match best {
            0 => (1, vec![(pl.1, bits), (pr.1, bits)]),
            1 => (8, vec![(pl.1, bits), (ps.1, bits + 1)]),
            2 => (9, vec![(ps.1, bits + 1), (pr.1, bits)]),
            _ => (10, vec![(pm.1, bits), (ps.1, bits + 1)]),
        }
    };

    let mut w = BitWriter::default();
    w.put(16, 0xFFF8); // sync, fixed block size
    let size_code = if n == BLOCK { 0b1100 } else { 0b0111 };
    w.put(4, size_code);
    w.put(4, 0); // sample rate: from STREAMINFO
    w.put(4, assignment);
    w.put(3, if bits == 16 { 0b100 } else { 0b110 });
    w.put(1, 0);
    put_coded_number(&mut w, number);
    if size_code == 0b0111 {
        w.put(16, n as u64 - 1);
    }
    // The header is byte aligned here; its CRC-8 follows.
    let crc = crc8(&w.buf);
    w.put(8, crc as u64);

    for (k, (sub, bps)) in subs.into_iter().enumerate() {
        let samples = match assignment {
            0 | 1 => chan(k),
            8 if k == 1 => chan(0).iter().zip(chan(1)).map(|(a, b)| a - b).collect(),
            8 => chan(0),
            9 if k == 0 => chan(0).iter().zip(chan(1)).map(|(a, b)| a - b).collect(),
            9 => chan(1),
            _ if k == 0 => chan(0).iter().zip(chan(1)).map(|(a, b)| (a + b) >> 1).collect(),
            _ => chan(0).iter().zip(chan(1)).map(|(a, b)| a - b).collect(),
        };
        write_sub(&mut w, &sub, &samples, bps);
    }
    let mut frame = w.finish();
    let crc = crc16(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

/// The residual of fixed predictor `order`, or None when it does not fit 32 bits.
fn fixed_residual(x: &[i64], order: usize) -> Option<Vec<i64>> {
    let r: Vec<i64> = (order..x.len())
        .map(|i| match order {
            0 => x[i],
            1 => x[i] - x[i - 1],
            2 => x[i] - 2 * x[i - 1] + x[i - 2],
            3 => x[i] - 3 * x[i - 1] + 3 * x[i - 2] - x[i - 3],
            _ => x[i] - 4 * x[i - 1] + 6 * x[i - 2] - 4 * x[i - 3] + x[i - 4],
        })
        .collect();
    r.iter().all(|&v| v >= i32::MIN as i64 && v <= i32::MAX as i64).then_some(r)
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Estimated bits and the best Rice parameter for values summing to `sum` over `count`.
fn rice_cost(sum: u64, count: u64, max_k: u32) -> (u64, u32) {
    if count == 0 {
        return (0, 0);
    }
    let mean = sum / count;
    let k0 = if mean == 0 { 0 } else { (63 - mean.leading_zeros()).min(max_k) };
    (k0.saturating_sub(1)..=(k0 + 1).min(max_k))
        .map(|k| (count * (k as u64 + 1) + (sum >> k), k))
        .min()
        .unwrap()
}

/// Cheapest partitioning of a residual for predictor `order` over a block of `n`: (bits,
/// partition order, parameters).
fn plan_residual(residual: &[i64], order: usize, n: usize) -> (u64, u32, Vec<u32>) {
    let u: Vec<u64> = residual.iter().map(|&r| zigzag(r)).collect();
    let mut max_p = 0;
    while max_p < MAX_PARTITION_ORDER && n % (1 << (max_p + 1)) == 0 && (n >> (max_p + 1)) > order {
        max_p += 1;
    }
    // Sums and counts at the finest partitioning, merged pairwise for coarser ones.
    let parts = 1usize << max_p;
    let len = n >> max_p;
    let mut sums: Vec<(u64, u64)> = (0..parts)
        .map(|j| {
            let start = if j == 0 { 0 } else { j * len - order };
            let end = (j + 1) * len - order;
            (u[start..end].iter().sum(), (end - start) as u64)
        })
        .collect();
    let mut best: Option<(u64, u32, Vec<u32>)> = None;
    for p in (0..=max_p).rev() {
        let mut bits = 2 + 4;
        let mut params = Vec::with_capacity(sums.len());
        for &(sum, count) in &sums {
            let (b, k) = rice_cost(sum, count, RICE2_MAX);
            bits += b;
            params.push(k);
        }
        let param_bits = if params.iter().any(|&k| k > RICE_MAX) { 5 } else { 4 };
        bits += param_bits * params.len() as u64;
        if best.as_ref().map_or(true, |b| bits < b.0) {
            best = Some((bits, p, params));
        }
        sums = sums.chunks(2).map(|c| c.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))).collect();
    }
    best.unwrap()
}

/// The cheapest coding of one channel: (estimated bits, coding).
fn plan(x: &[i64], bps: u32) -> (u64, Sub) {
    let n = x.len();
    if x.iter().all(|&v| v == x[0]) {
        return (8 + bps as u64, Sub::Constant(x[0]));
    }
    let mut best = (8 + n as u64 * bps as u64, Sub::Verbatim);
    for order in 0..=4.min(n.saturating_sub(1)) {
        let Some(residual) = fixed_residual(x, order) else { continue };
        let (bits, partition_order, params) = plan_residual(&residual, order, n);
        let total = 8 + order as u64 * bps as u64 + bits;
        if total < best.0 {
            best = (total, Sub::Fixed { order, residual, partition_order, params });
        }
    }
    best
}

fn write_sub(w: &mut BitWriter, sub: &Sub, x: &[i64], bps: u32) {
    match sub {
        Sub::Constant(v) => {
            w.put(8, 0b0000_0000);
            w.put_signed(bps, *v);
        }
        Sub::Verbatim => {
            w.put(8, 0b0000_0010);
            for &v in x {
                w.put_signed(bps, v);
            }
        }
        Sub::Fixed { order, residual, partition_order, params } => {
            w.put(8, (0b00_1000 | (*order as u64)) << 1); // 0, type 001xxx, no wasted bits
            for &v in &x[..*order] {
                w.put_signed(bps, v);
            }
            let rice2 = params.iter().any(|&k| k > RICE_MAX);
            w.put(2, rice2 as u64);
            w.put(4, *partition_order as u64);
            let n = x.len();
            let len = n >> partition_order;
            let param_bits = if rice2 { 5 } else { 4 };
            for (j, &k) in params.iter().enumerate() {
                w.put(param_bits, k as u64);
                let start = if j == 0 { 0 } else { j * len - order };
                let end = (j + 1) * len - order;
                for &r in &residual[start..end] {
                    let u = zigzag(r);
                    w.unary(u >> k);
                    w.put(k, u);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crcs_match_the_format() {
        // CRC-8 (poly 0x07) and CRC-16 (poly 0x8005, the "BUYPASS" variant) check values.
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(b"123456789"), 0xFEE8);
    }

    #[test]
    fn test_coded_numbers() {
        let code = |n| {
            let mut w = BitWriter::default();
            put_coded_number(&mut w, n);
            w.finish()
        };
        assert_eq!(code(0x7F), [0x7F]);
        assert_eq!(code(0x80), [0xC2, 0x80]);
        assert_eq!(code(0x800), [0xE0, 0xA0, 0x80]);
        assert_eq!(code(0x1_0000), [0xF0, 0x90, 0x80, 0x80]);
    }

    #[test]
    fn test_bit_writer() {
        let mut w = BitWriter::default();
        w.put(3, 0b101);
        w.unary(2);
        w.put_signed(4, -1);
        w.put(36, 0x1_2345_6789);
        assert_eq!(w.finish(), [0xA7, 0xC4, 0x8D, 0x15, 0x9E, 0x24]);
    }
}
