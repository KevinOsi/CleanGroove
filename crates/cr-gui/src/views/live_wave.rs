//! Live mode views: the take timeline strip and the scrolling L/R waveform with repair pins,
//! held-back clicks and clipping. Same colours and markers as the File mode waveform.

use cr_dsp::params::RtSoundMode;
use cr_rt::live::{WaveColumn, COLUMNS_PER_SEC};
use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};

use crate::live::{LiveWave, TakeTimeline};

const BG: Color32 = Color32::from_rgb(11, 15, 25);
const BORDER: Color32 = Color32::from_rgb(51, 65, 85);
const GRID: Color32 = Color32::from_rgb(30, 41, 59);
const TEXT: Color32 = Color32::from_rgb(203, 213, 225);
const DIM: Color32 = Color32::from_rgb(100, 116, 139);
/// File mode's semi-transparent purple (129, 140, 248 at alpha 140), premultiplied.
const ORIG: Color32 = Color32::from_rgba_premultiplied(71, 77, 136, 140);
const REPAIRED: Color32 = Color32::from_rgb(16, 185, 129);
const BYPASS: Color32 = Color32::from_rgb(92, 107, 192);
const DELTA: Color32 = Color32::from_rgb(239, 68, 68);
const HELD: Color32 = Color32::from_rgb(251, 191, 36);
const CLIP: Color32 = Color32::from_rgb(248, 113, 113);

/// Columns merged into one 2-pixel bar.
#[derive(Default)]
struct Bar {
    orig: [f32; 2],
    out: [f32; 2],
    mode: Option<RtSoundMode>,
    repaired: [u32; 2],
    held: [u32; 2],
    clipped: [u32; 2],
}

impl Bar {
    fn add(&mut self, c: &WaveColumn) {
        for ch in 0..2 {
            self.orig[ch] = self.orig[ch].max(c.orig_peak[ch]);
            self.out[ch] = self.out[ch].max(c.out_peak[ch]);
            self.repaired[ch] += c.repaired[ch] as u32;
            self.held[ch] += c.protected[ch] as u32;
            self.clipped[ch] += c.clipped[ch] as u32;
        }
        self.mode = Some(c.mode);
    }
}

fn vbar(p: &egui::Painter, x: f32, center: f32, h: f32, color: Color32) {
    if h > 0.5 {
        p.rect_filled(
            Rect::from_min_max(Pos2::new(x, center - h), Pos2::new(x + 1.6, center + h)),
            0.0,
            color,
        );
    }
}

/// The scrolling waveform: newest audio on the right edge, `view_secs` across.
pub fn render_live_waveform(ui: &mut Ui, wave: &LiveWave, height: f32) {
    let width = ui.available_width();
    let ruler_h = 16.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, BG);
    p.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, BORDER));

    let canvas = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - ruler_h));
    let channel_h = (canvas.height() - 4.0) / 2.0;
    let chans = [
        Rect::from_min_size(canvas.min, Vec2::new(width, channel_h)),
        Rect::from_min_size(Pos2::new(canvas.left(), canvas.top() + channel_h + 4.0), Vec2::new(width, channel_h)),
    ];
    p.line_segment(
        [Pos2::new(canvas.left(), canvas.top() + channel_h + 2.0), Pos2::new(canvas.right(), canvas.top() + channel_h + 2.0)],
        Stroke::new(2.0_f32, GRID),
    );
    for (r, name) in chans.iter().zip(["LEFT", "RIGHT"]) {
        p.line_segment([Pos2::new(r.left(), r.center().y), Pos2::new(r.right(), r.center().y)], Stroke::new(1.0_f32, GRID));
        p.rect_filled(
            Rect::from_min_size(Pos2::new(r.left() + 8.0, r.top() + 6.0), Vec2::new(46.0, 18.0)),
            2.0,
            Color32::from_rgba_unmultiplied(15, 23, 42, 220),
        );
        p.text(Pos2::new(r.left() + 12.0, r.top() + 8.0), egui::Align2::LEFT_TOP, name, egui::FontId::monospace(10.0), TEXT);
    }

    // Time ruler: seconds before now.
    let view_cols = (wave.view_secs * COLUMNS_PER_SEC) as f32;
    let ruler_y = rect.bottom() - ruler_h / 2.0;
    let step = match wave.view_secs {
        0..=5 => 1,
        6..=10 => 2,
        11..=30 => 5,
        _ => 10,
    };
    for s in (0..=wave.view_secs).step_by(step as usize) {
        let x = rect.right() - (s as f32 * COLUMNS_PER_SEC as f32 / view_cols) * width;
        p.line_segment([Pos2::new(x, canvas.bottom()), Pos2::new(x, canvas.bottom() + 4.0)], Stroke::new(1.0_f32, DIM));
        let label = if s == 0 { "now".to_string() } else { format!("-{} s", s) };
        // The end labels sit inside the canvas.
        let (align, dx) = if s == 0 {
            (egui::Align2::RIGHT_CENTER, -4.0)
        } else if s == wave.view_secs {
            (egui::Align2::LEFT_CENTER, 4.0)
        } else {
            (egui::Align2::CENTER_CENTER, 0.0)
        };
        p.text(Pos2::new(x + dx, ruler_y), align, label, egui::FontId::monospace(9.0), DIM);
    }

    let Some((start, end)) = wave.view_range() else {
        p.text(canvas.center(), egui::Align2::CENTER_CENTER, "Waiting for audio...", egui::FontId::proportional(13.0), DIM);
        return;
    };

    // Merge columns into 2-pixel bars, newest bar at the right edge.
    let n_bars = (width / 2.0).floor().max(1.0) as usize;
    let cols_per_bar = view_cols / n_bars as f32;
    let mut bars: Vec<Bar> = (0..n_bars).map(|_| Bar::default()).collect();
    for c in wave.slice(start, end) {
        let age = (end - c.index) as f32; // 0 = newest
        let from_right = (age / cols_per_bar) as usize;
        if from_right < n_bars {
            bars[n_bars - 1 - from_right].add(c);
        }
    }

    let half_h = channel_h * 0.44;
    for (i, b) in bars.iter().enumerate() {
        let Some(mode) = b.mode else { continue };
        let x = rect.left() + i as f32 * 2.0;
        for ch in 0..2 {
            let r = &chans[ch];
            let cy = r.center().y;
            match mode {
                RtSoundMode::SoundOut => {
                    vbar(&p, x, cy, (b.orig[ch] * half_h).min(half_h), ORIG);
                    vbar(&p, x, cy, (b.out[ch] * half_h).min(half_h), REPAIRED);
                }
                RtSoundMode::SoundIn => vbar(&p, x, cy, (b.orig[ch] * half_h).min(half_h), BYPASS),
                RtSoundMode::Noise => vbar(&p, x, cy, (b.out[ch] * half_h * 2.5).min(half_h), DELTA),
            }
            if b.repaired[ch] > 0 {
                // Repair pin on the channel's top edge, as in File mode.
                p.rect_filled(Rect::from_min_size(Pos2::new(x - 0.5, r.top()), Vec2::new(3.0, 6.0)), 0.0, REPAIRED);
            }
            if b.held[ch] > 0 {
                // A click found but left alone (pitch protection): amber pin on the bottom edge.
                p.add(egui::Shape::convex_polygon(
                    vec![Pos2::new(x + 1.0, r.bottom() - 8.0), Pos2::new(x - 2.5, r.bottom()), Pos2::new(x + 4.5, r.bottom())],
                    HELD,
                    Stroke::NONE,
                ));
            }
            if b.clipped[ch] > 0 {
                p.rect_filled(Rect::from_min_size(Pos2::new(x, r.top() + 7.0), Vec2::new(2.0, 3.0)), 0.0, CLIP);
                p.rect_filled(Rect::from_min_size(Pos2::new(x, r.bottom() - 11.0), Vec2::new(2.0, 3.0)), 0.0, CLIP);
            }
        }
    }

    // Legend, top right.
    let legend = Rect::from_min_size(Pos2::new(rect.right() - 392.0, rect.top() + 6.0), Vec2::new(384.0, 18.0));
    p.rect_filled(legend, 3.0, Color32::from_rgba_unmultiplied(15, 23, 42, 230));
    p.rect_stroke(legend, 3.0, Stroke::new(1.0_f32, BORDER));
    let mode = bars.iter().rev().find_map(|b| b.mode).unwrap_or(RtSoundMode::SoundOut);
    let traces: Vec<(Color32, &str)> = match mode {
        RtSoundMode::SoundOut => vec![(Color32::from_rgb(129, 140, 248), "Original"), (REPAIRED, "Output")],
        RtSoundMode::SoundIn => vec![(BYPASS, "Bypass: raw input")],
        RtSoundMode::Noise => vec![(DELTA, "Removed clicks")],
    };
    let mut x = legend.left() + 8.0;
    let y = legend.center().y;
    let item = |p: &egui::Painter, x: &mut f32, text: &str| {
        p.text(Pos2::new(*x + 7.0, y), egui::Align2::LEFT_CENTER, text, egui::FontId::monospace(9.0), TEXT);
        *x += 14.0 + text.len() as f32 * 5.6;
    };
    for (color, text) in traces {
        p.circle_filled(Pos2::new(x, y), 3.5, color);
        item(&p, &mut x, text);
    }
    // Marker shapes as drawn on the waveform.
    p.rect_filled(Rect::from_min_size(Pos2::new(x - 1.5, y - 5.0), Vec2::new(3.0, 6.0)), 0.0, REPAIRED);
    item(&p, &mut x, "Repair");
    p.add(egui::Shape::convex_polygon(
        vec![Pos2::new(x, y - 4.0), Pos2::new(x - 3.5, y + 3.0), Pos2::new(x + 3.5, y + 3.0)],
        HELD,
        Stroke::NONE,
    ));
    item(&p, &mut x, "Held back");
    p.rect_filled(Rect::from_min_size(Pos2::new(x - 1.0, y - 1.5), Vec2::new(2.0, 3.0)), 0.0, CLIP);
    item(&p, &mut x, "Input clip");

    if wave.frozen_at.is_some() {
        p.text(
            Pos2::new(rect.left() + 64.0, rect.top() + 15.0),
            egui::Align2::LEFT_CENTER,
            "FROZEN",
            egui::FontId::monospace(10.0),
            HELD,
        );
    }

    // Hover: what is under the pointer.
    if let Some(pos) = response.hover_pos() {
        let from_right = ((rect.right() - pos.x) / 2.0).floor() as usize;
        if from_right < n_bars {
            let b = &bars[n_bars - 1 - from_right];
            if b.mode.is_some() {
                let secs = (from_right as f32 * cols_per_bar) / COLUMNS_PER_SEC as f32;
                let tip = format!(
                    "-{:.2} s\nL peak {:.3}, repaired {} samples{}{}\nR peak {:.3}, repaired {} samples{}{}",
                    secs,
                    b.orig[0],
                    b.repaired[0],
                    if b.held[0] > 0 { format!(", {} held back", b.held[0]) } else { String::new() },
                    if b.clipped[0] > 0 { format!(", {} clipped", b.clipped[0]) } else { String::new() },
                    b.orig[1],
                    b.repaired[1],
                    if b.held[1] > 0 { format!(", {} held back", b.held[1]) } else { String::new() },
                    if b.clipped[1] > 0 { format!(", {} clipped", b.clipped[1]) } else { String::new() },
                );
                p.line_segment([Pos2::new(pos.x, canvas.top()), Pos2::new(pos.x, canvas.bottom())], Stroke::new(1.0_f32, DIM));
                response.on_hover_text_at_pointer(tip);
            }
        }
    }
}

/// The current take at a glance: repair density per second, clipping, pause joins.
pub fn render_take_timeline(ui: &mut Ui, timeline: &TakeTimeline, recording: bool) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 34.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, Color32::from_rgb(15, 23, 42));
    p.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, BORDER));

    let secs = timeline.seconds.len();
    if secs == 0 {
        let text = if recording { "Recording..." } else { "TAKE TIMELINE (press Rec to start a take)" };
        p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::monospace(10.0), Color32::from_rgb(71, 85, 105));
        return;
    }

    // At least a minute across, so the first seconds of a take do not fill the strip.
    let span = (secs as f32).max(60.0);
    let sec_w = (width - 8.0) / span;
    let x0 = rect.left() + 4.0;
    let inner = rect.shrink2(Vec2::new(0.0, 4.0));
    for (i, s) in timeline.seconds.iter().enumerate() {
        let x = x0 + i as f32 * sec_w;
        let cell = Rect::from_min_max(Pos2::new(x, inner.top()), Pos2::new(x + sec_w.max(1.0), inner.bottom()));
        // Repair density: share of 10 ms columns with a repair (same palette as File mode heat).
        let d = (s.repaired as f32 / COLUMNS_PER_SEC as f32).clamp(0.0, 1.0);
        let color = if d == 0.0 {
            Color32::from_rgba_unmultiplied(71, 85, 105, 60)
        } else {
            let alpha = (60.0 + 195.0 * (d * 4.0).min(1.0)) as u8;
            if d < 0.05 {
                Color32::from_rgba_unmultiplied(14, 165, 233, alpha)
            } else if d < 0.15 {
                Color32::from_rgba_unmultiplied(245, 158, 11, alpha)
            } else {
                Color32::from_rgba_unmultiplied(239, 68, 68, alpha)
            }
        };
        p.rect_filled(cell, 0.0, color);
        if s.clipped > 0 {
            p.rect_filled(Rect::from_min_size(Pos2::new(x, inner.top()), Vec2::new(sec_w.max(2.0), 3.0)), 0.0, CLIP);
        }
    }
    for &t in &timeline.pauses {
        let x = x0 + t as f32 * sec_w;
        p.line_segment([Pos2::new(x, rect.top() + 2.0), Pos2::new(x, rect.bottom() - 2.0)], Stroke::new(2.0_f32, HELD));
    }
    let len = timeline.length_secs();
    p.text(
        Pos2::new(rect.right() - 8.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        format!("{:02}:{:02}  |  {} pause(s)", (len / 60.0) as u64, (len % 60.0) as u64, timeline.pauses.len()),
        egui::FontId::monospace(10.0),
        TEXT,
    );

    if let Some(pos) = response.hover_pos() {
        let i = ((pos.x - x0) / sec_w).floor();
        if i >= 0.0 && (i as usize) < secs {
            let s = timeline.seconds[i as usize];
            response.on_hover_text_at_pointer(format!(
                "{:02}:{:02}\n{} of 100 x 10 ms with a repair{}{}",
                i as u64 / 60,
                i as u64 % 60,
                s.repaired,
                if s.protected > 0 { format!(", {} held back", s.protected) } else { String::new() },
                if s.clipped > 0 { format!(", {} clipped", s.clipped) } else { String::new() },
            ));
        }
    }
}
