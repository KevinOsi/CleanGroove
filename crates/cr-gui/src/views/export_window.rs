//! Tracks mode: the export window. Where the album goes, how files are named, the bit depth
//! and the cover, with a preview of the names before anything is written.

use egui::{Color32, RichText};

use crate::prefs::Prefs;
use crate::tracks::TracksState;
use cr_tracks::export::{album_dir, track_paths, DEFAULT_TEMPLATE};

const DIM: Color32 = Color32::from_rgb(148, 163, 184);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);

pub enum ExportAction {
    None,
    Export,
    PrefsChanged,
}

/// `rip_bits`: the loaded file's bit depth, for "As the rip".
pub fn render_export_window(ctx: &egui::Context, tracks: &mut TracksState, prefs: &mut Prefs, rip_bits: u16, busy: bool) -> ExportAction {
    let mut action = ExportAction::None;
    let mut open = tracks.export_open;
    let mut changed = false;
    egui::Window::new("Export Tracks")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(640.0)
        .show(ctx, |ui| {
            egui::Grid::new("export_settings").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new("Folder").color(DIM));
                ui.horizontal(|ui| {
                    let shown = if prefs.export_root.is_empty() { "(choose a folder)".to_string() } else { prefs.export_root.clone() };
                    ui.label(RichText::new(shown).monospace().size(11.0));
                    if ui.button("Choose...").clicked() {
                        let mut dialog = rfd::FileDialog::new();
                        if !prefs.export_root.is_empty() {
                            dialog = dialog.set_directory(&prefs.export_root);
                        }
                        if let Some(dir) = dialog.pick_folder() {
                            prefs.export_root = dir.display().to_string();
                            changed = true;
                        }
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Names").color(DIM));
                ui.horizontal(|ui| {
                    changed |= ui.add(egui::TextEdit::singleline(&mut prefs.export_template).desired_width(420.0).font(egui::TextStyle::Monospace)).changed();
                    if ui.small_button("Default").on_hover_text(DEFAULT_TEMPLATE).clicked() {
                        prefs.export_template = DEFAULT_TEMPLATE.to_string();
                        changed = true;
                    }
                });
                ui.end_row();
                ui.label("");
                ui.label(
                    RichText::new("%albumartist% %album% %artist% %title% %track% %tracktotal% %year% %label% %catalog% %genre%   $num(%track%,2) pads to 2 digits   \\ makes a folder")
                        .color(DIM)
                        .size(10.0),
                );
                ui.end_row();

                ui.label(RichText::new("Bit depth").color(DIM));
                ui.horizontal(|ui| {
                    let as_rip = if rip_bits == 16 { 16 } else { 24 };
                    changed |= ui.radio_value(&mut prefs.export_bits, 0, format!("As the rip ({} bit)", as_rip)).changed();
                    changed |= ui.radio_value(&mut prefs.export_bits, 24, "24 bit").changed();
                    changed |= ui.radio_value(&mut prefs.export_bits, 16, "16 bit").changed();
                });
                ui.end_row();
                if prefs.export_bits == 16 && rip_bits != 16 {
                    ui.label("");
                    ui.label(RichText::new("16 bit from a 24-bit rip drops the lowest 8 bits (no dither).").color(WARN).size(10.0));
                    ui.end_row();
                }

                ui.label(RichText::new("Cover").color(DIM));
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut prefs.embed_cover, "Embed in every track").changed();
                    changed |= ui.checkbox(&mut prefs.cover_jpg, "cover.jpg in the album folder").changed();
                    if tracks.cover.is_none() {
                        ui.label(RichText::new("No cover yet: fetch or choose one in the Tracks panel").color(WARN).size(10.0));
                    }
                });
                ui.end_row();

                ui.label("");
                ui.checkbox(&mut tracks.overwrite, "Replace files that are already there");
                ui.end_row();
            });

            ui.separator();
            let opts = tracks.export_options(prefs, rip_bits);
            let plan = tracks.sheet.as_ref().map(|s| track_paths(s, &opts));
            match &plan {
                Some(Ok(paths)) => {
                    let dir = album_dir(paths);
                    ui.label(RichText::new(format!("Album folder: {}", dir.display())).size(11.0));
                    for p in paths.iter().take(3) {
                        ui.label(RichText::new(format!("    {}", p.file_name().unwrap_or_default().to_string_lossy())).monospace().size(11.0));
                    }
                    if paths.len() > 3 {
                        ui.label(RichText::new(format!("    ... {} tracks in all", paths.len())).color(DIM).size(11.0));
                    }
                    let existing = paths.iter().filter(|p| p.exists()).count();
                    if existing > 0 {
                        let text = if tracks.overwrite {
                            format!("{} of these files are already there and will be replaced", existing)
                        } else {
                            format!("{} of these files are already there: tick Replace to write them again", existing)
                        };
                        ui.label(RichText::new(text).color(WARN).size(11.0));
                    }
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e.to_string()).color(WARN));
                }
                None => {}
            }
            ui.label(
                RichText::new("Each file is decoded again after writing and checked against the rip, sample for sample. The album folder also gets the track sheet, the rip's sidecar files and a log.")
                    .color(DIM)
                    .size(10.0),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let ready = !prefs.export_root.is_empty() && matches!(plan, Some(Ok(_))) && !busy;
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new("  Export  ").strong().color(Color32::WHITE)).fill(Color32::from_rgb(16, 185, 129)))
                    .clicked()
                {
                    action = ExportAction::Export;
                }
                if let Some(dir) = tracks.last_export.clone() {
                    if ui.button("Open Album Folder").on_hover_text(dir.display().to_string()).clicked() {
                        open_folder(&dir);
                    }
                }
            });
        });
    tracks.export_open = open && tracks.export_open;
    if changed && matches!(action, ExportAction::None) {
        action = ExportAction::PrefsChanged;
    }
    action
}

/// Shows a folder in the system's file manager.
pub fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(dir).spawn();
}
