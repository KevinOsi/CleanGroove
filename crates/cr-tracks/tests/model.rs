//! Track sheet editing, sidecar round trip, and the Discogs conversion.

use cr_tracks::{ReleaseInfo, Track, TrackSheet};

const SR: u32 = 1000;

fn sheet() -> TrackSheet {
    let mut s = TrackSheet::new("rip.wav", SR, 100_000);
    let t = |start, end, linked| Track { title: format!("t{start}"), start, end, linked, ..Default::default() };
    // 01 and 02 linked; dead space before 03 (a side change).
    s.tracks = vec![t(1_000, 30_000, false), t(30_000, 50_000, true), t(60_000, 90_000, false)];
    s
}

#[test]
fn test_linked_boundary_moves_both_tracks() {
    let mut s = sheet();
    s.set_start(1, 32_000);
    assert_eq!((s.tracks[0].end, s.tracks[1].start), (32_000, 32_000));
    s.set_end(0, 29_000);
    assert_eq!((s.tracks[0].end, s.tracks[1].start), (29_000, 29_000));
    assert!(s.check().is_empty());
}

#[test]
fn test_unlinked_boundary_moves_one_side_and_respects_neighbours() {
    let mut s = sheet();
    s.set_end(1, 55_000);
    assert_eq!((s.tracks[1].end, s.tracks[2].start), (55_000, 60_000));
    // Cannot run into the next track, nor shrink below one second.
    s.set_end(1, 70_000);
    assert_eq!(s.tracks[1].end, 60_000);
    s.set_start(2, 10_000);
    assert_eq!(s.tracks[2].start, 60_000);
    s.set_start(2, 95_000);
    assert_eq!(s.tracks[2].start, 89_000);
    assert!(s.check().is_empty());
}

#[test]
fn test_link_closes_gap_into_previous_tail_and_unlink_keeps_positions() {
    let mut s = sheet();
    s.link(2);
    assert!(s.tracks[2].linked);
    assert_eq!((s.tracks[1].end, s.tracks[2].start), (60_000, 60_000));
    s.unlink(1);
    assert_eq!((s.tracks[0].end, s.tracks[1].start), (30_000, 30_000));
    s.set_start(1, 35_000);
    assert_eq!((s.tracks[0].end, s.tracks[1].start), (30_000, 35_000));
    assert!(s.check().is_empty());
}

#[test]
fn test_check_reports_overlap_and_broken_link() {
    let mut s = sheet();
    s.tracks[1].start = 29_000;
    let p = s.check();
    assert!(p.iter().any(|m| m.contains("overlaps")), "{p:?}");
    assert!(p.iter().any(|m| m.contains("linked")), "{p:?}");
}

#[test]
fn test_numbers_have_leading_zero() {
    let mut s = sheet();
    assert_eq!(s.number_label(0), "01");
    s.tracks = vec![Track::default(); 120];
    assert_eq!(s.number_label(4), "005");
}

#[test]
fn test_sidecar_round_trip_and_labels() {
    let mut s = sheet();
    s.album.album_artist = "Various Artists".into();
    s.tracks[0].artist = "Someone".into();
    let dir = std::env::temp_dir().join(format!("cr-tracks-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = TrackSheet::sidecar_path(dir.join("rip.wav"));
    assert!(path.to_string_lossy().ends_with("rip.wav.cleangroove-tracks.json"));
    s.save(&path).unwrap();
    assert_eq!(TrackSheet::load(&path).unwrap(), s);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(s.audacity_labels().starts_with("1.000000\t30.000000\t01 t1000\n"));
}

#[test]
fn test_from_discogs_credits_sides_and_headings() {
    let r: cr_discogs::Release = serde_json::from_str(
        r#"{"id": 42, "title": "Soundtrack", "year": 1988,
            "artists": [{"name": "Various", "join": ""}],
            "labels": [{"name": "A&M (2)", "catno": "SP-3913"}],
            "genres": ["Rock", "Stage & Screen"], "styles": ["Soundtrack"],
            "tracklist": [
              {"position": "", "type_": "heading", "title": "Side One", "duration": ""},
              {"position": "A1", "type_": "track", "title": "One", "duration": "3:05",
               "artists": [{"name": "Simon (3)", "join": "&"}, {"name": "Garfunkel", "join": ""}]},
              {"position": "A2", "type_": "track", "title": "Two", "duration": ""},
              {"position": "B1", "type_": "track", "title": "Three", "duration": "4:00",
               "artists": [{"name": "A", "join": "Feat."}, {"name": "B", "join": ","}, {"name": "C"}]}
            ]}"#,
    )
    .unwrap();
    let info = ReleaseInfo::from_discogs(&r);
    let a = &info.album;
    assert_eq!(a.album_artist, "Various Artists");
    assert_eq!((a.label.as_str(), a.catalog_number.as_str(), a.year), ("A&M", "SP-3913", Some(1988)));
    assert_eq!(a.genre, "Rock; Stage & Screen");
    assert_eq!(a.discogs_release_id, Some(42));
    let artists: Vec<_> = info.tracks.iter().map(|t| t.artist.as_str()).collect();
    assert_eq!(artists, ["Simon & Garfunkel", "Various Artists", "A Feat. B, C"]);
    assert_eq!(info.tracks[0].seconds, Some(185.0));
    assert_eq!(info.tracks[1].seconds, None);
    assert_eq!(info.sides(), vec![0..2, 2..3]);
}

#[test]
fn test_tracks_of_a_bundled_cd_are_left_out() {
    let r: cr_discogs::Release = serde_json::from_str(
        r#"{"id": 1, "title": "LP + CD", "artists": [{"name": "X"}], "tracklist": [
            {"position": "A1", "type_": "track", "title": "a", "duration": "5:00"},
            {"position": "AA", "type_": "track", "title": "flip", "duration": "3:00"},
            {"position": "1", "type_": "track", "title": "a", "duration": "5:00"},
            {"position": "CD-2", "type_": "track", "title": "bonus", "duration": "4:00"}]}"#,
    )
    .unwrap();
    let info = ReleaseInfo::from_discogs(&r);
    let kept: Vec<_> = info.tracks.iter().map(|t| t.position.as_str()).collect();
    let left: Vec<_> = info.omitted.iter().map(|t| t.position.as_str()).collect();
    assert_eq!((kept, left), (vec!["A1", "AA"], vec!["1", "CD-2"]));
    assert_eq!(info.sides(), vec![0..1, 1..2]);
}

#[test]
fn test_split_merge_and_remove() {
    let mut s = TrackSheet::new("rip.wav", SR, 100_000);
    s.album.album_artist = "Band".into();
    // An empty sheet becomes one track, then splits.
    assert_eq!(s.split_at(40_000), Some(1));
    assert_eq!((s.tracks[0].start, s.tracks[0].end, s.tracks[1].end), (0, 40_000, 100_000));
    assert!(s.tracks[1].linked && s.tracks[1].artist == "Band");
    assert_eq!(s.split_at(70_000), Some(2));
    // Too close to a boundary: no split.
    assert_eq!(s.split_at(70_500), None);
    s.tracks[0].title = "first".into();
    s.merge_with_previous(1);
    assert_eq!(s.tracks.len(), 2);
    assert_eq!((s.tracks[0].title.as_str(), s.tracks[0].end, s.tracks[1].start), ("first", 70_000, 70_000));
    assert!(s.check().is_empty());
    s.remove(0);
    assert_eq!(s.tracks.len(), 1);
    assert!(!s.tracks[0].linked);
    assert!(s.check().is_empty());
}

#[test]
fn test_quiet_sample() {
    let frames: Vec<(i32, i32)> = vec![(9, 9), (5, -5), (1, -1), (4, 4), (0, 1), (7, 7), (1, 0), (8, 8)];
    // Within 2 of frame 3: frame 4 (|0| + |1|) beats frame 2 (2); ties go to the nearer.
    assert_eq!(cr_tracks::quiet_sample(&frames, 3, 2), 4);
    assert_eq!(cr_tracks::quiet_sample(&frames, 5, 1), 4);
    assert_eq!(cr_tracks::quiet_sample(&frames, 5, 2), 4);
    assert_eq!(cr_tracks::quiet_sample(&frames, 6, 1), 6);
    assert_eq!(cr_tracks::quiet_sample(&frames, 8, 3), 8);
    assert_eq!(cr_tracks::quiet_sample(&frames, 0, 3), 0);
    assert_eq!(cr_tracks::quiet_sample(&[], 5, 3), 5);
}
