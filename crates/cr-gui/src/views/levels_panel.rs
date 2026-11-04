//! Live mode bottom panel: input and output meters, clip lights, monitor volume, declicker
//! activity and stream health. Takes the place of the record analysis panel.

use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::live::{LiveController, Meter, METER_FLOOR_DB};

const LABEL: Color32 = Color32::from_rgb(148, 163, 184);
const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
const DIM: Color32 = Color32::from_rgb(100, 116, 139);
const GOOD: Color32 = Color32::from_rgb(52, 211, 153);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);
const BAD: Color32 = Color32::from_rgb(248, 113, 113);
const TRACK: Color32 = Color32::from_rgb(30, 41, 59);

/// Peaks above this are getting close to clipping.
const HOT_DB: f32 = -3.0;
/// Peaks above this are in the comfortable recording range.
const GOOD_DB: f32 = -12.0;
/// Level the recording peaks should aim for.
const TARGET_DB: f32 = -6.0;

const BAR_W: f32 = 300.0;
const BAR_H: f32 = 12.0;

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).strong().color(LABEL).size(10.0));
    ui.add_space(2.0);
}

fn x_of(db: f32, rect: Rect) -> f32 {
    let f = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    rect.left() + f * rect.width()
}

fn db_text(db: f32) -> String {
    if db <= METER_FLOOR_DB {
        "-inf".to_string()
    } else {
        format!("{:.1}", db)
    }
}

/// One horizontal bar: green / amber / red zones, hold line, target tick. Returns true when
/// the clip light was clicked.
fn bar(ui: &mut Ui, label: &str, m: &Meter, c: usize, show_clip: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).monospace().color(LABEL).size(10.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(BAR_W, BAR_H), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 2.0, TRACK);

        let level = m.bar_db[c];
        let zones = [
            (METER_FLOOR_DB, GOOD_DB, Color32::from_rgb(16, 185, 129)),
            (GOOD_DB, HOT_DB, Color32::from_rgb(234, 179, 8)),
            (HOT_DB, 0.0, Color32::from_rgb(239, 68, 68)),
        ];
        for (lo, hi, color) in zones {
            if level > lo {
                let r = Rect::from_min_max(
                    Pos2::new(x_of(lo, rect), rect.top()),
                    Pos2::new(x_of(level.min(hi), rect), rect.bottom()),
                );
                p.rect_filled(r, 0.0, color);
            }
        }
        let tx = x_of(TARGET_DB, rect);
        p.line_segment([Pos2::new(tx, rect.top()), Pos2::new(tx, rect.bottom())], Stroke::new(1.0_f32, DIM));
        if m.hold_db[c] > METER_FLOOR_DB {
            let hx = x_of(m.hold_db[c], rect);
            p.line_segment([Pos2::new(hx, rect.top()), Pos2::new(hx, rect.bottom())], Stroke::new(2.0_f32, Color32::WHITE));
        }

        if show_clip {
            let (light, resp) = ui.allocate_exact_size(Vec2::new(30.0, BAR_H), Sense::click());
            let lit = m.clip[c];
            ui.painter().rect_filled(light, 2.0, if lit { BAD } else { TRACK });
            ui.painter().text(
                light.center(),
                egui::Align2::CENTER_CENTER,
                "CLIP",
                egui::FontId::monospace(8.0),
                if lit { Color32::WHITE } else { DIM },
            );
            clicked = resp
                .on_hover_text(format!("{} clipped samples since reset. Click to reset.", m.clip_count[c]))
                .clicked();
        }
        ui.label(
            RichText::new(format!("{:>5} dB", db_text(m.hold_db[c])))
                .monospace()
                .color(TEXT)
                .size(10.0),
        );
    });
    clicked
}

/// Level advice from the highest peak since reset.
fn level_advice(max_db: f32) -> (String, Color32) {
    if max_db <= METER_FLOOR_DB {
        return ("Play the loudest track to check the level".to_string(), DIM);
    }
    let d = TARGET_DB - max_db;
    if max_db >= -0.1 {
        ("Clipping: turn the input gain down".to_string(), BAD)
    } else if d < -1.5 {
        (format!("Hot: about {:.0} dB less gain for {:.0} dB peaks", -d, TARGET_DB), WARN)
    } else if d > 6.0 {
        (format!("Quiet: about {:.0} dB more gain would help", d), WARN)
    } else {
        ("Level is good".to_string(), GOOD)
    }
}

pub fn render_levels_panel(ui: &mut Ui, live: &mut LiveController) {
    let running = live.is_running();
    ui.horizontal_top(|ui| {
        // Input meters
        ui.vertical(|ui| {
            heading(ui, "INPUT LEVEL (raw capture, before declick)");
            let mut reset = false;
            reset |= bar(ui, "L", &live.input_meter, 0, true);
            reset |= bar(ui, "R", &live.input_meter, 1, true);
            if reset {
                live.input_meter.reset();
            }
            let max = live.input_meter.max_db[0].max(live.input_meter.max_db[1]);
            let (advice, color) = level_advice(max);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Max {} dBFS", db_text(max))).monospace().color(TEXT).size(10.0));
                ui.label(RichText::new(advice).color(color).size(10.0));
                if ui.small_button("Reset").on_hover_text("Clear clip lights and the maximum").clicked() {
                    live.input_meter.reset();
                }
            });
            if live.mono_check.is_mono() {
                ui.label(RichText::new("Input is MONO: L and R are identical copies").strong().color(BAD).size(10.0))
                    .on_hover_text(format!(
                        "The device delivers one channel and the system copies it to both sides, so a \
                         stereo record is captured as mono.\nFix: set the input to 2 channels in {}. \
                         Then Stop and Start Monitoring.",
                        crate::live::DEVICE_FORMAT_HINT
                    ));
            } else {
                ui.label(
                    RichText::new("The needle drop alone can clip: press Reset once the needle is in the groove.")
                        .color(DIM)
                        .size(9.5),
                );
            }
        });

        ui.separator();

        // Monitor output and volume
        ui.vertical(|ui| {
            heading(ui, "MONITOR OUTPUT (after declick)");
            bar(ui, "L", &live.output_meter, 0, false);
            bar(ui, "R", &live.output_meter, 1, false);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Volume").color(LABEL).size(10.0));
                let mut vol = live.monitor_volume * 100.0;
                if ui
                    .add(egui::Slider::new(&mut vol, 0.0..=100.0).suffix(" %").fixed_decimals(0))
                    .on_hover_text("What you hear only; the recording is never changed")
                    .changed()
                {
                    live.set_monitor_volume(vol / 100.0);
                }
            });
        });

        ui.separator();

        // Declicker and stream health
        ui.vertical(|ui| {
            heading(ui, "DECLICKER");
            let t = &live.telemetry;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Activity").color(LABEL).size(10.0));
                let (rect, _) = ui.allocate_exact_size(Vec2::new(110.0, BAR_H), Sense::hover());
                ui.painter().rect_filled(rect, 2.0, TRACK);
                let f = if running { t.click_activity.clamp(0.0, 1.0) as f32 } else { 0.0 };
                if f > 0.0 {
                    let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * f, rect.height()));
                    ui.painter().rect_filled(fill, 2.0, Color32::from_rgb(129, 140, 248));
                }
            });
            ui.label(
                RichText::new(format!("Repaired samples in window: L {}  R {}", t.active_l_fixes, t.active_r_fixes))
                    .monospace()
                    .color(TEXT)
                    .size(10.0),
            );
            if running {
                ui.label(RichText::new(format!("Latency {:.0} ms", live.latency_ms)).monospace().color(TEXT).size(10.0));
            }

            ui.add_space(4.0);
            heading(ui, "STREAM HEALTH");
            let c = &live.counters;
            let lost = c.dropped_input + c.dropped_tape;
            let color = if lost > 0 { BAD } else if c.underruns > 0 { WARN } else { GOOD };
            ui.label(
                RichText::new(format!(
                    "Lost: {} in / {} rec   Dropouts: {}",
                    c.dropped_input, c.dropped_tape, c.underruns
                ))
                .monospace()
                .color(color)
                .size(10.0),
            )
            .on_hover_text(
                "Lost in: input frames the declicker could not keep up with (monitor only).\n\
                 Lost rec: frames missing from recordings (should always be 0).\n\
                 Dropouts: times the monitor output ran dry.",
            );
        });
    });
}
