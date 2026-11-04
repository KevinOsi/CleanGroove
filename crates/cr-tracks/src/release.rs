//! A provider's release, reduced to what the track sheet needs: album fields and the track
//! list with side positions and lengths.

use crate::Album;

/// One track as the provider lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseTrack {
    /// "A1", "B3", "1"; may be empty.
    pub position: String,
    pub title: String,
    /// Track artist; the album artist when the provider credits none.
    pub artist: String,
    pub seconds: Option<f64>,
}

impl ReleaseTrack {
    /// The side this track is on: the leading letters of the position ("A", "B", "AA" for a
    /// single's flip side), or "" when the position has none.
    pub fn side(&self) -> &str {
        let p = self.position.trim();
        let n = p.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
        &p[..n]
    }

    /// True for a record side: one letter, or one letter repeated ("AA" on a single).
    pub fn is_record_side(&self) -> bool {
        let s = self.side().as_bytes();
        !s.is_empty() && s.iter().all(|&b| b == s[0])
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseInfo {
    pub album: Album,
    pub tracks: Vec<ReleaseTrack>,
    /// Tracks of another medium in the same release (a CD or download sold with the record),
    /// left out of `tracks` because the rip is of the record.
    pub omitted: Vec<ReleaseTrack>,
}

impl ReleaseInfo {
    /// Track index ranges, one per side, in tracklist order. A release without side letters is
    /// one side.
    pub fn sides(&self) -> Vec<std::ops::Range<usize>> {
        let mut out: Vec<std::ops::Range<usize>> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            match out.last_mut() {
                Some(r) if self.tracks[r.start].side() == t.side() => r.end = i + 1,
                _ => out.push(i..i + 1),
            }
        }
        out
    }

    pub fn from_discogs(r: &cr_discogs::Release) -> Self {
        let album_artist = match cr_discogs::artist_credit(&r.artists) {
            a if a == "Various" => "Various Artists".to_string(),
            a => a,
        };
        let label = r.labels.first();
        let album = Album {
            album_artist: album_artist.clone(),
            title: r.title.clone(),
            year: (r.year > 0).then_some(r.year),
            label: label.map(|l| cr_discogs::strip_disambiguation(&l.name).to_string()).unwrap_or_default(),
            catalog_number: label.map(|l| l.catno.clone()).filter(|c| c != "none").unwrap_or_default(),
            genre: r.genres.join("; "),
            style: r.styles.join("; "),
            discogs_release_id: Some(r.id),
            musicbrainz_release_id: None,
            cover_file: None,
        };
        // Headings ("Side B") are not tracks; an index track (a medley) counts as one track.
        let tracks = r
            .tracklist
            .iter()
            .filter(|t| t.kind.is_empty() || t.kind == "track" || t.kind == "index")
            .map(|t| ReleaseTrack {
                position: t.position.clone(),
                title: t.title.clone(),
                artist: if t.artists.is_empty() { album_artist.clone() } else { cr_discogs::artist_credit(&t.artists) },
                seconds: t.seconds().map(f64::from),
            })
            .collect();
        let mut info = ReleaseInfo { album, tracks, omitted: Vec::new() };
        info.keep_record_sides();
        info
    }

    /// When some tracks are on record sides, moves the others to `omitted`.
    pub fn keep_record_sides(&mut self) {
        if self.tracks.iter().any(|t| t.is_record_side()) {
            let (keep, other): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tracks).into_iter().partition(|t| t.is_record_side());
            self.tracks = keep;
            self.omitted.extend(other);
        }
    }
}
