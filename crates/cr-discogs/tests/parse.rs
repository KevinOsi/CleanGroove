//! Offline: the response shapes parse, and the format helpers read them right.

use cr_discogs::{artist_credit, CollectionItem, Field, Release, SearchResult, Speed};

const ITEM: &str = r#"{
  "id": 123, "instance_id": 456, "folder_id": 1, "rating": 0,
  "notes": [{"field_id": 1, "value": "Very Good Plus (VG+)"}, {"field_id": 5, "value": "Yes"}],
  "basic_information": {
    "title": "Test Single", "year": 1984,
    "artists": [{"name": "Some Band", "id": 1}, {"name": "Guest", "id": 2}],
    "labels": [{"name": "Label", "catno": "X-1"}],
    "formats": [{"name": "Vinyl", "qty": "1", "descriptions": ["7\"", "45 RPM", "Single", "Mono"]}],
    "genres": ["Rock"], "styles": ["Pop Rock"]
  }
}"#;

#[test]
fn test_collection_item_parses() {
    let it: CollectionItem = serde_json::from_str(ITEM).unwrap();
    let fields: Vec<Field> = serde_json::from_str(
        r#"[{"id": 1, "name": "Media Condition", "type": "dropdown", "options": []},
            {"id": 5, "name": "Ripped", "type": "dropdown"}]"#,
    )
    .unwrap();
    let b = &it.basic_information;
    assert_eq!(b.artist(), "Some Band / Guest");
    assert!(b.is_vinyl() && b.is_mono());
    assert_eq!(b.speed(), Some(Speed::Rpm45));
    assert_eq!(it.field(&fields, "Media Condition"), Some("Very Good Plus (VG+)"));
    assert_eq!(it.field(&fields, "Ripped"), Some("Yes"));
    assert_eq!(it.field(&fields, "Notes"), None);
}

#[test]
fn test_speed_from_format() {
    let speed = |formats: &str| {
        let json = format!(r#"{{"title": "t", "formats": {formats}}}"#);
        serde_json::from_str::<cr_discogs::BasicInformation>(&json).unwrap().speed()
    };
    assert_eq!(speed(r#"[{"name": "Vinyl", "descriptions": ["LP", "Album"]}]"#), Some(Speed::Rpm33));
    assert_eq!(speed(r#"[{"name": "Vinyl", "descriptions": ["12\"", "33 ⅓ RPM", "Single"]}]"#), Some(Speed::Rpm33));
    assert_eq!(speed(r#"[{"name": "Vinyl", "descriptions": ["7\""]}]"#), Some(Speed::Rpm45));
    assert_eq!(speed(r#"[{"name": "Shellac", "descriptions": ["10\""]}]"#), Some(Speed::Rpm78));
    assert_eq!(speed(r#"[{"name": "CD", "descriptions": ["Album"]}]"#), None);
}

#[test]
fn test_release_tracklist_durations() {
    let r: Release = serde_json::from_str(
        r#"{"id": 9, "title": "LP", "tracklist": [
            {"position": "A1", "type_": "track", "title": "One", "duration": "3:25"},
            {"position": "", "type_": "heading", "title": "Side B", "duration": ""},
            {"position": "B1", "type_": "track", "title": "Long", "duration": "1:02:03"}]}"#,
    )
    .unwrap();
    let secs: Vec<_> = r.tracklist.iter().map(|t| t.seconds()).collect();
    assert_eq!(secs, vec![Some(205), None, Some(3723)]);
    assert_eq!(r.tracklist[1].kind, "heading");
}

#[test]
fn test_search_result_parses() {
    let r: SearchResult = serde_json::from_str(
        r#"{"id": 4093774, "type": "release", "title": "AC/DC - Back In Black", "year": "1980", "country": "US",
            "format": ["Vinyl", "LP", "Album"], "label": ["Atlantic", "Atlantic"], "catno": "SD 16018",
            "thumb": "https://i.discogs.com/x.jpg", "master_id": 8873}"#,
    )
    .unwrap();
    assert_eq!(r.artist_title(), ("AC/DC".to_string(), "Back In Black".to_string()));
    assert_eq!(r.year.as_deref(), Some("1980"));
    // Missing fields are fine.
    let r: SearchResult = serde_json::from_str(r#"{"id": 1, "title": "Nirvana (2) - Local Anaesthetic"}"#).unwrap();
    assert_eq!(r.artist_title().0, "Nirvana");
    assert!(r.format.is_empty() && r.catno.is_none());
}

#[test]
fn test_artist_credit_joins() {
    let a: Vec<cr_discogs::Artist> = serde_json::from_str(
        r#"[{"name": "Thelonious Monk", "join": "&"}, {"name": "Johnny Mercer (2)", "join": ""}]"#,
    )
    .unwrap();
    assert_eq!(artist_credit(&a), "Thelonious Monk & Johnny Mercer");
}
