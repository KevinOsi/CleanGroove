//! Minimal MusicBrainz client: release search, and release details with the tracklist
//! (positions, lengths in milliseconds, track artists), for tagging and track splitting.
//!
//! MusicBrainz needs no key, but asks every program to identify itself with a User-Agent
//! that includes a way to contact its user, and to make at most one request a second. The
//! client keeps that pace itself.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use thiserror::Error;

const API: &str = "https://musicbrainz.org/ws/2";
/// Time between requests (MusicBrainz allows one a second).
const PACE: Duration = Duration::from_millis(1100);

#[derive(Error, Debug)]
pub enum MusicBrainzError {
    #[error("HTTP status {0} from {1}")]
    Status(u16, String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("could not parse the response: {0}")]
    Parse(String),
}

pub struct Client {
    agent: ureq::Agent,
    last: Mutex<Option<Instant>>,
}

impl Client {
    /// `contact` (an email address or a web page) goes in the User-Agent, as MusicBrainz asks.
    pub fn new(contact: &str) -> Self {
        let ua = match contact.trim() {
            "" => format!("CleanGroove/{}", env!("CARGO_PKG_VERSION")),
            c => format!("CleanGroove/{} ( {} )", env!("CARGO_PKG_VERSION"), c),
        };
        let agent = ureq::AgentBuilder::new().user_agent(&ua).timeout(Duration::from_secs(30)).build();
        Client { agent, last: Mutex::new(None) }
    }

    fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, MusicBrainzError> {
        {
            let mut last = self.last.lock().unwrap();
            if let Some(t) = *last {
                if let Some(wait) = PACE.checked_sub(t.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            *last = Some(Instant::now());
        }
        let mut req = self.agent.get(&format!("{API}{path}")).query("fmt", "json");
        for (k, v) in query {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(r) => {
                let body = r.into_string().map_err(|e| MusicBrainzError::Transport(e.to_string()))?;
                serde_json::from_str(&body).map_err(|e| MusicBrainzError::Parse(e.to_string()))
            }
            Err(ureq::Error::Status(code, _)) => Err(MusicBrainzError::Status(code, path.to_string())),
            Err(e) => Err(MusicBrainzError::Transport(e.to_string())),
        }
    }

    /// Searches releases. Empty fields are left out; `text` matches a catalogue number, a
    /// barcode or any words.
    pub fn search(&self, artist: &str, title: &str, text: &str, limit: u32) -> Result<Vec<ReleaseSummary>, MusicBrainzError> {
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            releases: Vec<ReleaseSummary>,
        }
        let query = search_query(artist, title, text);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 100).to_string();
        Ok(self.get::<Page>("/release/", &[("query", &query), ("limit", &limit)])?.releases)
    }

    /// The MusicBrainz release linked to a Discogs release, if MusicBrainz has the link.
    pub fn release_for_discogs(&self, discogs_id: u64) -> Result<Option<String>, MusicBrainzError> {
        #[derive(Deserialize)]
        struct Url {
            #[serde(default)]
            relations: Vec<Relation>,
        }
        #[derive(Deserialize)]
        struct Relation {
            #[serde(default)]
            release: Option<ReleaseRef>,
        }
        #[derive(Deserialize)]
        struct ReleaseRef {
            id: String,
        }
        let resource = format!("https://www.discogs.com/release/{discogs_id}");
        match self.get::<Url>("/url", &[("resource", &resource), ("inc", "release-rels")]) {
            Ok(u) => Ok(u.relations.into_iter().find_map(|r| r.release.map(|r| r.id))),
            Err(MusicBrainzError::Status(404, _)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The release group (all editions of the album) a release belongs to.
    pub fn release_group_of(&self, mbid: &str) -> Result<Option<String>, MusicBrainzError> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "release-group", default)]
            release_group: Option<Group>,
        }
        #[derive(Deserialize)]
        struct Group {
            id: String,
        }
        Ok(self.get::<R>(&format!("/release/{mbid}"), &[("inc", "release-groups")])?.release_group.map(|g| g.id))
    }

    /// The front cover of a release from the Cover Art Archive (1200 px, else the original),
    /// or None when it has none.
    pub fn front_cover(&self, mbid: &str) -> Result<Option<Vec<u8>>, MusicBrainzError> {
        self.archive_front("release", mbid)
    }

    /// The front cover the Cover Art Archive shows for a release group.
    pub fn group_front_cover(&self, group_id: &str) -> Result<Option<Vec<u8>>, MusicBrainzError> {
        self.archive_front("release-group", group_id)
    }

    fn archive_front(&self, kind: &str, id: &str) -> Result<Option<Vec<u8>>, MusicBrainzError> {
        for size in ["front-1200", "front"] {
            let url = format!("https://coverartarchive.org/{kind}/{id}/{size}");
            match self.agent.get(&url).call() {
                Ok(r) => {
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut r.into_reader(), &mut bytes).map_err(|e| MusicBrainzError::Transport(e.to_string()))?;
                    return Ok(Some(bytes));
                }
                Err(ureq::Error::Status(404, _)) => continue,
                Err(ureq::Error::Status(code, _)) => return Err(MusicBrainzError::Status(code, url)),
                Err(e) => return Err(MusicBrainzError::Transport(e.to_string())),
            }
        }
        Ok(None)
    }

    /// A release with its tracklist, credits, labels and genres.
    pub fn release(&self, mbid: &str) -> Result<Release, MusicBrainzError> {
        self.get(&format!("/release/{mbid}"), &[("inc", "recordings+artist-credits+labels+genres")])
    }
}

/// The search query in MusicBrainz's query syntax.
pub fn search_query(artist: &str, title: &str, text: &str) -> String {
    let quote = |s: &str| format!("\"{}\"", s.trim().replace('\\', "\\\\").replace('"', "\\\""));
    let mut parts = Vec::new();
    if !artist.trim().is_empty() {
        parts.push(format!("artist:{}", quote(artist)));
    }
    if !title.trim().is_empty() {
        parts.push(format!("release:{}", quote(title)));
    }
    if !text.trim().is_empty() {
        let t = quote(text);
        parts.push(format!("(catno:{t} OR barcode:{t} OR {t})"));
    }
    parts.join(" AND ")
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Credit {
    pub name: String,
    #[serde(default)]
    pub joinphrase: String,
}

/// An artist credit as it reads: names with their join phrases.
pub fn credit(c: &[Credit]) -> String {
    c.iter().map(|a| format!("{}{}", a.name, a.joinphrase)).collect::<String>().trim().to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LabelInfo {
    #[serde(rename = "catalog-number", default)]
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub label: Option<Label>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediumSummary {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(rename = "track-count", default)]
    pub track_count: u32,
}

/// One release from a search.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReleaseSummary {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<Credit>,
    #[serde(rename = "label-info", default)]
    pub label_info: Vec<LabelInfo>,
    #[serde(default)]
    pub media: Vec<MediumSummary>,
    #[serde(default)]
    pub barcode: Option<String>,
}

impl ReleaseSummary {
    /// "2 x 12\" Vinyl", "CD + 12\" Vinyl".
    pub fn format(&self) -> String {
        let mut parts: Vec<(String, usize)> = Vec::new();
        for m in &self.media {
            let f = m.format.clone().unwrap_or_else(|| "?".into());
            match parts.iter_mut().find(|(p, _)| *p == f) {
                Some((_, n)) => *n += 1,
                None => parts.push((f, 1)),
            }
        }
        parts.iter().map(|(f, n)| if *n > 1 { format!("{n} x {f}") } else { f.clone() }).collect::<Vec<_>>().join(" + ")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Track {
    /// As printed: "A1", "B3", or "1".
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub position: u32,
    pub title: String,
    /// Milliseconds.
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<Credit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Medium {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Genre {
    pub name: String,
}

/// A release with its tracklist.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<Credit>,
    #[serde(rename = "label-info", default)]
    pub label_info: Vec<LabelInfo>,
    #[serde(default)]
    pub media: Vec<Medium>,
    #[serde(default)]
    pub genres: Vec<Genre>,
}

/// The year of a MusicBrainz date ("1980-07-25", "1980").
pub fn year(date: Option<&str>) -> Option<u32> {
    date.and_then(|d| d.get(..4)).and_then(|y| y.parse().ok()).filter(|&y| y > 0)
}
