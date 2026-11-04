//! The album's front cover: fetched from the Cover Art Archive (through MusicBrainz) or
//! Discogs, or chosen by the user, kept next to the rip, embedded in every track and written
//! as `cover.jpg` on export.

use std::path::{Path, PathBuf};

use crate::Album;

/// Longest side, pixels, a cover is scaled down to before it is kept (embedding a large scan
/// in every track would bloat each file).
pub const MAX_COVER_PX: u32 = 1400;

#[derive(Debug, Clone, PartialEq)]
pub struct Cover {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

impl Cover {
    /// Recognises JPEG, PNG, WebP and GIF data (browsers often serve WebP). Covers are kept
    /// and embedded as JPEG: see `as_jpeg`.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Cover> {
        let mime = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png"
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            "image/webp"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else {
            return None;
        };
        Some(Cover { bytes, mime })
    }

    pub fn load(path: &Path) -> Result<Cover, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        Cover::from_bytes(bytes).ok_or_else(|| format!("{} is not a JPEG, PNG, WebP or GIF image", path.display()))
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        image::ImageReader::new(std::io::Cursor::new(&self.bytes)).with_guessed_format().ok()?.into_dimensions().ok()
    }

    /// The cover as a JPEG no larger than `MAX_COVER_PX`; a JPEG that already fits is kept
    /// byte for byte.
    pub fn as_jpeg(&self) -> Result<Cover, String> {
        let fits = self.dimensions().is_some_and(|(w, h)| w.max(h) <= MAX_COVER_PX);
        if self.mime == "image/jpeg" && fits {
            return Ok(self.clone());
        }
        let img = image::load_from_memory(&self.bytes).map_err(|e| e.to_string())?;
        let img = if fits { img } else { img.resize(MAX_COVER_PX, MAX_COVER_PX, image::imageops::FilterType::Lanczos3) };
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img.to_rgb8()).map_err(|e| e.to_string())?;
        Ok(Cover { bytes: out, mime: "image/jpeg" })
    }

    /// The file a cover is kept in next to the rip: `<rip>.cover.jpg`.
    pub fn path_for(audio: &Path) -> PathBuf {
        let mut s = audio.as_os_str().to_os_string();
        s.push(".cover.jpg");
        PathBuf::from(s)
    }
}

/// Fetches the front cover for the album: the Cover Art Archive for its MusicBrainz release
/// (or the one linked to its Discogs release), then the Discogs release's own primary image,
/// then the Cover Art Archive's cover for the release group (another edition of the album).
/// Returns the cover (as JPEG) and where it came from.
pub fn fetch_cover(album: &Album, discogs: &cr_discogs::Client, mb_contact: &str) -> Result<(Cover, String), String> {
    let mb = cr_musicbrainz::Client::new(mb_contact);
    let mut mbid = album.musicbrainz_release_id.clone();
    if mbid.is_none() {
        if let Some(id) = album.discogs_release_id {
            mbid = mb.release_for_discogs(id).map_err(|e| format!("MusicBrainz: {}", e))?;
        }
    }
    if let Some(id) = &mbid {
        if let Some(bytes) = mb.front_cover(id).map_err(|e| format!("Cover Art Archive: {}", e))? {
            if let Some(c) = Cover::from_bytes(bytes) {
                return Ok((c.as_jpeg()?, "Cover Art Archive".into()));
            }
        }
    }
    if let Some(id) = album.discogs_release_id {
        let release = discogs.release(id).map_err(|e| format!("Discogs: {}", e))?;
        if let Some(uri) = release.cover_uri() {
            let resp = ureq::get(uri)
                .set("User-Agent", concat!("CleanGroove/", env!("CARGO_PKG_VERSION")))
                .call()
                .map_err(|e| format!("Discogs image: {}", e))?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes).map_err(|e| e.to_string())?;
            if let Some(c) = Cover::from_bytes(bytes) {
                return Ok((c.as_jpeg()?, "Discogs".into()));
            }
        }
    }
    if let Some(id) = &mbid {
        if let Some(group) = mb.release_group_of(id).map_err(|e| format!("MusicBrainz: {}", e))? {
            if let Some(c) = mb.group_front_cover(&group).map_err(|e| format!("Cover Art Archive: {}", e))?.and_then(Cover::from_bytes) {
                return Ok((c.as_jpeg()?, "Cover Art Archive (another edition of the album)".into()));
            }
        }
    }
    Err(match (mbid.is_some(), album.discogs_release_id.is_some()) {
        (false, false) => "Look up the release first: the cover comes from its MusicBrainz or Discogs entry".into(),
        _ => "Neither the Cover Art Archive nor Discogs has a cover for this release".into(),
    })
}
