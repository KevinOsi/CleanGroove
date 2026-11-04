//! Where album details come from: the user's Discogs collection, the Discogs database and
//! MusicBrainz. A search gives `Candidate`s, one per release, the same shape whatever the
//! source; picking one fetches its tracklist as a `ReleaseInfo`.

use crate::release::{ReleaseInfo, ReleaseTrack};
use crate::Album;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The user's own Discogs collection (searched here, from the downloaded list).
    Collection,
    Discogs,
    MusicBrainz,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Collection => "My collection",
            Source::Discogs => "Discogs",
            Source::MusicBrainz => "MusicBrainz",
        }
    }
}

/// What to search for; empty fields are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub artist: String,
    pub title: String,
    /// Catalogue number, barcode or any words.
    pub text: String,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.artist.trim().is_empty() && self.title.trim().is_empty() && self.text.trim().is_empty()
    }
}

/// One release found by a search.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidate {
    pub source: Option<Source>,
    /// Discogs release number, or MusicBrainz release id.
    pub id: String,
    pub artist: String,
    pub title: String,
    pub year: Option<u32>,
    pub label: String,
    pub catno: String,
    pub format: String,
    pub country: String,
}

impl Candidate {
    pub fn is_vinyl(&self) -> bool {
        let f = self.format.to_lowercase();
        f.contains("vinyl") || f.contains("shellac") || f.contains("lp")
    }

    pub fn from_discogs_search(r: &cr_discogs::SearchResult) -> Self {
        let (artist, title) = r.artist_title();
        Candidate {
            source: Some(Source::Discogs),
            id: r.id.to_string(),
            artist,
            title,
            year: r.year.as_deref().and_then(|y| y.parse().ok()).filter(|&y| y > 0),
            label: r.label.first().map(|l| cr_discogs::strip_disambiguation(l).to_string()).unwrap_or_default(),
            catno: r.catno.clone().unwrap_or_default(),
            format: dedup(&r.format).join(", "),
            country: r.country.clone().unwrap_or_default(),
        }
    }

    pub fn from_collection(item: &cr_discogs::CollectionItem) -> Self {
        let b = &item.basic_information;
        let label = b.labels.first();
        let format = b
            .formats
            .iter()
            .map(|f| {
                let mut parts = vec![f.name.clone()];
                parts.extend(f.descriptions.iter().cloned());
                dedup(&parts).join(", ")
            })
            .collect::<Vec<_>>()
            .join(" + ");
        Candidate {
            source: Some(Source::Collection),
            id: item.id.to_string(),
            artist: cr_discogs::artist_credit(&b.artists),
            title: b.title.clone(),
            year: (b.year > 0).then_some(b.year),
            label: label.map(|l| cr_discogs::strip_disambiguation(&l.name).to_string()).unwrap_or_default(),
            catno: label.map(|l| l.catno.clone()).unwrap_or_default(),
            format,
            country: String::new(),
        }
    }

    pub fn from_musicbrainz(r: &cr_musicbrainz::ReleaseSummary) -> Self {
        let li = r.label_info.first();
        Candidate {
            source: Some(Source::MusicBrainz),
            id: r.id.clone(),
            artist: cr_musicbrainz::credit(&r.artist_credit),
            title: r.title.clone(),
            year: cr_musicbrainz::year(r.date.as_deref()),
            label: li.and_then(|l| l.label.as_ref()).map(|l| l.name.clone()).unwrap_or_default(),
            catno: li.and_then(|l| l.catalog_number.clone()).unwrap_or_default(),
            format: r.format(),
            country: r.country.clone().unwrap_or_default(),
        }
    }

    /// True when every word of the query appears in the artist, title, label or catalogue
    /// number (any case). Used to search the downloaded collection.
    pub fn matches(&self, q: &Query) -> bool {
        let hay = format!("{} {} {} {}", self.artist, self.title, self.label, self.catno).to_lowercase();
        let hay_tight: String = hay.chars().filter(|c| c.is_alphanumeric()).collect();
        [&q.artist, &q.title, &q.text].iter().flat_map(|f| f.split_whitespace()).all(|w| {
            let w = w.to_lowercase();
            let tight: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
            hay.contains(&w) || (!tight.is_empty() && hay_tight.contains(&tight))
        })
    }
}

/// Repeated entries dropped, order kept ("Vinyl, LP, Album, Vinyl" -> "Vinyl, LP, Album").
fn dedup(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

/// Records first, otherwise in the order found.
pub fn vinyl_first(c: &mut [Candidate]) {
    c.sort_by_key(|c| !c.is_vinyl());
}

impl ReleaseInfo {
    pub fn from_musicbrainz(r: &cr_musicbrainz::Release) -> Self {
        let album_artist = cr_musicbrainz::credit(&r.artist_credit);
        let li = r.label_info.first();
        let album = Album {
            album_artist: album_artist.clone(),
            title: r.title.clone(),
            year: cr_musicbrainz::year(r.date.as_deref()),
            label: li.and_then(|l| l.label.as_ref()).map(|l| l.name.clone()).unwrap_or_default(),
            catalog_number: li.and_then(|l| l.catalog_number.clone()).filter(|c| c != "[none]").unwrap_or_default(),
            genre: r.genres.iter().map(|g| title_case(&g.name)).collect::<Vec<_>>().join("; "),
            style: String::new(),
            discogs_release_id: None,
            musicbrainz_release_id: Some(r.id.clone()),
            cover_file: None,
        };
        let tracks = r
            .media
            .iter()
            .flat_map(|m| m.tracks.iter())
            .map(|t| ReleaseTrack {
                position: t.number.clone(),
                title: t.title.clone(),
                artist: if t.artist_credit.is_empty() { album_artist.clone() } else { cr_musicbrainz::credit(&t.artist_credit) },
                seconds: t.length.map(|ms| ms as f64 / 1000.0),
            })
            .collect();
        let mut info = ReleaseInfo { album, tracks, omitted: Vec::new() };
        info.keep_record_sides();
        info
    }
}

/// MusicBrainz genres are lower case ("hard rock" -> "Hard Rock").
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
