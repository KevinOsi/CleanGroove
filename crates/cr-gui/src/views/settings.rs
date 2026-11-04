//! Settings window: where takes go and how they are named, the live view and monitor volume,
//! and the online services used to look up albums. The services settings are saved (see
//! `crate::prefs`); the others are not saved between runs yet.

use egui::{Color32, RichText};

use crate::live::{LiveController, WAVE_VIEW_SECS};
use crate::prefs::{ServicesState, TokenSource};

const LABEL: Color32 = Color32::from_rgb(148, 163, 184);
const DIM: Color32 = Color32::from_rgb(100, 116, 139);

pub fn render_settings_window(ctx: &egui::Context, open: &mut bool, live: &mut LiveController, services: &mut ServicesState) {
    egui::Window::new("Settings")
        .open(open)
        .resizable(false)
        .collapsible(false)
        .default_width(520.0)
        .show(ctx, |ui| {
            ui.label(RichText::new("RECORDING").strong().color(LABEL).size(10.0));
            egui::Grid::new("settings_recording").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Folder:");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(live.folder.display().to_string()).monospace().size(11.0));
                    if ui.button("Choose...").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().set_directory(&live.folder).pick_folder() {
                            live.folder = dir;
                        }
                    }
                });
                ui.end_row();

                ui.label("Name:");
                ui.add(egui::TextEdit::singleline(&mut live.take_name).hint_text("album / record").desired_width(260.0));
                ui.end_row();

                ui.label("File name:");
                ui.add(egui::TextEdit::singleline(&mut live.template).desired_width(260.0));
                ui.end_row();

                ui.label("");
                ui.label(
                    RichText::new("{name} the name above, {n} side / take number, {date} {time} (UTC)")
                        .color(DIM)
                        .size(10.0),
                );
                ui.end_row();

                ui.label("Next take:");
                let next = live.next_take();
                ui.label(
                    RichText::new(next.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())
                        .monospace()
                        .color(Color32::from_rgb(52, 211, 153)),
                );
                ui.end_row();
            });

            ui.add_space(8.0);
            ui.label(RichText::new("LIVE MONITOR").strong().color(LABEL).size(10.0));
            egui::Grid::new("settings_live").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Waveform length:");
                egui::ComboBox::from_id_source("settings_wave_view")
                    .selected_text(format!("{} s", live.wave.view_secs))
                    .show_ui(ui, |ui| {
                        for s in WAVE_VIEW_SECS {
                            ui.selectable_value(&mut live.wave.view_secs, s, format!("{} s", s));
                        }
                    });
                ui.end_row();

                ui.label("Monitor volume:");
                let mut vol = live.monitor_volume * 100.0;
                if ui.add(egui::Slider::new(&mut vol, 0.0..=100.0).suffix(" %").fixed_decimals(0)).changed() {
                    live.set_monitor_volume(vol / 100.0);
                }
                ui.end_row();
            });

            ui.add_space(8.0);
            ui.label(RichText::new("ONLINE SERVICES (Tracks)").strong().color(LABEL).size(10.0));
            let source = services.token_source;
            egui::Grid::new("settings_services").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Discogs token:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut services.token_input)
                            .password(true)
                            .desired_width(200.0)
                            .hint_text(if source == TokenSource::None { "paste your token" } else { "paste to replace" }),
                    );
                    if ui.add_enabled(!services.token_input.trim().is_empty(), egui::Button::new("Save")).clicked() {
                        services.save_token();
                    }
                    if ui.add_enabled(source != TokenSource::None && !services.is_working(), egui::Button::new("Test")).clicked() {
                        services.test_token();
                    }
                    if ui.add_enabled(source == TokenSource::Keychain, egui::Button::new("Remove")).clicked() {
                        services.remove_token();
                    }
                });
                ui.end_row();

                ui.label("");
                let line = match source {
                    TokenSource::Keychain => "Saved in the system keychain.",
                    TokenSource::Environment => "Taken from DISCOGS_TOKEN.",
                    TokenSource::None => "None. Release numbers work without one; search and your collection need it (discogs.com > Settings > Developers > Generate new token).",
                };
                ui.label(RichText::new(line).color(DIM).size(10.0));
                ui.end_row();

                ui.label("MusicBrainz contact:");
                if ui
                    .add(egui::TextEdit::singleline(&mut services.prefs.musicbrainz_contact).desired_width(260.0).hint_text("your email or web page"))
                    .on_hover_text("MusicBrainz asks programs to say who to contact about their requests. No account is needed.")
                    .changed()
                {
                    if let Err(e) = services.prefs.save() {
                        services.status = format!("Could not save the settings: {}", e);
                    }
                }
                ui.end_row();
            });
            if !services.status.is_empty() {
                ui.label(RichText::new(&services.status).color(Color32::from_rgb(52, 211, 153)).size(11.0));
            }

            ui.add_space(8.0);
            ui.label(RichText::new("Recording and Live Monitor settings are not saved between runs yet.").color(DIM).size(10.0));
        });
}
