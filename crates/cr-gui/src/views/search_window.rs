//! Tracks mode: the window that finds the release a rip is of, in the user's Discogs
//! collection, the Discogs database and MusicBrainz.

use egui::{Color32, RichText};

use crate::tracks::SearchState;
use cr_tracks::providers::{Candidate, Source};

const DIM: Color32 = Color32::from_rgb(148, 163, 184);

pub enum SearchAction {
    None,
    Search,
    Use(Candidate),
}

pub fn render_search_window(ctx: &egui::Context, s: &mut SearchState, has_token: bool) -> SearchAction {
    let mut action = SearchAction::None;
    let mut open = s.open;
    egui::Window::new("Find the Release")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([900.0, 460.0])
        .show(ctx, |ui| {
            let mut enter = false;
            egui::Grid::new("search_fields").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                for (label, value, hint) in [
                    ("Artist", &mut s.query.artist, ""),
                    ("Album", &mut s.query.title, ""),
                    ("Other", &mut s.query.text, "catalogue number, barcode or other words"),
                ] {
                    ui.label(RichText::new(label).color(DIM));
                    let r = ui.add(egui::TextEdit::singleline(value).desired_width(360.0).hint_text(hint));
                    enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Search in:").color(DIM));
                let need_token = "Needs a Discogs token (Settings > Online services)";
                ui.add_enabled(has_token, egui::Checkbox::new(&mut s.use_collection, "My Discogs collection"))
                    .on_hover_text("Your own records first. Empty fields list the whole collection.")
                    .on_disabled_hover_text(need_token);
                ui.add_enabled(has_token, egui::Checkbox::new(&mut s.use_discogs, "Discogs")).on_disabled_hover_text(need_token);
                ui.checkbox(&mut s.use_musicbrainz, "MusicBrainz");
                ui.separator();
                let search = ui.add_enabled(!s.is_searching(), egui::Button::new(RichText::new("  Search  ").strong()));
                if search.clicked() || (enter && !s.is_searching()) {
                    action = SearchAction::Search;
                }
                if s.is_searching() {
                    ui.spinner();
                }
                ui.label(RichText::new(&s.status).color(DIM).size(11.0));
                if s.has_collection() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Reload collection").on_hover_text("Download your Discogs collection again on the next search").clicked() {
                            s.refresh_collection();
                        }
                    });
                }
            });
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("search_results").num_columns(9).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                    for h in ["", "Source", "Artist", "Album", "Year", "Label", "Catalog #", "Format", "Country"] {
                        ui.label(RichText::new(h).color(DIM).size(10.0));
                    }
                    ui.end_row();
                    for c in &s.results {
                        if ui.button("Use").on_hover_text("Fill in the album and place its tracks (replaces the current tracks)").clicked() {
                            action = SearchAction::Use(c.clone());
                        }
                        let (name, color) = match c.source {
                            Some(Source::Collection) => ("Mine", Color32::from_rgb(52, 211, 153)),
                            Some(Source::Discogs) => ("Discogs", Color32::from_rgb(203, 213, 225)),
                            Some(Source::MusicBrainz) => ("MusicBrainz", Color32::from_rgb(196, 181, 253)),
                            None => ("", DIM),
                        };
                        ui.label(RichText::new(name).color(color).size(11.0));
                        ui.label(&c.artist);
                        ui.label(&c.title);
                        ui.label(c.year.map(|y| y.to_string()).unwrap_or_default());
                        ui.label(&c.label);
                        ui.label(RichText::new(&c.catno).monospace().size(11.0));
                        ui.label(RichText::new(&c.format).size(11.0));
                        ui.label(&c.country);
                        ui.end_row();
                    }
                });
            });
        });
    s.open = open && s.open;
    action
}
