//! Searches MusicBrainz (and Discogs, with `DISCOGS_TOKEN`) and prints the candidates; with a
//! MusicBrainz release id, prints its tracklist.
//!
//! usage: cargo run --release -p cr-tracks --example search -- ARTIST TITLE [TEXT]
//!        cargo run --release -p cr-tracks --example search -- --mb-release MBID
use cr_tracks::providers::{vinyl_first, Candidate, Query};
use cr_tracks::ReleaseInfo;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mb = cr_musicbrainz::Client::new("");
    if a.first().map(String::as_str) == Some("--mb-release") {
        let info = ReleaseInfo::from_musicbrainz(&mb.release(&a[1]).unwrap_or_else(|e| panic!("{e}")));
        println!("{:?}", info.album);
        for t in &info.tracks {
            println!("{:<4} {:>7} {} / {}", t.position, t.seconds.map(|s| format!("{s:.1}")).unwrap_or_default(), t.title, t.artist);
        }
        return;
    }
    let q = Query { artist: a.first().cloned().unwrap_or_default(), title: a.get(1).cloned().unwrap_or_default(), text: a.get(2).cloned().unwrap_or_default() };
    let mut found: Vec<Candidate> = mb.search(&q.artist, &q.title, &q.text, 15).unwrap_or_else(|e| panic!("{e}")).iter().map(Candidate::from_musicbrainz).collect();
    if let Ok(d) = cr_discogs::Client::from_env() {
        let dq = cr_discogs::SearchQuery { artist: q.artist.clone(), title: q.title.clone(), text: q.text.clone() };
        found.extend(d.search(&dq, 15).unwrap_or_else(|e| panic!("{e}")).iter().map(Candidate::from_discogs_search));
    }
    vinyl_first(&mut found);
    for c in &found {
        println!("{:<12} {:<38} {:<28} {:<28} {:>4} {:<14} {:<24} {}", c.source.unwrap().name(), c.id, c.artist, c.title, c.year.unwrap_or(0), c.catno, c.format, c.country);
    }
}
