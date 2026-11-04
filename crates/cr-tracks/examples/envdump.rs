//! Prints the envelope level (dBFS above the rumble band) every STEP_S (default 0.5 s) over a time range.
//!
//! usage: cargo run --release -p cr-tracks --example envdump -- AUDIO FROM_S TO_S [STEP_S]
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (from, to): (f64, f64) = (a[1].parse().unwrap(), a[2].parse().unwrap());
    let mut r = cr_audio_io::AudioFileReader::open(&a[0]).unwrap();
    let sr = r.meta.sample_rate;
    let env = cr_tracks::Envelope::from_frames(&r.read_all_frames().unwrap(), sr);
    println!("quiet below {:.1}", env.quiet_threshold());
    let mut t = from;
    while t < to {
        let h = env.s_to_hops(t).min(env.len() - 1);
        println!("{:>8.1} {:>7.1}", t, env.db[h]);
        t += a.get(3).map_or(0.5, |s| s.parse().unwrap());
    }
}
