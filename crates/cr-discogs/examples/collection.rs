//! Lists the records in a Discogs collection as TSV: release id, artist, title, year, speed,
//! mono, media condition, ripped, genres, styles, notes. Needs `DISCOGS_TOKEN` in the
//! environment.
//!
//! usage: cargo run -p cr-discogs --example collection -- USERNAME [--all] > records.tsv
//!        (--all lists every format, not only records)

use cr_discogs::{Client, Speed};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let all = args.iter().any(|a| a == "--all");
    let user = args.iter().find(|a| !a.starts_with("--")).expect("usage: collection USERNAME [--all]");
    let client = Client::from_env().unwrap_or_else(|e| panic!("{e}"));
    let fields = client.collection_fields(user).unwrap();
    let items = client.collection(user).unwrap();
    println!("release\tartist\ttitle\tyear\tspeed\tmono\tmedia\tripped\tgenres\tstyles\tnotes");
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    for it in items.iter().filter(|it| all || it.basic_information.is_vinyl()) {
        let b = &it.basic_information;
        let speed = match b.speed() {
            Some(Speed::Rpm33) => "33",
            Some(Speed::Rpm45) => "45",
            Some(Speed::Rpm78) => "78",
            None => "",
        };
        let field = |name| clean(it.field(&fields, name).unwrap_or(""));
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            it.id,
            clean(&b.artist()),
            clean(&b.title),
            b.year,
            speed,
            if b.is_mono() { "mono" } else { "" },
            field("Media Condition"),
            field("Ripped"),
            b.genres.join(","),
            b.styles.join(","),
            field("Notes"),
        );
    }
    eprintln!("{} items, {} listed", items.len(), items.iter().filter(|it| all || it.basic_information.is_vinyl()).count());
}
