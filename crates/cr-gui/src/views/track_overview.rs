//! Whole-album overview for Tracks mode: the waveform outline with a colour band per track, so
//! the transitions are easy to find, and the zoomed view's window. Click or drag to move the
//! view.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::state::TrackData;
use crate::views::track_lane::track_color;
use cr_tracks::TrackSheet;

const HEIGHT: f32 = 54.0;

pub fn render_track_overview(
    ui: &mut Ui,
    track: Option<&TrackData>,
    sheet: Option<&TrackSheet>,
    selected: Option<usize>,
    view_start: &mut usize,
    view_end: &mut usize,
) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(15, 23, 42));
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));
    let Some(track) = track.filter(|t| t.total_frames > 0) else {
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No file loaded", egui::FontId::proportional(11.0), Color32::from_rgb(100, 116, 139));
        return;
    };
    let total = track.total_frames as f64;
    let x_of = |f: f64| rect.left() + (f / total * width as f64) as f32;

    // Track bands, numbered.
    if let Some(sheet) = sheet {
        for (i, t) in sheet.tracks.iter().enumerate() {
            let (x0, x1) = (x_of(t.start as f64), x_of(t.end as f64));
            let c = track_color(i);
            let alpha = if selected == Some(i) { 110 } else { 55 };
            let band = Rect::from_min_max(Pos2::new(x0, rect.top() + 1.0), Pos2::new(x1, rect.bottom() - 1.0));
            painter.rect_filled(band, 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha));
            painter.line_segment([Pos2::new(x0, rect.top()), Pos2::new(x0, rect.bottom())], Stroke::new(1.0_f32, c));
            if x1 - x0 > 16.0 {
                painter.text(Pos2::new(x0 + 3.0, rect.top() + 2.0), egui::Align2::LEFT_TOP, sheet.number_label(i), egui::FontId::monospace(9.0), Color32::WHITE);
            }
        }
    }

    // Waveform outline from the overview buckets (louder channel).
    let n = track.overview_min_max_l.len().min(track.overview_min_max_r.len());
    if n > 0 {
        let mid = rect.center().y;
        let half = HEIGHT / 2.0 - 4.0;
        let cols = width.max(1.0) as usize;
        for c in 0..cols {
            let b0 = c * n / cols;
            let b1 = ((c + 1) * n / cols).max(b0 + 1).min(n);
            let peak = (b0..b1)
                .map(|b| {
                    let (l, r) = (track.overview_min_max_l[b], track.overview_min_max_r[b]);
                    l.0.abs().max(l.1.abs()).max(r.0.abs()).max(r.1.abs())
                })
                .fold(0.0f32, f32::max);
            let h = (peak * half).min(half);
            let x = rect.left() + c as f32;
            painter.line_segment([Pos2::new(x, mid - h), Pos2::new(x, mid + h)], Stroke::new(1.0_f32, Color32::from_rgb(203, 213, 225)));
        }
    }

    // The zoomed view's window.
    let w = Rect::from_min_max(Pos2::new(x_of(*view_start as f64), rect.top()), Pos2::new(x_of(*view_end as f64).max(x_of(*view_start as f64) + 2.0), rect.bottom()));
    painter.rect_filled(w, 0.0, Color32::from_rgba_unmultiplied(250, 204, 21, 30));
    painter.rect_stroke(w, 0.0, Stroke::new(1.0_f32, Color32::from_rgb(250, 204, 21)));

    if response.clicked() || response.dragged() {
        if let Some(p) = response.interact_pointer_pos() {
            let len = view_end.saturating_sub(*view_start).max(1);
            let centre = ((p.x - rect.left()) as f64 / width as f64 * total).clamp(0.0, total) as usize;
            *view_start = centre.saturating_sub(len / 2).min(track.total_frames.saturating_sub(len));
            *view_end = (*view_start + len).min(track.total_frames);
        }
    }
}
