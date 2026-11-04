//! Tracks mode bottom panel: the Discogs lookup, the album fields and the track table.

use egui::{Color32, RichText, Ui};

use crate::tracks::{fmt_time, parse_time, TracksState};
use crate::views::track_lane::track_color;

pub enum TracksAction {
    None,
    LookUp,
    OpenSearch,
    OpenExport,
    FetchCover,
    ChooseCover,
    PasteCover,
    RemoveCover,
    PlayFrom(u64),
    /// Put the cursor at this frame (a track's start).
    GoTo(u64),
    /// Play across the boundary at this frame (a couple of seconds either side).
    PlayAcross(u64),
    SplitAtCursor,
}

const DIM: Color32 = Color32::from_rgb(148, 163, 184);
/// Width of the album fields; the track table takes the rest.
const FIELDS_W: f32 = 300.0;

/// `cover`: the cover's thumbnail, when there is one. `dragging`: files are being dragged
/// over the window (the cover box invites the drop).
pub fn render_tracks_panel(
    ui: &mut Ui,
    tracks: &mut TracksState,
    has_track: bool,
    playhead: u64,
    cover: Option<&egui::TextureHandle>,
    dragging: bool,
) -> TracksAction {
    let mut action = TracksAction::None;
    // Boundaries set from the cursor settle on the quietest sample nearby.
    let cursor = tracks.settle(playhead);
    let (fetching, has_cover, cover_note) = (tracks.is_fetching_cover(), tracks.cover.is_some(), tracks.cover_note.clone());

    // Lookup row.
    ui.horizontal(|ui| {
        ui.label(RichText::new("TRACKS").strong().color(Color32::from_rgb(203, 213, 225)).size(10.0));
        ui.separator();
        if ui
            .add_enabled(has_track && tracks.busy.is_none(), egui::Button::new(RichText::new("Search...").strong()))
            .on_hover_text("Find the release in your Discogs collection, on Discogs or on MusicBrainz")
            .clicked()
        {
            action = TracksAction::OpenSearch;
        }
        ui.separator();
        ui.label("Release:");
        let edit = ui.add_enabled(
            has_track && tracks.busy.is_none(),
            egui::TextEdit::singleline(&mut tracks.release_text)
                .desired_width(240.0)
                .hint_text("Discogs number or link, or a MusicBrainz link"),
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let replaces = tracks.sheet.as_ref().is_some_and(|s| !s.tracks.is_empty());
        let button = ui
            .add_enabled(has_track && tracks.busy.is_none(), egui::Button::new("Look Up"))
            .on_hover_text(if replaces {
                "Fetch the release and place its tracks from its lengths. Replaces the current tracks."
            } else {
                "Fetch the release and place its tracks from its lengths"
            });
        if (button.clicked() || enter) && has_track {
            action = TracksAction::LookUp;
        }
        if let Some(b) = &tracks.busy {
            ui.spinner();
            ui.label(RichText::new(b).color(DIM).size(11.0));
        } else if has_track && tracks.envelope.is_none() {
            ui.spinner();
            ui.label(RichText::new("Measuring the quiet between tracks...").color(DIM).size(11.0));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can_export = tracks.sheet.as_ref().is_some_and(|s| !s.tracks.is_empty());
            if ui
                .add_enabled(can_export, egui::Button::new(RichText::new(" Export... ").strong().color(Color32::WHITE)).fill(Color32::from_rgb(16, 185, 129)))
                .on_hover_text("Write each track as a tagged FLAC")
                .clicked()
            {
                action = TracksAction::OpenExport;
            }
            ui.separator();
            ui.checkbox(&mut tracks.snap, "Snap").on_hover_text("Dragged boundaries snap to quiet, the cursor and other boundaries (Alt holds it off)");
            if let Some(p) = &tracks.sheet_path {
                let name = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                ui.label(RichText::new(format!("Saved to {}", name)).color(DIM).size(10.0)).on_hover_text(p.display().to_string());
            }
        });
    });
    for n in &tracks.notes {
        ui.label(RichText::new(n).color(Color32::from_rgb(251, 191, 36)).size(11.0));
    }
    ui.add_space(4.0);

    let Some(sheet) = tracks.sheet.as_mut() else {
        ui.label(RichText::new("Open an audio file (Ctrl+O) to split it into tracks.").color(DIM));
        return action;
    };
    let mut changed = false;
    let sr = sheet.sample_rate;

    ui.horizontal_top(|ui| {
        // Album fields.
        ui.vertical(|ui| {
            ui.set_width(FIELDS_W);
            egui::Grid::new("album_fields").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                let field = |ui: &mut Ui, label: &str, value: &mut String| {
                    ui.label(RichText::new(label).color(DIM));
                    let r = ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
                    ui.end_row();
                    r.changed()
                };
                let a = &mut sheet.album;
                let before = a.album_artist.clone();
                if field(ui, "Album artist", &mut a.album_artist) {
                    // Tracks credited to the album artist follow it.
                    for t in sheet.tracks.iter_mut().filter(|t| t.artist == before) {
                        t.artist = sheet.album.album_artist.clone();
                    }
                    changed = true;
                }
                let a = &mut sheet.album;
                changed |= field(ui, "Album", &mut a.title);
                let mut year = a.year.map(|y| y.to_string()).unwrap_or_default();
                if field(ui, "Year", &mut year) {
                    a.year = year.trim().parse().ok();
                    changed = true;
                }
                changed |= field(ui, "Label", &mut a.label);
                changed |= field(ui, "Catalog #", &mut a.catalog_number);
                changed |= field(ui, "Genre", &mut a.genre);
                changed |= field(ui, "Style", &mut a.style);
                ui.label(RichText::new("Release").color(DIM));
                let release = match (a.discogs_release_id, a.musicbrainz_release_id.as_ref()) {
                    (Some(id), _) => format!("Discogs {}", id),
                    (None, Some(id)) => format!("MusicBrainz {}", &id[..id.len().min(8)]),
                    _ => "-".into(),
                };
                ui.label(release);
                ui.end_row();
            });
            ui.add_space(6.0);
            // The front cover.
            ui.horizontal(|ui| {
                let size = egui::vec2(96.0, 96.0);
                let (r, response) = ui.allocate_exact_size(size, egui::Sense::hover());
                match cover {
                    Some(tex) => {
                        egui::Image::new(tex).rounding(3.0).paint_at(ui, r);
                    }
                    None => {
                        ui.painter().rect_stroke(r, 3.0, egui::Stroke::new(1.0_f32, Color32::from_rgb(71, 85, 105)));
                        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "No cover", egui::FontId::proportional(11.0), DIM);
                    }
                }
                if dragging {
                    let accent = Color32::from_rgb(52, 211, 153);
                    ui.painter().rect_filled(r, 3.0, Color32::from_rgba_unmultiplied(15, 23, 42, 200));
                    ui.painter().rect_stroke(r, 3.0, egui::Stroke::new(2.0_f32, accent));
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "Drop image", egui::FontId::proportional(12.0), accent);
                }
                response.on_hover_text("Drag an image here from a browser or a folder to use it as the cover");
                ui.vertical(|ui| {
                    if ui
                        .add_enabled(!fetching, egui::Button::new("Fetch Cover"))
                        .on_hover_text("From the Cover Art Archive (MusicBrainz) or Discogs, for the release looked up")
                        .clicked()
                    {
                        action = TracksAction::FetchCover;
                    }
                    if ui.button("Choose File...").on_hover_text("Use an image file of your own").clicked() {
                        action = TracksAction::ChooseCover;
                    }
                    if ui.button("Paste").on_hover_text("Use the image on the clipboard (Copy image in a browser), or the image a copied link points to").clicked() {
                        action = TracksAction::PasteCover;
                    }
                    if ui.add_enabled(has_cover, egui::Button::new("Remove")).clicked() {
                        action = TracksAction::RemoveCover;
                    }
                    if fetching {
                        ui.spinner();
                    }
                });
            });
            if !cover_note.is_empty() {
                ui.label(RichText::new(&cover_note).color(DIM).size(10.0));
            }
        });
        ui.separator();

        // Track table.
        ui.vertical(|ui| {
            let sel = tracks.selected;
            ui.horizontal(|ui| {
                if ui.button("Split at Cursor").on_hover_text("Split the track under the cursor there (S)").clicked() {
                    action = TracksAction::SplitAtCursor;
                }
                let has_sel = sel.is_some_and(|i| i < sheet.tracks.len());
                if ui.add_enabled(has_sel && sel != Some(0), egui::Button::new("Merge with Previous")).clicked() {
                    sheet.merge_with_previous(sel.unwrap());
                    tracks.selected = Some(sel.unwrap() - 1);
                    changed = true;
                }
                if ui.add_enabled(has_sel, egui::Button::new("Remove")).on_hover_text("Remove the track; its span becomes dead space").clicked() {
                    sheet.remove(sel.unwrap());
                    tracks.selected = None;
                    changed = true;
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Start = Cursor")).on_hover_text("Set the selected track's start to the cursor ([)").clicked() {
                    sheet.set_start(sel.unwrap(), cursor);
                    changed = true;
                }
                if ui.add_enabled(has_sel, egui::Button::new("End = Cursor")).on_hover_text("Set the selected track's end to the cursor (])").clicked() {
                    sheet.set_end(sel.unwrap(), cursor);
                    changed = true;
                }
                if ui
                    .add_enabled(has_sel, egui::Button::new("Hear Start"))
                    .on_hover_text("Play from 3 s before the selected track's start (B)")
                    .clicked()
                {
                    action = TracksAction::PlayAcross(sheet.tracks[sel.unwrap()].start);
                }
            });
            ui.add_space(2.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("track_table").num_columns(10).striped(true).spacing([6.0, 3.0]).show(ui, |ui| {
                    for h in ["", "#", "Title", "Artist", "Start", "End", "Length", "Discogs", "Diff", "Link"] {
                        ui.label(RichText::new(h).color(DIM).size(10.0));
                    }
                    ui.end_row();
                    let n = sheet.tracks.len();
                    for i in 0..n {
                        if ui.small_button("▶").on_hover_text("Play the track").clicked() {
                            tracks.selected = Some(i);
                            action = TracksAction::PlayFrom(sheet.tracks[i].start);
                        }
                        let num = RichText::new(sheet.number_label(i)).monospace().color(track_color(i)).strong();
                        if ui.selectable_label(tracks.selected == Some(i), num).on_hover_text("Go to the track's start").clicked() {
                            tracks.selected = Some(i);
                            action = TracksAction::GoTo(sheet.tracks[i].start);
                        }
                        let t = &mut sheet.tracks[i];
                        changed |= ui.add_sized([220.0, 18.0], egui::TextEdit::singleline(&mut t.title)).changed();
                        changed |= ui.add_sized([150.0, 18.0], egui::TextEdit::singleline(&mut t.artist)).changed();
                        let (mut start_s, mut end_s) = (t.start as f64 / sr as f64, t.end as f64 / sr as f64);
                        let len = t.len();
                        let source = t.source_seconds;
                        let linked = t.linked;
                        if time_box(ui, &mut start_s, sr).changed() {
                            sheet.set_start(i, (start_s * sr as f64).round() as u64);
                            changed = true;
                        }
                        if time_box(ui, &mut end_s, sr).changed() {
                            sheet.set_end(i, (end_s * sr as f64).round() as u64);
                            changed = true;
                        }
                        ui.label(RichText::new(fmt_time(len, sr)).monospace().size(11.0));
                        match source {
                            Some(d) => {
                                let diff = len as f64 / sr as f64 - d;
                                ui.label(RichText::new(fmt_time((d * sr as f64) as u64, sr)).monospace().color(DIM).size(11.0));
                                let c = if diff.abs() > 10.0 { Color32::from_rgb(251, 191, 36) } else { DIM };
                                ui.label(RichText::new(format!("{:+.1}", diff)).monospace().color(c).size(11.0));
                            }
                            None => {
                                ui.label("");
                                ui.label("");
                            }
                        }
                        if i == 0 {
                            ui.label("");
                        } else {
                            let (text, tip) = if linked {
                                ("Linked", "Starts where the previous track ends. Click to unlink.")
                            } else {
                                ("Gap", "Dead space before this track (side change, lead-in). Click to link: the previous track is extended.")
                            };
                            if ui.small_button(text).on_hover_text(tip).clicked() {
                                if linked {
                                    sheet.unlink(i);
                                } else {
                                    sheet.link(i);
                                }
                                changed = true;
                            }
                        }
                        ui.end_row();
                    }
                });
                for p in sheet.check() {
                    ui.label(RichText::new(p).color(Color32::from_rgb(248, 113, 113)).size(11.0));
                }
            });
        });
    });

    if changed {
        tracks.changed();
    }
    action
}

/// A time cell: drag to nudge (10 ms per pixel), click to type m:ss.mmm.
fn time_box(ui: &mut Ui, seconds: &mut f64, sr: u32) -> egui::Response {
    ui.add(
        egui::DragValue::new(seconds)
            .speed(0.01)
            .range(0.0..=f64::MAX)
            .custom_formatter(move |v, _| fmt_time((v * sr as f64).round() as u64, sr))
            .custom_parser(parse_time),
    )
}
