//! Minimal Discogs API client.
//!
//! Reads the user's own collection (with their private notes: media condition, ripped, ...)
//! and release details (tracklist with positions and durations), the starting point for
//! tagging and track splitting of rips.
//!
//! Authentication is a personal access token, read from the `DISCOGS_TOKEN` environment
//! variable by `Client::from_env`. Never put the token in source or in a tracked file.
//!
//! Discogs allows 60 authenticated requests a minute; the client slows down when the
//! `X-Discogs-Ratelimit-Remaining` header runs low and retries once after a 429.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use thiserror::Error;

const API: &str = "https://api.discogs.com";
const USER_AGENT: &str = concat!("CleanGroove/", env!("CARGO_PKG_VERSION"));
/// Below this many requests left in the minute window, wait before the next one.
const RATE_LOW: u32 = 5;
const RATE_WAIT: Duration = Duration::from_secs(20);

#[derive(Error, Debug)]
pub enum DiscogsError {
    #[error("DISCOGS_TOKEN is not set")]
    NoToken,
    #[error("HTTP status {0} from {1}")]
    Status(u16, String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("could not parse the response: {0}")]
    Parse(String),
}

pub struct Client {
    agent: ureq::Agent,
    /// None for anonymous use: release details work without a token (at a lower rate limit);
    /// collection and search need one.
    token: Option<String>,
}

impl Client {
    pub fn new(token: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new().user_agent(USER_AGENT).timeout(Duration::from_secs(30)).build();
        Client { agent, token: Some(token.into()) }
    }

    /// A client without a token, for public data (release details).
    pub fn anonymous() -> Self {
        let agent = ureq::AgentBuilder::new().user_agent(USER_AGENT).timeout(Duration::from_secs(30)).build();
        Client { agent, token: None }
    }

    pub fn from_env() -> Result<Self, DiscogsError> {
        let token = std::env::var("DISCOGS_TOKEN").map_err(|_| DiscogsError::NoToken)?;
        Ok(Client::new(token.trim()))
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, DiscogsError> {
        let body = self.get_text(path)?;
        serde_json::from_str(&body).map_err(|e| DiscogsError::Parse(e.to_string()))
    }

    fn get_text(&self, path: &str) -> Result<String, DiscogsError> {
        self.get_text_query(path, &[])
    }

    /// GET with query parameters (encoded here).
    fn get_text_query(&self, path: &str, query: &[(&str, &str)]) -> Result<String, DiscogsError> {
        let url = format!("{API}{path}");
        for attempt in 0..2 {
            let mut req = self.agent.get(&url);
            for (k, v) in query {
                req = req.query(k, v);
            }
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Discogs token={t}"));
            }
            let resp = req.call();
            match resp {
                Ok(r) => {
                    let left = r.header("X-Discogs-Ratelimit-Remaining").and_then(|v| v.parse::<u32>().ok());
                    let body = r.into_string().map_err(|e| DiscogsError::Transport(e.to_string()))?;
                    if left.is_some_and(|l| l < RATE_LOW) {
                        std::thread::sleep(RATE_WAIT);
                    }
                    return Ok(body);
                }
                Err(ureq::Error::Status(429, _)) if attempt == 0 => std::thread::sleep(Duration::from_secs(60)),
                Err(ureq::Error::Status(code, _)) => return Err(DiscogsError::Status(code, path.to_string())),
                Err(e) => return Err(DiscogsError::Transport(e.to_string())),
            }
        }
        Err(DiscogsError::Status(429, path.to_string()))
    }

    /// True when the client has a token (search and the collection need one).
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Searches the Discogs database for releases. Needs a token. Empty fields are left out;
    /// `text` goes in the general query (catalogue number, barcode, any words).
    pub fn search(&self, q: &SearchQuery, per_page: u32) -> Result<Vec<SearchResult>, DiscogsError> {
        if self.token.is_none() {
            return Err(DiscogsError::NoToken);
        }
        #[derive(Deserialize)]
        struct Page {
            results: Vec<SearchResult>,
        }
        let per_page = per_page.clamp(1, 100).to_string();
        let mut query: Vec<(&str, &str)> = vec![("type", "release"), ("per_page", &per_page)];
        for (k, v) in [("artist", &q.artist), ("release_title", &q.title), ("q", &q.text)] {
            if !v.trim().is_empty() {
                query.push((k, v.trim()));
            }
        }
        let body = self.get_text_query("/database/search", &query)?;
        Ok(serde_json::from_str::<Page>(&body).map_err(|e| DiscogsError::Parse(e.to_string()))?.results)
    }

    /// The user the token belongs to.
    pub fn identity(&self) -> Result<Identity, DiscogsError> {
        self.get("/oauth/identity")
    }

    /// The user's custom collection fields (`Media Condition`, `Ripped`, ...).
    pub fn collection_fields(&self, username: &str) -> Result<Vec<Field>, DiscogsError> {
        #[derive(Deserialize)]
        struct Fields {
            fields: Vec<Field>,
        }
        Ok(self.get::<Fields>(&format!("/users/{username}/collection/fields"))?.fields)
    }

    /// Every item in the user's collection (folder 0, "All"), all pages.
    pub fn collection(&self, username: &str) -> Result<Vec<CollectionItem>, DiscogsError> {
        #[derive(Deserialize)]
        struct Page {
            pagination: Pagination,
            releases: Vec<CollectionItem>,
        }
        #[derive(Deserialize)]
        struct Pagination {
            pages: u32,
        }
        let mut items = Vec::new();
        let mut page = 1;
        loop {
            let p: Page = self.get(&format!("/users/{username}/collection/folders/0/releases?per_page=100&page={page}"))?;
            items.extend(p.releases);
            if page >= p.pagination.pages {
                return Ok(items);
            }
            page += 1;
        }
    }

    /// Full release details, including the tracklist.
    pub fn release(&self, release_id: u64) -> Result<Release, DiscogsError> {
        self.get(&format!("/releases/{release_id}"))
    }

    /// Release details as the JSON text Discogs sent, for caching; parse with `parse_release`.
    pub fn release_json(&self, release_id: u64) -> Result<String, DiscogsError> {
        self.get_text(&format!("/releases/{release_id}"))
    }
}

/// Parses release JSON from `Client::release_json` or a saved copy.
pub fn parse_release(json: &str) -> Result<Release, DiscogsError> {
    serde_json::from_str(json).map_err(|e| DiscogsError::Parse(e.to_string()))
}

/// What to search for; empty fields are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    pub artist: String,
    pub title: String,
    /// Catalogue number, barcode or any words.
    pub text: String,
}

/// One release from a database search.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    pub id: u64,
    /// "Artist - Title".
    pub title: String,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub format: Vec<String>,
    #[serde(default)]
    pub label: Vec<String>,
    #[serde(default)]
    pub catno: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

impl SearchResult {
    /// The artist and title halves of "Artist - Title".
    pub fn artist_title(&self) -> (String, String) {
        match self.title.split_once(" - ") {
            Some((a, t)) => (strip_disambiguation(a).to_string(), t.to_string()),
            None => (String::new(), self.title.clone()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub id: u64,
    pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectionItem {
    /// The release id.
    pub id: u64,
    pub instance_id: u64,
    pub folder_id: u64,
    #[serde(default)]
    pub rating: u8,
    #[serde(default)]
    pub notes: Vec<Note>,
    pub basic_information: BasicInformation,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    pub field_id: u32,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BasicInformation {
    pub title: String,
    #[serde(default)]
    pub year: u32,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub formats: Vec<Format>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub styles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub name: String,
    #[serde(default)]
    pub id: u64,
    /// How this artist joins the next one in a credit: ",", "&", "Feat.", ...
    #[serde(default)]
    pub join: String,
}

/// An artist credit as it reads on the record: "Simon & Garfunkel", "A, B Feat. C". Discogs'
/// disambiguation suffix ("Nirvana (2)") is dropped.
pub fn artist_credit(artists: &[Artist]) -> String {
    let mut out = String::new();
    for (i, a) in artists.iter().enumerate() {
        out.push_str(strip_disambiguation(&a.name));
        if i + 1 < artists.len() {
            match a.join.trim() {
                "" => out.push_str(", "),
                "," => out.push_str(", "),
                j => {
                    out.push(' ');
                    out.push_str(j);
                    out.push(' ');
                }
            }
        }
    }
    out
}

/// "Nirvana (2)" -> "Nirvana".
pub fn strip_disambiguation(name: &str) -> &str {
    let n = name.trim_end();
    if let Some(open) = n.rfind(" (") {
        let inner = &n[open + 2..];
        if inner.len() > 1 && inner.ends_with(')') && inner[..inner.len() - 1].bytes().all(|b| b.is_ascii_digit()) {
            return &n[..open];
        }
    }
    n
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub catno: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Format {
    pub name: String,
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub descriptions: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub year: u32,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub formats: Vec<Format>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub styles: Vec<String>,
    #[serde(default)]
    pub tracklist: Vec<Track>,
    /// Pictures of the release; the "primary" one is usually the front cover.
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub uri: String,
}

impl Release {
    /// The front cover's address: the primary image, else the first.
    pub fn cover_uri(&self) -> Option<&str> {
        self.images
            .iter()
            .find(|i| i.kind == "primary")
            .or(self.images.first())
            .map(|i| i.uri.as_str())
            .filter(|u| !u.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    /// Side and number as printed: "A1", "B3"; empty for headings.
    pub position: String,
    #[serde(rename = "type_", default)]
    pub kind: String,
    pub title: String,
    /// Track artists, on compilations and soundtracks; empty when the release artist applies.
    #[serde(default)]
    pub artists: Vec<Artist>,
    /// "m:ss" as entered on Discogs, often empty.
    #[serde(default)]
    pub duration: String,
}

impl Track {
    /// The duration in seconds, when Discogs has one.
    pub fn seconds(&self) -> Option<u32> {
        let d = self.duration.trim();
        if d.is_empty() {
            return None;
        }
        d.split(':').try_fold(0u32, |acc, p| Some(acc * 60 + p.parse::<u32>().ok()?))
    }
}

/// Record speed, as far as the format tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Rpm33,
    Rpm45,
    Rpm78,
}

impl Speed {
    /// One turn of the record, in seconds.
    pub fn period_s(self) -> f64 {
        match self {
            Speed::Rpm33 => 1.8,
            Speed::Rpm45 => 60.0 / 45.0,
            Speed::Rpm78 => 60.0 / 78.0,
        }
    }
}

impl BasicInformation {
    pub fn artist(&self) -> String {
        self.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(" / ")
    }

    pub fn is_vinyl(&self) -> bool {
        self.formats.iter().any(|f| f.name == "Vinyl" || f.name == "Shellac")
    }

    pub fn is_mono(&self) -> bool {
        self.formats.iter().any(|f| f.descriptions.iter().any(|d| d == "Mono"))
    }

    /// The record's speed: stated in the format if Discogs has it, otherwise shellac is 78,
    /// a 7" single 45 and an LP 33. None for anything that is not a record.
    pub fn speed(&self) -> Option<Speed> {
        let f = self.formats.iter().find(|f| f.name == "Vinyl" || f.name == "Shellac")?;
        let has = |s: &str| f.descriptions.iter().any(|d| d.contains(s));
        Some(if has("78 RPM") || f.name == "Shellac" {
            Speed::Rpm78
        } else if has("45 RPM") || (has("7\"") && !has("33")) {
            Speed::Rpm45
        } else {
            Speed::Rpm33
        })
    }
}

impl CollectionItem {
    /// The value of a custom field by its name (`Media Condition`, `Ripped`, ...), given the
    /// fields from `Client::collection_fields`.
    pub fn field<'a>(&'a self, fields: &[Field], name: &str) -> Option<&'a str> {
        let id = fields.iter().find(|f| f.name == name)?.id;
        self.notes.iter().find(|n| n.field_id == id).map(|n| n.value.as_str())
    }
}
