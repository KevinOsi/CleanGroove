//! Help menu windows: the user guide (docs/USER_GUIDE.md, compiled in) and About.

use egui::{Color32, RichText, Ui};

const GUIDE: &str = include_str!("../../../../docs/USER_GUIDE.md");

const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
const LABEL: Color32 = Color32::from_rgb(148, 163, 184);
const ACCENT: Color32 = Color32::from_rgb(165, 180, 252);

/// One section of the guide: a "## " heading and the lines under it.
struct Section<'a> {
    title: &'a str,
    lines: Vec<&'a str>,
}

/// Splits the guide into its title, the text before the first section, and the sections.
fn parse(md: &str) -> (&str, Vec<&str>, Vec<Section<'_>>) {
    let mut title = "";
    let mut intro = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    for line in md.lines() {
        if let Some(t) = line.strip_prefix("# ") {
            title = t;
        } else if let Some(t) = line.strip_prefix("## ") {
            sections.push(Section { title: t, lines: Vec::new() });
        } else if let Some(s) = sections.last_mut() {
            s.lines.push(line);
        } else {
            intro.push(line);
        }
    }
    (title, intro, sections)
}

/// Renders the small Markdown subset the guide uses: paragraphs, "- " bullets, "1. " steps.
fn body(ui: &mut Ui, lines: &[&str]) {
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            ui.add_space(4.0);
        } else if let Some(item) = line.strip_prefix("- ") {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("  •").color(ACCENT));
                ui.label(RichText::new(item).color(TEXT));
            });
        } else if let Some((num, item)) = line.split_once(". ").filter(|(n, _)| n.parse::<u32>().is_ok()) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("  {}.", num)).color(ACCENT).monospace());
                ui.label(RichText::new(item).color(TEXT));
            });
        } else {
            ui.label(RichText::new(line).color(TEXT));
        }
    }
}

pub fn render_user_guide(ctx: &egui::Context, open: &mut bool) {
    let (title, intro, sections) = parse(GUIDE);
    egui::Window::new(title)
        .open(open)
        .default_size([620.0, 560.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(600.0);
                body(ui, &intro);
                ui.add_space(6.0);
                for (i, s) in sections.iter().enumerate() {
                    egui::CollapsingHeader::new(RichText::new(s.title).strong().color(ACCENT))
                        .default_open(i == 0)
                        .show(ui, |ui| body(ui, &s.lines));
                }
                ui.add_space(8.0);
                ui.label(RichText::new("This guide is docs/USER_GUIDE.md in the source.").color(LABEL).size(10.0));
            });
        });
}

pub fn render_about(ctx: &egui::Context, open: &mut bool, icon: Option<&egui::TextureHandle>) {
    egui::Window::new("About CleanGroove")
        .open(open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(tex) = icon {
                    ui.image((tex.id(), egui::vec2(96.0, 96.0)));
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new("CleanGroove").strong().size(20.0).color(Color32::WHITE));
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(LABEL));
                    ui.add_space(4.0);
                    ui.label(RichText::new("Vinyl restoration workstation: offline declick and decrackle, real-time monitoring and raw capture.").color(TEXT));
                });
            });
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "An independent reimplementation of the ClickRepair and ClickRepairRT algorithms, \
                     written from functional specifications.",
                )
                .color(LABEL)
                .size(11.0),
            );
            ui.label(RichText::new(format!("License: {}", env!("CARGO_PKG_LICENSE"))).color(LABEL).size(11.0));
            ui.add_space(4.0);
            ui.label(RichText::new("Help > User Guide (F1) explains all three modes.").color(LABEL).size(11.0));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_parses_into_sections() {
        let (title, intro, sections) = parse(GUIDE);
        assert_eq!(title, "CleanGroove User Guide");
        assert!(intro.iter().any(|l| !l.trim().is_empty()));
        let names: Vec<&str> = sections.iter().map(|s| s.title).collect();
        for want in ["Three modes", "Tracks: splitting an album", "Live Monitor: recording a record", "Keyboard shortcuts", "Troubleshooting"] {
            assert!(names.contains(&want), "missing section {}: {:?}", want, names);
        }
        assert!(sections.iter().all(|s| s.lines.iter().any(|l| !l.trim().is_empty())));
    }
}
