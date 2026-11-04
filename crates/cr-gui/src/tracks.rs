//! Tracks mode: the album's metadata and track splits for the loaded rip (`cr_tracks`).
//!
//! The track sheet is kept next to the audio as `<file>.cleangroove-tracks.json` and saved a moment
//! after every change. The level envelope (for the first guess and for snapping), searches
//! and release lookups (Discogs, MusicBrainz) run on background threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use cr_tracks::cover::Cover;
use cr_tracks::export::ExportOptions;
use cr_tracks::providers::{vinyl_first, Candidate, Query, Source};
use cr_tracks::{place_tracks, quiet_sample, Envelope, Placement, ReleaseInfo, TrackSheet, SETTLE_S};

/// The sheet is saved this long after the last change.
const AUTOSAVE_AFTER: Duration = Duration::from_millis(800);
/// Shortest quiet stretch offered as a snap point.
const SNAP_GAP_S: f64 = 0.4;
/// Largest image downloaded from a dropped or pasted link.
const MAX_COVER_DOWNLOAD: u64 = 30 * 1024 * 1024;
/// Results asked of each online source per search.
const SEARCH_LIMIT: u32 = 25;
/// Collection entries listed when the search is empty (browsing the collection).
const COLLECTION_BROWSE: usize = 300;

/// A release to fetch: a Discogs release number or a MusicBrainz release id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseRef {
    Discogs(u64),
    MusicBrainz(String),
}

impl ReleaseRef {
    pub fn of(c: &Candidate) -> Option<Self> {
        match c.source? {
            Source::Collection | Source::Discogs => c.id.parse().ok().map(ReleaseRef::Discogs),
            Source::MusicBrainz => Some(ReleaseRef::MusicBrainz(c.id.clone())),
        }
    }

    fn describe(&self) -> String {
        match self {
            ReleaseRef::Discogs(id) => format!("Discogs release {}", id),
            ReleaseRef::MusicBrainz(id) => format!("MusicBrainz release {}", id),
        }
    }

    fn fetch(&self, mb_contact: &str) -> Result<ReleaseInfo, String> {
        match self {
            ReleaseRef::Discogs(id) => {
                let r = crate::prefs::discogs_client().release(*id).map_err(|e| format!("Discogs: {}", e))?;
                Ok(ReleaseInfo::from_discogs(&r))
            }
            ReleaseRef::MusicBrainz(id) => {
                let r = cr_musicbrainz::Client::new(mb_contact).release(id).map_err(|e| format!("MusicBrainz: {}", e))?;
                Ok(ReleaseInfo::from_musicbrainz(&r))
            }
        }
    }
}

/// The search window: what to look for, where, and what was found.
pub struct SearchState {
    pub open: bool,
    pub query: Query,
    pub use_collection: bool,
    pub use_discogs: bool,
    pub use_musicbrainz: bool,
    pub results: Vec<Candidate>,
    pub status: String,
    /// The user's Discogs collection, downloaded on the first collection search.
    collection: Option<Arc<Vec<Candidate>>>,
    rx: Option<Receiver<SearchDone>>,
}

struct SearchDone {
    results: Vec<Candidate>,
    errors: Vec<String>,
    collection: Option<Arc<Vec<Candidate>>>,
}

impl Default for SearchState {
    fn default() -> Self {
        SearchState {
            open: false,
            query: Query::default(),
            use_collection: true,
            use_discogs: true,
            use_musicbrainz: true,
            results: Vec::new(),
            status: String::new(),
            collection: None,
            rx: None,
        }
    }
}

impl SearchState {
    pub fn is_searching(&self) -> bool {
        self.rx.is_some()
    }

    /// Searches the chosen sources in the background.
    pub fn run(&mut self, mb_contact: &str) {
        let token = crate::prefs::discogs_token().0;
        let (q, mb_contact, cached) = (self.query.clone(), mb_contact.to_string(), self.collection.clone());
        let (use_coll, use_discogs, use_mb) = (self.use_collection && token.is_some(), self.use_discogs && token.is_some(), self.use_musicbrainz);
        if !use_coll && (q.is_empty() || (!use_discogs && !use_mb)) {
            self.status = "Type an artist, an album or a catalogue number".to_string();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let mut done = SearchDone { results: Vec::new(), errors: Vec::new(), collection: None };
            let discogs = token.map(cr_discogs::Client::new);
            if let (true, Some(client)) = (use_coll, discogs.as_ref()) {
                let coll = match cached {
                    Some(c) => Ok(c),
                    None => download_collection(client).map(Arc::new),
                };
                match coll {
                    Ok(c) => {
                        let mut hits: Vec<Candidate> = c.iter().filter(|x| x.matches(&q)).cloned().collect();
                        hits.truncate(COLLECTION_BROWSE);
                        done.results.extend(hits);
                        done.collection = Some(c);
                    }
                    Err(e) => done.errors.push(format!("Your collection: {}", e)),
                }
            }
            if !q.is_empty() {
                if let (true, Some(client)) = (use_discogs, discogs.as_ref()) {
                    let dq = cr_discogs::SearchQuery { artist: q.artist.clone(), title: q.title.clone(), text: q.text.clone() };
                    match client.search(&dq, SEARCH_LIMIT) {
                        Ok(r) => {
                            let mut found: Vec<Candidate> = r
                                .iter()
                                .map(Candidate::from_discogs_search)
                                .filter(|c| !done.results.iter().any(|m| m.id == c.id))
                                .collect();
                            vinyl_first(&mut found);
                            done.results.extend(found);
                        }
                        Err(e) => done.errors.push(format!("Discogs: {}", e)),
                    }
                }
                if use_mb {
                    match cr_musicbrainz::Client::new(&mb_contact).search(&q.artist, &q.title, &q.text, SEARCH_LIMIT) {
                        Ok(r) => {
                            let mut found: Vec<Candidate> = r.iter().map(Candidate::from_musicbrainz).collect();
                            vinyl_first(&mut found);
                            done.results.extend(found);
                        }
                        Err(e) => done.errors.push(format!("MusicBrainz: {}", e)),
                    }
                }
            }
            let _ = tx.send(done);
        });
        self.rx = Some(rx);
        self.results.clear();
        self.status = "Searching...".to_string();
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok(done) => {
                self.results = done.results;
                if done.collection.is_some() {
                    self.collection = done.collection;
                }
                let found = match self.results.len() {
                    0 => "Nothing found".to_string(),
                    1 => "1 release".to_string(),
                    n => format!("{} releases", n),
                };
                self.status = std::iter::once(found).chain(done.errors).collect::<Vec<_>>().join("  |  ");
                self.rx = None;
            }
            Err(TryRecvError::Disconnected) => self.rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Forgets the downloaded collection, so the next search downloads it again.
    pub fn refresh_collection(&mut self) {
        self.collection = None;
    }

    pub fn has_collection(&self) -> bool {
        self.collection.is_some()
    }
}

/// The user's whole collection, as search candidates.
fn download_collection(client: &cr_discogs::Client) -> Result<Vec<Candidate>, String> {
    let me = client.identity().map_err(|e| e.to_string())?;
    let items = client.collection(&me.username).map_err(|e| e.to_string())?;
    Ok(items.iter().map(Candidate::from_collection).collect())
}

/// Which boundary handle is being dragged in the track lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    /// Start of track i (with a linked previous track, the shared boundary).
    Start(usize),
    /// End of track i (unlinked from the next).
    End(usize),
}

pub struct TracksState {
    /// The sheet for the loaded track; None with no track loaded.
    pub sheet: Option<TrackSheet>,
    pub sheet_path: Option<PathBuf>,
    pub selected: Option<usize>,
    pub drag: Option<Handle>,
    /// Snap dragged boundaries to quiet, the cursor and other boundaries (Alt holds it off).
    pub snap: bool,
    /// Release number or link typed in the lookup box (Discogs or MusicBrainz).
    pub release_text: String,
    pub search: SearchState,
    pub busy: Option<String>,
    pub envelope: Option<Arc<Envelope>>,
    /// Snap points from the envelope: starts and ends of quiet stretches, frames.
    pub snap_points: Vec<u64>,
    /// Lines from the last first guess: warnings and tracks left out.
    pub notes: Vec<String>,
    /// The rip, for settling boundaries on quiet samples; and its path.
    pub frames: Option<Arc<Vec<(i32, i32)>>>,
    pub audio_path: Option<PathBuf>,
    /// The front cover (kept next to the rip as `<rip>.cover.jpg`); `cover_changed` asks the
    /// panel to make a new thumbnail.
    pub cover: Option<Cover>,
    pub cover_changed: bool,
    pub cover_note: String,
    /// The export window, and where the last export went.
    pub export_open: bool,
    pub overwrite: bool,
    pub last_export: Option<PathBuf>,
    /// The MusicBrainz contact the last lookup used (the cover fetch after it uses it too).
    mb_contact: String,
    dirty_since: Option<Instant>,
    envelope_rx: Option<Receiver<Envelope>>,
    lookup_rx: Option<Receiver<Result<Placement, String>>>,
    cover_rx: Option<Receiver<Result<(Cover, String), String>>>,
}

impl Default for TracksState {
    fn default() -> Self {
        Self::new()
    }
}

impl TracksState {
    pub fn new() -> Self {
        TracksState {
            sheet: None,
            sheet_path: None,
            selected: None,
            drag: None,
            snap: true,
            release_text: String::new(),
            search: SearchState::default(),
            busy: None,
            envelope: None,
            snap_points: Vec::new(),
            notes: Vec::new(),
            frames: None,
            audio_path: None,
            cover: None,
            cover_changed: true,
            cover_note: String::new(),
            export_open: false,
            overwrite: false,
            last_export: None,
            mb_contact: String::new(),
            dirty_since: None,
            envelope_rx: None,
            lookup_rx: None,
            cover_rx: None,
        }
    }

    /// A new track was loaded: read its sheet if it has one, else start an empty one. Returns
    /// a status line.
    pub fn open_for(&mut self, audio: Option<&Path>, filename: &str, sample_rate: u32, total_frames: u64) -> Option<String> {
        self.close();
        let path = audio.map(TrackSheet::sidecar_path);
        let mut msg = None;
        let sheet = match path.as_ref().filter(|p| p.exists()).map(TrackSheet::load) {
            Some(Ok(s)) if s.total_frames == total_frames && s.sample_rate == sample_rate => {
                msg = Some(format!("Track splits loaded ({} tracks)", s.tracks.len()));
                s
            }
            Some(Ok(_)) => {
                msg = Some("The saved track splits are for a file of another length; starting fresh".to_string());
                TrackSheet::new(filename, sample_rate, total_frames)
            }
            Some(Err(e)) => {
                msg = Some(format!("Could not read the saved track splits: {}", e));
                TrackSheet::new(filename, sample_rate, total_frames)
            }
            None => TrackSheet::new(filename, sample_rate, total_frames),
        };
        if let Some(id) = sheet.album.discogs_release_id {
            self.release_text = id.to_string();
        } else if let Some(id) = &sheet.album.musicbrainz_release_id {
            self.release_text = format!("https://musicbrainz.org/release/{}", id);
        }
        if let (Some(name), Some(dir)) = (sheet.album.cover_file.as_ref(), audio.and_then(Path::parent)) {
            match Cover::load(&dir.join(name)) {
                Ok(c) => self.cover = Some(c),
                Err(e) => self.cover_note = format!("Cover not loaded: {}", e),
            }
        }
        self.sheet = Some(sheet);
        self.sheet_path = path;
        self.audio_path = audio.map(Path::to_path_buf);
        msg
    }

    /// A boundary placed by hand: the quietest sample within `SETTLE_S`.
    pub fn settle(&self, frame: u64) -> u64 {
        match (self.frames.as_ref(), self.sheet.as_ref()) {
            (Some(f), Some(s)) => quiet_sample(f, frame, (SETTLE_S * s.sample_rate as f64) as u64),
            _ => frame,
        }
    }

    /// The export options from the saved preferences and this window.
    pub fn export_options(&self, prefs: &crate::prefs::Prefs, rip_bits: u16) -> ExportOptions {
        let bits = match prefs.export_bits {
            16 => 16,
            24 => 24,
            _ if rip_bits == 16 => 16,
            _ => 24,
        };
        let template = if prefs.export_template.trim().is_empty() { cr_tracks::export::DEFAULT_TEMPLATE.to_string() } else { prefs.export_template.clone() };
        ExportOptions {
            root: PathBuf::from(&prefs.export_root),
            template,
            bits,
            overwrite: self.overwrite,
            embed_cover: prefs.embed_cover,
            cover_jpg: prefs.cover_jpg,
        }
    }

    /// Fetches the front cover for the album's release in the background.
    pub fn fetch_cover(&mut self, mb_contact: &str) {
        let Some(sheet) = self.sheet.as_ref() else { return };
        let album = sheet.album.clone();
        let contact = mb_contact.to_string();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(cr_tracks::cover::fetch_cover(&album, &crate::prefs::discogs_client(), &contact));
        });
        self.cover_rx = Some(rx);
        self.cover_note = "Fetching the cover...".to_string();
    }

    pub fn is_fetching_cover(&self) -> bool {
        self.cover_rx.is_some()
    }

    /// Uses an image file as the cover.
    pub fn choose_cover(&mut self, path: &Path) {
        match Cover::load(path).and_then(|c| c.as_jpeg()) {
            Ok(c) => self.keep_cover(c, &format!("Cover from {}", path.file_name().unwrap_or_default().to_string_lossy())),
            Err(e) => self.cover_note = e,
        }
    }

    /// Uses something dropped on the window as the cover: an image file, an image dragged from
    /// a browser, or a link to one (downloaded). False when it is none of those.
    pub fn drop_cover(&mut self, item: &crate::drop::DropItem) -> bool {
        use crate::drop::DropItem;
        let (name, bytes) = match item {
            DropItem::Url(url) => {
                self.download_cover(url);
                return true;
            }
            DropItem::Bytes { name, bytes } => (name.clone(), Some(bytes.clone())),
            DropItem::File(p) => (p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), std::fs::read(p).ok()),
        };
        let Some(cover) = bytes.and_then(Cover::from_bytes) else { return false };
        match cover.as_jpeg() {
            Ok(c) => self.keep_cover(c, &format!("Cover dropped: {}", name)),
            Err(e) => self.cover_note = format!("Could not read the dropped image: {}", e),
        }
        true
    }

    /// Downloads an image from a web address as the cover, in the background.
    pub fn download_cover(&mut self, url: &str) {
        let url = url.to_string();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = (|| {
                let resp = ureq::get(&url)
                    .set("User-Agent", concat!("CleanGroove/", env!("CARGO_PKG_VERSION")))
                    .call()
                    .map_err(|e| format!("Could not download the image: {}", e))?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::Read::take(resp.into_reader(), MAX_COVER_DOWNLOAD), &mut bytes).map_err(|e| e.to_string())?;
                let cover = Cover::from_bytes(bytes).ok_or("The link is not to a JPEG, PNG, WebP or GIF image")?;
                let host = url.split('/').nth(2).unwrap_or("the web").to_string();
                Ok((cover.as_jpeg()?, host))
            })();
            let _ = tx.send(result);
        });
        self.cover_rx = Some(rx);
        self.cover_note = "Downloading the dropped image...".to_string();
    }

    /// Uses the image on the clipboard as the cover ("Copy image" in a browser), or the image
    /// a copied link points to.
    pub fn paste_cover(&mut self) {
        let Ok(mut clip) = arboard::Clipboard::new() else {
            self.cover_note = "The clipboard is not available".into();
            return;
        };
        if let Ok(img) = clip.get_image() {
            let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned());
            let mut png = Vec::new();
            let encoded = rgba.map(|i| image::DynamicImage::ImageRgba8(i).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png));
            match encoded.and_then(|r| r.ok()).and_then(|_| Cover::from_bytes(png)).map(|c| c.as_jpeg()) {
                Some(Ok(c)) => self.keep_cover(c, "Cover pasted"),
                Some(Err(e)) => self.cover_note = format!("Could not read the pasted image: {}", e),
                None => self.cover_note = "Could not read the pasted image".into(),
            }
            return;
        }
        match clip.get_text().ok().and_then(|t| crate::drop::url_from_text(&t)) {
            Some(url) => self.download_cover(&url),
            None => self.cover_note = "The clipboard holds no image: use Copy image in the browser, then Paste".into(),
        }
    }

    pub fn remove_cover(&mut self) {
        if let (Some(audio), Some(sheet)) = (self.audio_path.as_ref(), self.sheet.as_mut()) {
            if sheet.album.cover_file.take().is_some() {
                let _ = std::fs::remove_file(Cover::path_for(audio));
            }
        }
        self.cover = None;
        self.cover_changed = true;
        self.cover_note.clear();
        self.changed();
    }

    /// Saves the cover next to the rip and records it in the sheet.
    fn keep_cover(&mut self, c: Cover, note: &str) {
        let Some(audio) = self.audio_path.clone() else { return };
        let path = Cover::path_for(&audio);
        match std::fs::write(&path, &c.bytes) {
            Ok(()) => {
                if let Some(sheet) = self.sheet.as_mut() {
                    sheet.album.cover_file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                }
                let size = c.dimensions().map(|(w, h)| format!(", {} x {}", w, h)).unwrap_or_default();
                self.cover_note = format!("{}{}", note, size);
                self.cover = Some(c);
                self.cover_changed = true;
                self.changed();
            }
            Err(e) => self.cover_note = format!("Could not save the cover to {}: {}", path.display(), e),
        }
    }

    /// Forgets the file's sheet. The snap setting, the search sources and the downloaded
    /// collection stay.
    pub fn close(&mut self) {
        self.flush();
        let mut search = std::mem::take(&mut self.search);
        search.open = false;
        search.results.clear();
        search.status.clear();
        search.query = Query::default();
        *self = TracksState { snap: self.snap, search, ..TracksState::new() };
    }

    /// Opens the search window, filled in from the album fields or else the file name
    /// ("Artist - Album.wav").
    pub fn open_search(&mut self) {
        let Some(sheet) = self.sheet.as_ref() else { return };
        if self.search.query.is_empty() {
            let a = &sheet.album;
            self.search.query = if !a.album_artist.is_empty() || !a.title.is_empty() {
                Query { artist: a.album_artist.clone(), title: a.title.clone(), text: String::new() }
            } else {
                let stem = Path::new(&sheet.source_file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                match stem.split_once(" - ") {
                    Some((artist, title)) => Query { artist: artist.trim().into(), title: title.trim().into(), text: String::new() },
                    None => Query { text: stem, ..Default::default() },
                }
            };
        }
        self.search.open = true;
    }

    /// Starts measuring the level envelope, once per track.
    pub fn ensure_envelope(&mut self, frames: &Arc<Vec<(i32, i32)>>, sample_rate: u32) {
        if self.envelope.is_some() || self.envelope_rx.is_some() || self.sheet.is_none() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.frames = Some(Arc::clone(frames));
        let frames = Arc::clone(frames);
        std::thread::spawn(move || {
            let _ = tx.send(Envelope::from_frames(&frames, sample_rate));
        });
        self.envelope_rx = Some(rx);
    }

    /// Fetches the release in the lookup box and places its tracks.
    pub fn look_up(&mut self, frames: &Arc<Vec<(i32, i32)>>, sample_rate: u32, mb_contact: &str) {
        match parse_release_ref(&self.release_text) {
            Some(r) => self.fetch_and_place(r, frames, sample_rate, mb_contact),
            None => self.notes = vec!["Enter a Discogs release number or link, or a musicbrainz.org/release/... link".to_string()],
        }
    }

    /// Fetches a release found by a search and places its tracks.
    pub fn use_candidate(&mut self, c: &Candidate, frames: &Arc<Vec<(i32, i32)>>, sample_rate: u32, mb_contact: &str) {
        if let Some(r) = ReleaseRef::of(c) {
            self.release_text = match &r {
                ReleaseRef::Discogs(id) => id.to_string(),
                ReleaseRef::MusicBrainz(id) => format!("https://musicbrainz.org/release/{}", id),
            };
            self.search.open = false;
            self.fetch_and_place(r, frames, sample_rate, mb_contact);
        }
    }

    fn fetch_and_place(&mut self, r: ReleaseRef, frames: &Arc<Vec<(i32, i32)>>, sample_rate: u32, mb_contact: &str) {
        let Some(sheet) = self.sheet.as_ref() else { return };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let frames = Arc::clone(frames);
        let envelope = self.envelope.clone();
        let source = sheet.source_file.clone();
        let mb_contact = mb_contact.to_string();
        self.mb_contact = mb_contact.clone();
        self.busy = Some(format!("Looking up {}...", r.describe()));
        std::thread::spawn(move || {
            let result = (|| {
                let info = r.fetch(&mb_contact)?;
                if info.tracks.is_empty() {
                    return Err(format!("{} has no tracklist", r.describe()));
                }
                let env = envelope.unwrap_or_else(|| Arc::new(Envelope::from_frames(&frames, sample_rate)));
                let mut p = place_tracks(&info, &env, frames.len() as u64, &source);
                // Every boundary on its quietest nearby sample, as when placed by hand.
                let radius = (SETTLE_S * sample_rate as f64) as u64;
                for i in 0..p.sheet.tracks.len() {
                    let start = quiet_sample(&frames, p.sheet.tracks[i].start, radius);
                    p.sheet.set_start(i, start);
                    if !p.sheet.tracks.get(i + 1).is_some_and(|t| t.linked) {
                        let end = quiet_sample(&frames, p.sheet.tracks[i].end, radius);
                        p.sheet.set_end(i, end);
                    }
                }
                if !info.omitted.is_empty() {
                    let pos: Vec<_> = info.omitted.iter().map(|t| t.position.as_str()).collect();
                    p.warnings.push(format!("Left out {} tracks of another medium in this release: {}", pos.len(), pos.join(" ")));
                }
                Ok(p)
            })();
            let _ = tx.send(result);
        });
        self.lookup_rx = Some(rx);
    }

    /// Collects background results and saves when due. Returns status lines.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.search.poll();
        if let Some(rx) = &self.envelope_rx {
            match rx.try_recv() {
                Ok(env) => {
                    let th = env.quiet_threshold();
                    self.snap_points = env
                        .quiet_runs(th, env.s_to_hops(SNAP_GAP_S))
                        .into_iter()
                        .flat_map(|r| [env.hop_to_frame(r.start), env.hop_to_frame(r.end)])
                        .collect();
                    self.envelope = Some(Arc::new(env));
                    self.envelope_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.envelope_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.lookup_rx {
            match rx.try_recv() {
                Ok(Ok(p)) => {
                    let n = p.sheet.tracks.len();
                    let title = format!("{} - {}", p.sheet.album.album_artist, p.sheet.album.title);
                    self.notes = p.warnings;
                    // The new release keeps a cover already chosen for this rip.
                    let mut sheet = p.sheet;
                    if let Some(old) = self.sheet.as_ref() {
                        sheet.album.cover_file = old.album.cover_file.clone();
                    }
                    self.sheet = Some(sheet);
                    if self.cover.is_none() {
                        let contact = self.mb_contact.clone();
                        self.fetch_cover(&contact);
                    }
                    self.selected = None;
                    self.changed();
                    self.busy = None;
                    self.lookup_rx = None;
                    out.push(format!("{}: {} tracks placed from the release's lengths. Check each split by ear.", title, n));
                }
                Ok(Err(e)) => {
                    self.busy = None;
                    self.lookup_rx = None;
                    out.push(e);
                }
                Err(TryRecvError::Disconnected) => {
                    self.busy = None;
                    self.lookup_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.cover_rx {
            match rx.try_recv() {
                Ok(Ok((c, from))) => {
                    self.cover_rx = None;
                    self.keep_cover(c, &format!("Cover from {}", from));
                }
                Ok(Err(e)) => {
                    self.cover_rx = None;
                    self.cover_note = e;
                }
                Err(TryRecvError::Disconnected) => self.cover_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.dirty_since.is_some_and(|t| t.elapsed() >= AUTOSAVE_AFTER) && self.drag.is_none() {
            if let Some(e) = self.save() {
                out.push(e);
            }
        }
        out
    }

    pub fn is_working(&self) -> bool {
        self.envelope_rx.is_some() || self.lookup_rx.is_some() || self.dirty_since.is_some() || self.search.is_searching() || self.cover_rx.is_some()
    }

    /// Marks the sheet changed; it is saved shortly after.
    pub fn changed(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    /// Saves now if there are unsaved changes.
    pub fn flush(&mut self) {
        if self.dirty_since.is_some() {
            let _ = self.save();
        }
    }

    /// Writes the sheet; returns an error line on failure.
    fn save(&mut self) -> Option<String> {
        self.dirty_since = None;
        let (Some(sheet), Some(path)) = (self.sheet.as_ref(), self.sheet_path.as_ref()) else { return None };
        sheet.save(path).err().map(|e| format!("Could not save the track splits to {}: {}", path.display(), e))
    }

    /// Nearest snap target to `frame` within `reach` frames: quiet stretch edges, the cursor and
    /// other boundaries (not the dragged one's own).
    pub fn snap(&self, frame: u64, reach: u64, playhead: u64, skip: Handle) -> u64 {
        let Some(sheet) = self.sheet.as_ref() else { return frame };
        let mut best = frame;
        let mut best_d = reach + 1;
        let mut consider = |f: u64| {
            let d = f.abs_diff(frame);
            if d < best_d {
                best = f;
                best_d = d;
            }
        };
        consider(playhead);
        self.snap_points.iter().for_each(|&f| consider(f));
        for (i, t) in sheet.tracks.iter().enumerate() {
            if skip != Handle::Start(i) {
                consider(t.start);
            }
            if skip != Handle::End(i) && !(sheet.tracks.get(i + 1).is_some_and(|n| n.linked) && skip == Handle::Start(i + 1)) {
                consider(t.end);
            }
        }
        best
    }
}

/// Track to jump to from the cursor: forward, the next track starting after it; back, the
/// track it is in, or the one before when it is within `grace` frames of that track's start.
pub fn track_jump(sheet: &TrackSheet, cursor: u64, forward: bool, grace: u64) -> Option<usize> {
    if forward {
        sheet.tracks.iter().position(|t| t.start > cursor)
    } else {
        sheet.tracks.iter().rposition(|t| t.start + grace < cursor)
    }
}

/// A MusicBrainz release id (or a musicbrainz.org/release/... link), else a Discogs release
/// number or link.
pub fn parse_release_ref(text: &str) -> Option<ReleaseRef> {
    let t = text.trim();
    let tail = t.find("/release/").map_or(t, |i| &t[i + "/release/".len()..]);
    let uuid: String = tail.chars().take(36).collect();
    let is_uuid = uuid.len() == 36
        && uuid.chars().enumerate().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
    if is_uuid {
        return Some(ReleaseRef::MusicBrainz(uuid.to_lowercase()));
    }
    parse_release_id(t).map(ReleaseRef::Discogs)
}

/// "4093774", "r4093774", "[r4093774]" or a discogs.com/release/4093774-... link.
pub fn parse_release_id(text: &str) -> Option<u64> {
    let t = text.trim();
    let t = t.find("/release/").map_or(t, |i| &t[i + "/release/".len()..]);
    let digits: String = t.trim_start_matches(['[', 'r']).chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().filter(|&n| n > 0)
}

/// m:ss.mmm
pub fn fmt_time(frames: u64, sample_rate: u32) -> String {
    let ms = (frames as f64 * 1000.0 / sample_rate.max(1) as f64).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// Seconds from "m:ss.mmm", "h:mm:ss" or plain seconds.
pub fn parse_time(text: &str) -> Option<f64> {
    text.trim().split(':').try_fold(0.0, |acc: f64, part| Some(acc * 60.0 + part.trim().parse::<f64>().ok()?)).filter(|s| *s >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_jump() {
        let mut sheet = TrackSheet::new("x.wav", 1000, 100_000);
        sheet.split_at(40_000);
        sheet.split_at(70_000);
        assert_eq!(track_jump(&sheet, 10_000, true, 1_000), Some(1));
        assert_eq!(track_jump(&sheet, 40_000, true, 1_000), Some(2));
        assert_eq!(track_jump(&sheet, 80_000, true, 1_000), None);
        // Back: the start of this track, or the previous one when right at the start.
        assert_eq!(track_jump(&sheet, 55_000, false, 1_000), Some(1));
        assert_eq!(track_jump(&sheet, 40_500, false, 1_000), Some(0));
        assert_eq!(track_jump(&sheet, 500, false, 1_000), None);
    }

    #[test]
    fn test_release_id_forms() {
        assert_eq!(parse_release_id("4093774"), Some(4093774));
        assert_eq!(parse_release_id(" [r4093774] "), Some(4093774));
        assert_eq!(parse_release_id("https://www.discogs.com/release/4093774-AC-DC-Back-In-Black"), Some(4093774));
        assert_eq!(parse_release_id("https://www.discogs.com/master/123"), None);
        assert_eq!(parse_release_id("abc"), None);
    }

    #[test]
    fn test_dropped_image_becomes_the_cover() {
        let dir = std::env::temp_dir().join(format!("cleangroove-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut t = TracksState::new();
        t.sheet = Some(TrackSheet::new("rip.wav", 1000, 1000));
        t.audio_path = Some(dir.join("rip.wav"));
        // A WebP, as a browser might hand over.
        let mut webp = Vec::new();
        image::RgbImage::from_pixel(4, 4, image::Rgb([10, 200, 10]))
            .write_to(&mut std::io::Cursor::new(&mut webp), image::ImageFormat::WebP)
            .unwrap();
        let dropped = crate::drop::DropItem::Bytes { name: "cover.webp".into(), bytes: webp };
        assert!(t.drop_cover(&dropped));
        let kept = std::fs::read(dir.join("rip.wav.cover.jpg")).unwrap();
        assert!(kept.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert_eq!(t.sheet.as_ref().unwrap().album.cover_file.as_deref(), Some("rip.wav.cover.jpg"));
        assert!(t.cover_changed && t.cover_note.starts_with("Cover dropped: cover.webp"));
        // Something that is not an image is left alone.
        let text = crate::drop::DropItem::Bytes { name: "notes.txt".into(), bytes: b"hello".to_vec() };
        assert!(!t.drop_cover(&text));
        // An image file on disk.
        let png = dir.join("art.png");
        image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3])).save(&png).unwrap();
        assert!(t.drop_cover(&crate::drop::DropItem::File(png)));
        assert!(t.cover_note.starts_with("Cover dropped: art.png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_release_refs() {
        assert_eq!(parse_release_ref("4093774"), Some(ReleaseRef::Discogs(4093774)));
        let mb = "10c1bc9b-50a4-3a3e-b83b-2fe4c16e1a9e";
        assert_eq!(parse_release_ref(mb), Some(ReleaseRef::MusicBrainz(mb.into())));
        assert_eq!(parse_release_ref(&format!("https://musicbrainz.org/release/{}/discids", mb.to_uppercase())), Some(ReleaseRef::MusicBrainz(mb.into())));
        assert_eq!(parse_release_ref("https://www.discogs.com/release/4093774-AC-DC"), Some(ReleaseRef::Discogs(4093774)));
        assert_eq!(parse_release_ref("hello"), None);
    }

    #[test]
    fn test_open_search_fills_in_from_the_file_name() {
        let mut t = TracksState::new();
        t.sheet = Some(TrackSheet::new("Thelonious Monk - Plays Duke Ellington.wav", 1000, 1000));
        t.open_search();
        assert_eq!((t.search.query.artist.as_str(), t.search.query.title.as_str()), ("Thelonious Monk", "Plays Duke Ellington"));
        // The album fields win when set; a new file keeps the downloaded collection setting.
        let mut t = TracksState::new();
        let mut s = TrackSheet::new("rip.wav", 1000, 1000);
        s.album.album_artist = "AC/DC".into();
        t.sheet = Some(s);
        t.open_search();
        assert_eq!(t.search.query.artist, "AC/DC");
        t.search.use_musicbrainz = false;
        t.close();
        assert!(!t.search.open && t.search.query.is_empty() && !t.search.use_musicbrainz);
    }

    #[test]
    fn test_time_round_trip() {
        assert_eq!(fmt_time(44_100 * 75 + 441, 44_100), "1:15.010");
        assert_eq!(parse_time("1:15.010"), Some(75.01));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("12.5"), Some(12.5));
        assert_eq!(parse_time("x"), None);
    }

    #[test]
    fn test_snap_prefers_nearest_target_and_skips_own_handle() {
        let mut s = TracksState::new();
        let mut sheet = TrackSheet::new("x.wav", 1000, 100_000);
        sheet.split_at(50_000);
        s.sheet = Some(sheet);
        s.snap_points = vec![49_200, 60_000];
        // Dragging the shared boundary at 50 000 to 49 500: the quiet edge at 49 200 is nearest.
        assert_eq!(s.snap(49_500, 1_000, 0, Handle::Start(1)), 49_200);
        // Nothing within reach: unchanged.
        assert_eq!(s.snap(30_000, 1_000, 0, Handle::Start(1)), 30_000);
        // The cursor is a target too.
        assert_eq!(s.snap(70_300, 1_000, 70_000, Handle::Start(1)), 70_000);
    }
}
