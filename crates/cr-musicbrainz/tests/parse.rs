//! Offline: the response shapes parse, and the query and credit helpers read them right.

use cr_musicbrainz::{credit, search_query, year, Release, ReleaseSummary};

#[test]
fn test_search_query() {
    assert_eq!(search_query("AC/DC", "Back In Black", ""), r#"artist:"AC/DC" AND release:"Back In Black""#);
    assert_eq!(search_query("", "", "SD 16018"), r#"(catno:"SD 16018" OR barcode:"SD 16018" OR "SD 16018")"#);
    assert_eq!(search_query(r#"The "Band""#, "", ""), r#"artist:"The \"Band\"""#);
    assert_eq!(search_query(" ", "", ""), "");
}

#[test]
fn test_release_summary_parses() {
    let r: ReleaseSummary = serde_json::from_str(
        r#"{"id": "abc", "score": 100, "title": "Back in Black", "date": "1980-07-25", "country": "US",
            "artist-credit": [{"name": "AC/DC", "artist": {"id": "x", "name": "AC/DC"}}],
            "label-info": [{"catalog-number": "SD 16018", "label": {"id": "y", "name": "Atlantic"}}],
            "media": [{"format": "12\" Vinyl", "track-count": 5}, {"format": "12\" Vinyl", "track-count": 5}]}"#,
    )
    .unwrap();
    assert_eq!(credit(&r.artist_credit), "AC/DC");
    assert_eq!(r.format(), "2 x 12\" Vinyl");
    assert_eq!(year(r.date.as_deref()), Some(1980));
    assert_eq!(r.label_info[0].label.as_ref().unwrap().name, "Atlantic");
}

#[test]
fn test_release_tracklist_parses() {
    let r: Release = serde_json::from_str(
        r#"{"id": "abc", "title": "Duets", "date": "1960",
            "artist-credit": [{"name": "Ella Fitzgerald", "joinphrase": " & "}, {"name": "Louis Armstrong"}],
            "genres": [{"name": "jazz", "count": 3}],
            "media": [{"format": "12\" Vinyl", "position": 1, "tracks": [
                {"number": "A1", "position": 1, "title": "One", "length": 185000},
                {"number": "A2", "position": 2, "title": "Two", "length": null,
                 "artist-credit": [{"name": "Louis Armstrong"}]}]}]}"#,
    )
    .unwrap();
    assert_eq!(credit(&r.artist_credit), "Ella Fitzgerald & Louis Armstrong");
    let t = &r.media[0].tracks;
    assert_eq!((t[0].number.as_str(), t[0].length), ("A1", Some(185_000)));
    assert_eq!(t[1].length, None);
    assert_eq!(credit(&t[1].artist_credit), "Louis Armstrong");
    assert_eq!(r.genres[0].name, "jazz");
    assert_eq!(year(Some("")), None);
}
