use cr_dsp::analysis::AlbumAnalysis;
use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use crate::state::TrackData;

/// Renders the 30-second segmentized timeline mini-map with the album analysis' exposed-click
/// heatmap, silence between tracks and problem-spot markers.
pub fn render_timeline_mini_map(
    ui: &mut Ui,
    track_opt: Option<&TrackData>,
    view_start_frame: &mut usize,
    view_end_frame: &mut usize,
    heatmap: &[f32],
    analysis: Option<&AlbumAnalysis>,
) {
    let available_width = ui.available_width();
    let height = 36.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(available_width, height), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // Background track fill
    painter.rect_filled(rect, 4.0, Color32::from_rgb(15, 23, 42)); // slate-900
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85))); // slate-700

    let track = match track_opt {
        Some(t) if t.total_frames > 0 => t,
        _ => {
            // Empty state display
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "TIMELINE OVERVIEW (NO AUDIO LOADED)",
                egui::FontId::monospace(10.0),
                Color32::from_rgb(71, 85, 105),
            );
            return;
        }
    };

    let total_frames = track.total_frames.max(1);
    let sample_rate = track.sample_rate.max(1) as f32;
    let total_seconds = total_frames as f32 / sample_rate;

    // 1. Draw 30-second segment markers
    let segment_dur = 30.0f32;
    let num_segments = (total_seconds / segment_dur).ceil() as usize;
    for i in 0..=num_segments {
        let sec = i as f32 * segment_dur;
        if sec > total_seconds {
            break;
        }
        let norm = sec / total_seconds;
        let x = rect.left() + norm * rect.width();

        // Vertical divider
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(71, 85, 105, 80)),
        );

        // Timestamp label
        let mins = (sec / 60.0).floor() as u32;
        let secs = (sec % 60.0).floor() as u32;
        let label = format!("{}:{:02}", mins, secs);
        painter.text(
            Pos2::new(x + 3.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::monospace(9.0),
            Color32::from_rgb(100, 116, 139),
        );
    }

    // 2. Draw Thermal Click Density Heatmap (BEHIND the waveform silhouette, O(1) zero-allocation)
    // Silence between tracks, shaded so track boundaries are visible.
    if let Some(a) = analysis {
        let n = a.block_db.len().max(1) as f32;
        for &b in &a.gap_blocks {
            let x1 = rect.left() + (b as f32 / n) * rect.width();
            let x2 = rect.left() + ((b + 1) as f32 / n) * rect.width();
            painter.rect_filled(
                Rect::from_x_y_ranges(x1..=x2.max(x1 + 1.0), rect.y_range()),
                0.0,
                Color32::from_rgba_unmultiplied(0, 0, 0, 110),
            );
        }
    }

    if !heatmap.is_empty() {
        let num_bins = heatmap.len();
        let bin_width = rect.width() / (num_bins as f32);
        for (b, &ratio) in heatmap.iter().enumerate() {
            if ratio > 0.0 {
                let bx1 = rect.left() + (b as f32) * bin_width;
                let bx2 = bx1 + bin_width + 0.5;
                let bin_rect = Rect::from_x_y_ranges(bx1..=bx2, (rect.top() + 2.0)..=(rect.bottom() - 2.0));

                // Thermal heat gradient:
                // Cool teal -> warm amber -> glowing hot crimson
                let heat_color = if ratio < 0.22 {
                    let alpha = (ratio / 0.22 * 170.0).clamp(50.0, 170.0) as u8;
                    Color32::from_rgba_unmultiplied(14, 165, 233, alpha)
                } else if ratio < 0.58 {
                    let t = (ratio - 0.22) / 0.36;
                    let alpha = (170.0 + t * 50.0) as u8;
                    Color32::from_rgba_unmultiplied(245, 158, 11, alpha)
                } else {
                    let t = (ratio - 0.58) / 0.42;
                    let alpha = (210.0 + t * 45.0) as u8;
                    Color32::from_rgba_unmultiplied(239, 68, 68, alpha)
                };

                painter.rect_filled(bin_rect, 0.0, heat_color);
            }
        }
    }

    // 3. Draw downsampled waveform silhouette (OVER the thermal heatmap)
    let mid_y = rect.center().y;
    let overview = &track.overview_min_max_l;
    if !overview.is_empty() {
        let bucket_count = overview.len();
        let half_h = (rect.height() * 0.38) as f32;

        for (i, &(min_val, max_val)) in overview.iter().enumerate() {
            let x = rect.left() + (i as f32 / bucket_count as f32) * rect.width();
            let y1 = mid_y - (max_val.abs() * half_h).min(half_h);
            let y2 = mid_y + (min_val.abs() * half_h).min(half_h);

            painter.line_segment(
                [Pos2::new(x, y1), Pos2::new(x, y2)],
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(199, 210, 254, 210)),
            );
        }
    }

    // Problem spots: small triangles on the bottom edge.
    if let Some(a) = analysis {
        for spot in &a.problem_spots {
            let x = rect.left() + (spot.start_frame as f32 / total_frames as f32) * rect.width();
            let color = if spot.near_silence {
                Color32::from_rgb(148, 163, 184)
            } else {
                Color32::from_rgb(251, 191, 36)
            };
            painter.add(egui::Shape::convex_polygon(
                vec![Pos2::new(x, rect.bottom() - 7.0), Pos2::new(x - 4.0, rect.bottom()), Pos2::new(x + 4.0, rect.bottom())],
                color,
                Stroke::NONE,
            ));
        }
    }

    // 4. Viewport Zoom Window
    let cur_v_start = *view_start_frame;
    let cur_v_end = (*view_end_frame).max(cur_v_start + 1);
    let left_norm = (cur_v_start as f32 / total_frames as f32).clamp(0.0, 1.0);
    let right_norm = (cur_v_end as f32 / total_frames as f32).clamp(0.0, 1.0);

    let vp_x1 = rect.left() + left_norm * rect.width();
    let vp_x2 = (rect.left() + right_norm * rect.width()).max(vp_x1 + 18.0);
    let vp_rect = Rect::from_x_y_ranges(vp_x1..=vp_x2, rect.y_range());

    // Highlight inside viewport
    painter.rect_filled(vp_rect, 2.0, Color32::from_rgba_unmultiplied(99, 102, 241, 40));
    painter.rect_stroke(vp_rect, 2.0, Stroke::new(1.8_f32, Color32::from_rgb(129, 140, 248)));

    // Viewport drag handle lines
    painter.line_segment(
        [Pos2::new(vp_rect.left() + 2.0, vp_rect.center().y - 6.0), Pos2::new(vp_rect.left() + 2.0, vp_rect.center().y + 6.0)],
        Stroke::new(1.5_f32, Color32::from_rgb(165, 180, 252)),
    );
    painter.line_segment(
        [Pos2::new(vp_rect.right() - 2.0, vp_rect.center().y - 6.0), Pos2::new(vp_rect.right() - 2.0, vp_rect.center().y + 6.0)],
        Stroke::new(1.5_f32, Color32::from_rgb(165, 180, 252)),
    );

    // Label on viewport
    painter.text(
        vp_rect.center(),
        egui::Align2::CENTER_CENTER,
        "ZOOM VIEWPORT",
        egui::FontId::monospace(8.0),
        Color32::from_rgb(224, 231, 255),
    );

    // Scrub / drag interaction
    if response.dragged() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let clicked_norm = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let view_len = cur_v_end.saturating_sub(cur_v_start);
            let new_center = (clicked_norm * total_frames as f32) as usize;
            let half = view_len / 2;

            let new_start = new_center.saturating_sub(half);
            let new_end = (new_start + view_len).min(total_frames);
            *view_start_frame = new_start;
            *view_end_frame = new_end;
        }
    }
}
