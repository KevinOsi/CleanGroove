//! Search candidates from each source, the collection filter, and the MusicBrainz tracklist.

use cr_tracks::providers::{vinyl_first, Candidate, Query, Source};
use cr_tracks::ReleaseInfo;

#[test]
fn test_candidates_from_each_source() {
    let d: cr_discogs::SearchResult = serde_json::from_str(
        r#"{"id": 4093774, "title": "AC/DC - Back In Black", "year": "1980", "country": "US",
            "format": ["Vinyl", "LP", "Album", "Vinyl"], "label": ["Atlantic"], "catno": "SD 16018"}"#,
    )
    .unwrap();
    let c = Candidate::from_discogs_search(&d);
    assert_eq!((c.source, c.id.as_str(), c.artist.as_str(), c.year), (Some(Source::Discogs), "4093774", "AC/DC", Some(1980)));
    assert_eq!(c.format, "Vinyl, LP, Album");

    let item: cr_discogs::CollectionItem = serde_json::from_str(
        r#"{"id": 5, "instance_id": 1, "folder_id": 1, "basic_information": {"title": "Thelonious Monk Plays Duke Ellington",
            "year": 1956, "artists": [{"name": "Thelonious Monk"}], "labels": [{"name": "Riverside Records", "catno": "RLP 12-201"}],
            "formats": [{"name": "Vinyl", "descriptions": ["LP", "Album", "Mono"]}]}}"#,
    )
    .unwrap();
    let c = Candidate::from_collection(&item);
    assert_eq!(c.format, "Vinyl, LP, Album, Mono");
    assert_eq!(c.catno, "RLP 12-201");
    // Every word must appear somewhere; punctuation in catalogue numbers is ignored.
    let q = |a: &str, t: &str, x: &str| Query { artist: a.into(), title: t.into(), text: x.into() };
    assert!(c.matches(&q("monk", "duke", "")));
    assert!(c.matches(&q("", "", "rlp12201")));
    assert!(!c.matches(&q("monk", "mercer", "")));

    let m: cr_musicbrainz::ReleaseSummary = serde_json::from_str(
        r#"{"id": "mb-1", "title": "Back in Black", "date": "1980-07-25",
            "artist-credit": [{"name": "AC/DC"}], "media": [{"format": "CD", "track-count": 10}]}"#,
    )
    .unwrap();
    let mb = Candidate::from_musicbrainz(&m);
    assert_eq!((mb.source, mb.year, mb.format.as_str()), (Some(Source::MusicBrainz), Some(1980), "CD"));

    let mut list = vec![mb.clone(), c.clone()];
    vinyl_first(&mut list);
    assert_eq!(list[0].source, Some(Source::Collection));
}

#[test]
fn test_musicbrainz_release_to_tracks() {
    let r: cr_musicbrainz::Release = serde_json::from_str(
        r#"{"id": "mb-2", "title": "Soundtrack", "date": "1988",
            "artist-credit": [{"name": "Various Artists"}],
            "label-info": [{"catalog-number": "SP-3913", "label": {"name": "A&M"}}],
            "genres": [{"name": "hard rock"}],
            "media": [
              {"format": "12\" Vinyl", "position": 1, "tracks": [
                {"number": "A1", "title": "One", "length": 185500, "artist-credit": [{"name": "Simon", "joinphrase": " & "}, {"name": "Garfunkel"}]},
                {"number": "B1", "title": "Two", "length": null}]},
              {"format": "CD", "position": 2, "tracks": [{"number": "1", "title": "One", "length": 185000}]}]}"#,
    )
    .unwrap();
    let info = ReleaseInfo::from_musicbrainz(&r);
    assert_eq!(info.album.musicbrainz_release_id.as_deref(), Some("mb-2"));
    assert_eq!((info.album.catalog_number.as_str(), info.album.genre.as_str(), info.album.year), ("SP-3913", "Hard Rock", Some(1988)));
    let artists: Vec<_> = info.tracks.iter().map(|t| t.artist.as_str()).collect();
    assert_eq!(artists, ["Simon & Garfunkel", "Various Artists"]);
    assert_eq!(info.tracks[0].seconds, Some(185.5));
    assert_eq!(info.tracks[1].seconds, None);
    // The bundled CD is left out.
    assert_eq!(info.omitted.len(), 1);
    assert_eq!(info.sides(), vec![0..1, 1..2]);
}
