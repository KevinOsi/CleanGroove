use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use crate::state::{AuditionMode, ClickMarker, TrackData, ViewportWaveformCache};

pub enum WaveformAction {
    None,
    OpenAudioFile,
    SeekTo(usize),
    SetSelection(usize, usize),
}

/// Renders the Audacity-style stacked waveform display with real-time output overlay, click pins, and selection highlight
pub fn render_waveform_stack(
    ui: &mut Ui,
    track_opt: Option<&TrackData>,
    view_start_frame: usize,
    view_end_frame: usize,
    selection_start: &mut usize,
    selection_end: &mut usize,
    playhead_frame: usize,
    audition_mode: AuditionMode,
    channel_h: f32,
    is_dragging_selection: &mut bool,
    cache: &ViewportWaveformCache,
    missed: &[ClickMarker],
) -> WaveformAction {
    let available_width = ui.available_width();
    let total_canvas_h = channel_h * 2.0 + 4.0;
    let ruler_h = 20.0;

    let track = match track_opt {
        Some(t) if t.total_frames > 0 => t,
        _ => {
            // Empty State View when no track is loaded
            let (canvas_rect, _) = ui.allocate_exact_size(
                Vec2::new(available_width, total_canvas_h + ruler_h + 2.0),
                egui::Sense::hover(),
            );
            let painter = ui.painter_at(canvas_rect);

            painter.rect_filled(canvas_rect, 6.0, Color32::from_rgb(11, 15, 25)); // dark slate-950
            painter.rect_stroke(canvas_rect, 6.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));

            // Center empty state card
            let center = canvas_rect.center();
            painter.text(
                Pos2::new(center.x, center.y - 24.0),
                egui::Align2::CENTER_CENTER,
                "NO AUDIO TRACK LOADED",
                egui::FontId::proportional(16.0),
                Color32::from_rgb(203, 213, 225),
            );
            painter.text(
                Pos2::new(center.x, center.y + 4.0),
                egui::Align2::CENTER_CENTER,
                "Load a WAV, FLAC, or AIFF file to inspect waveforms, detect clicks, and tune filters.",
                egui::FontId::proportional(12.0),
                Color32::from_rgb(148, 163, 184),
            );

            // Centered Open Audio File button inside the empty canvas
            let btn_rect = Rect::from_center_size(Pos2::new(center.x, center.y + 40.0), Vec2::new(180.0, 32.0));
            let btn_resp = ui.allocate_rect(btn_rect, egui::Sense::click());
            let btn_painter = ui.painter_at(btn_rect);

            let btn_bg = if btn_resp.hovered() { Color32::from_rgb(99, 102, 241) } else { Color32::from_rgb(79, 70, 229) };
            btn_painter.rect_filled(btn_rect, 4.0, btn_bg);
            btn_painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Open Audio File...",
                egui::FontId::proportional(13.0),
                Color32::WHITE,
            );

            if btn_resp.clicked() {
                return WaveformAction::OpenAudioFile;
            }

            return WaveformAction::None;
        }
    };

    let view_len = view_end_frame.saturating_sub(view_start_frame).max(1);
    let sample_rate = track.sample_rate.max(1) as f32;
    let mut action = WaveformAction::None;

    // 1. Time Ruler Strip
    let (ruler_rect, _) = ui.allocate_exact_size(Vec2::new(available_width, ruler_h), egui::Sense::hover());
    let ruler_painter = ui.painter_at(ruler_rect);
    ruler_painter.rect_filled(ruler_rect, 2.0, Color32::from_rgb(30, 41, 59)); // slate-800
    ruler_painter.rect_stroke(ruler_rect, 2.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));

    let num_ticks = 8;
    for i in 0..=num_ticks {
        let frac = i as f32 / num_ticks as f32;
        let frame = view_start_frame + (frac * view_len as f32) as usize;
        let sec = frame as f32 / sample_rate;
        let x = ruler_rect.left() + frac * ruler_rect.width();

        ruler_painter.line_segment(
            [Pos2::new(x, ruler_rect.bottom() - 4.0), Pos2::new(x, ruler_rect.bottom())],
            Stroke::new(1.0_f32, Color32::from_rgb(148, 163, 184)),
        );

        let mins = (sec / 60.0).floor() as u32;
        let rem_sec = sec % 60.0;
        let label = format!("{:02}:{:06.3}", mins, rem_sec);
        ruler_painter.text(
            Pos2::new(x + 2.0, ruler_rect.top() + 3.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::monospace(9.0),
            Color32::from_rgb(148, 163, 184),
        );
    }

    ui.add_space(2.0);

    // 2. Main Stacked Stereo Canvas (Left Channel + Right Channel)
    let (canvas_rect, response) = ui.allocate_exact_size(
        Vec2::new(available_width, total_canvas_h),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(canvas_rect);

    // Canvas Background
    painter.rect_filled(canvas_rect, 4.0, Color32::from_rgb(11, 15, 25)); // dark slate-950
    painter.rect_stroke(canvas_rect, 4.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));

    let left_rect = Rect::from_min_size(canvas_rect.min, Vec2::new(available_width, channel_h));
    let right_rect = Rect::from_min_size(
        Pos2::new(canvas_rect.left(), canvas_rect.top() + channel_h + 4.0),
        Vec2::new(available_width, channel_h),
    );

    // Divider between Left and Right channels
    painter.line_segment(
        [Pos2::new(canvas_rect.left(), canvas_rect.top() + channel_h + 2.0),
         Pos2::new(canvas_rect.right(), canvas_rect.top() + channel_h + 2.0)],
        Stroke::new(2.0_f32, Color32::from_rgb(30, 41, 59)),
    );

    // Center zero-crossing lines
    painter.line_segment(
        [Pos2::new(left_rect.left(), left_rect.center().y), Pos2::new(left_rect.right(), left_rect.center().y)],
        Stroke::new(1.0_f32, Color32::from_rgb(30, 41, 59)),
    );
    painter.line_segment(
        [Pos2::new(right_rect.left(), right_rect.center().y), Pos2::new(right_rect.right(), right_rect.center().y)],
        Stroke::new(1.0_f32, Color32::from_rgb(30, 41, 59)),
    );

    // Channel Badges
    let badge_font = egui::FontId::monospace(10.0);
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(left_rect.left() + 8.0, left_rect.top() + 6.0), Vec2::new(46.0, 18.0)),
        2.0,
        Color32::from_rgba_unmultiplied(15, 23, 42, 220),
    );
    painter.text(
        Pos2::new(left_rect.left() + 12.0, left_rect.top() + 8.0),
        egui::Align2::LEFT_TOP,
        "LEFT",
        badge_font.clone(),
        Color32::from_rgb(203, 213, 225),
    );

    painter.rect_filled(
        Rect::from_min_size(Pos2::new(right_rect.left() + 8.0, right_rect.top() + 6.0), Vec2::new(46.0, 18.0)),
        2.0,
        Color32::from_rgba_unmultiplied(15, 23, 42, 220),
    );
    painter.text(
        Pos2::new(right_rect.left() + 12.0, right_rect.top() + 8.0),
        egui::Align2::LEFT_TOP,
        "RIGHT",
        badge_font,
        Color32::from_rgb(203, 213, 225),
    );

    // Dual-Trace Waveform Legend (Top-right corner)
    let legend_rect = Rect::from_min_size(
        Pos2::new(canvas_rect.right() - 252.0, canvas_rect.top() + 6.0),
        Vec2::new(244.0, 18.0),
    );
    painter.rect_filled(
        legend_rect,
        3.0,
        Color32::from_rgba_unmultiplied(15, 23, 42, 230),
    );
    painter.rect_stroke(
        legend_rect,
        3.0,
        Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)),
    );

    match audition_mode {
        AuditionMode::OutClean => {
            let dot_y = legend_rect.center().y;
            let mut cur_x = legend_rect.left() + 8.0;

            painter.circle_filled(Pos2::new(cur_x, dot_y), 3.5, Color32::from_rgb(129, 140, 248));
            cur_x += 7.0;
            painter.text(
                Pos2::new(cur_x, dot_y),
                egui::Align2::LEFT_CENTER,
                "Original",
                egui::FontId::monospace(9.0),
                Color32::from_rgb(203, 213, 225),
            );
            cur_x += 54.0;

            painter.circle_filled(Pos2::new(cur_x, dot_y), 3.5, Color32::from_rgb(16, 185, 129));
            cur_x += 7.0;
            painter.text(
                Pos2::new(cur_x, dot_y),
                egui::Align2::LEFT_CENTER,
                if cache.repairs_previewed { "Repaired Output" } else { "Zoom in to preview repairs" },
                egui::FontId::monospace(9.0),
                Color32::from_rgb(52, 211, 153),
            );
        }
        AuditionMode::InBypass => {
            let dot_y = legend_rect.center().y;
            painter.circle_filled(Pos2::new(legend_rect.left() + 10.0, dot_y), 3.5, Color32::from_rgb(129, 140, 248));
            painter.text(
                Pos2::new(legend_rect.left() + 18.0, dot_y),
                egui::Align2::LEFT_CENTER,
                "Bypass: Raw Input Waveform",
                egui::FontId::monospace(9.0),
                Color32::from_rgb(203, 213, 225),
            );
        }
        AuditionMode::NoiseDelta => {
            let dot_y = legend_rect.center().y;
            painter.circle_filled(Pos2::new(legend_rect.left() + 10.0, dot_y), 3.5, Color32::from_rgb(239, 68, 68));
            painter.text(
                Pos2::new(legend_rect.left() + 18.0, dot_y),
                egui::Align2::LEFT_CENTER,
                "Noise Delta: Removed Clicks Only",
                egui::FontId::monospace(9.0),
                Color32::from_rgb(248, 113, 113),
            );
        }
    }

    // Amplitude Scales on Right Edge
    for (rect, _) in [(&left_rect, "L"), (&right_rect, "R")] {
        let x_scale = rect.right() - 28.0;
        let scale_font = egui::FontId::monospace(8.0);
        let scale_color = Color32::from_rgb(100, 116, 139);

        painter.text(Pos2::new(x_scale, rect.top() + 2.0), egui::Align2::LEFT_TOP, "+1.0", scale_font.clone(), scale_color);
        painter.text(Pos2::new(x_scale, rect.center().y - 4.0), egui::Align2::LEFT_TOP, " 0.0", scale_font.clone(), Color32::from_rgb(203, 213, 225));
        painter.text(Pos2::new(x_scale, rect.bottom() - 10.0), egui::Align2::LEFT_TOP, "-1.0", scale_font, scale_color);
    }

    // 3. Draw Waveform Bars (Dual-Trace: Original Uncorrected Purple `#818cf8` + Real-Time Repaired Green `#10b981`)
    let num_columns = (available_width / 2.0).ceil() as usize;
    let frames_per_col = (view_len as f32 / num_columns as f32).max(1.0) as usize;

    let orig_color = Color32::from_rgba_unmultiplied(129, 140, 248, 140); // semi-transparent purple
    let rep_color = Color32::from_rgba_unmultiplied(16, 185, 129, 230);   // vibrant emerald green
    let bypass_color = Color32::from_rgb(92, 107, 192);                   // Audacity purple-indigo
    let delta_color = Color32::from_rgb(239, 68, 68);                     // crimson red

    let half_h = channel_h * 0.44;

    if cache.orig_l.len() == num_columns && !cache.orig_l.is_empty() {
        for col in 0..num_columns {
            let col_x = canvas_rect.left() + (col as f32 * 2.0);

            match audition_mode {
                AuditionMode::OutClean => {
                    // Left Channel
                    let l_orig_h = (cache.orig_l[col] * half_h).min(half_h);
                    let l_rep_h = (cache.rep_l[col] * half_h).min(half_h);

                    if l_orig_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, left_rect.center().y - l_orig_h),
                                Pos2::new(col_x + 1.6, left_rect.center().y + l_orig_h),
                            ),
                            0.0,
                            orig_color,
                        );
                    }
                    if l_rep_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, left_rect.center().y - l_rep_h),
                                Pos2::new(col_x + 1.6, left_rect.center().y + l_rep_h),
                            ),
                            0.0,
                            rep_color,
                        );
                    }

                    // Right Channel
                    let r_orig_h = (cache.orig_r[col] * half_h).min(half_h);
                    let r_rep_h = (cache.rep_r[col] * half_h).min(half_h);

                    if r_orig_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, right_rect.center().y - r_orig_h),
                                Pos2::new(col_x + 1.6, right_rect.center().y + r_orig_h),
                            ),
                            0.0,
                            orig_color,
                        );
                    }
                    if r_rep_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, right_rect.center().y - r_rep_h),
                                Pos2::new(col_x + 1.6, right_rect.center().y + r_rep_h),
                            ),
                            0.0,
                            rep_color,
                        );
                    }
                }
                AuditionMode::InBypass => {
                    let l_h = (cache.orig_l[col] * half_h).min(half_h);
                    if l_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, left_rect.center().y - l_h),
                                Pos2::new(col_x + 1.6, left_rect.center().y + l_h),
                            ),
                            0.0,
                            bypass_color,
                        );
                    }
                    let r_h = (cache.orig_r[col] * half_h).min(half_h);
                    if r_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, right_rect.center().y - r_h),
                                Pos2::new(col_x + 1.6, right_rect.center().y + r_h),
                            ),
                            0.0,
                            bypass_color,
                        );
                    }
                }
                AuditionMode::NoiseDelta => {
                    let l_delta = (cache.orig_l[col] - cache.rep_l[col]).abs();
                    let l_h = (l_delta * half_h * 2.5).min(half_h);
                    if l_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, left_rect.center().y - l_h),
                                Pos2::new(col_x + 1.6, left_rect.center().y + l_h),
                            ),
                            0.0,
                            delta_color,
                        );
                    }
                    let r_delta = (cache.orig_r[col] - cache.rep_r[col]).abs();
                    let r_h = (r_delta * half_h * 2.5).min(half_h);
                    if r_h > 0.5 {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(col_x, right_rect.center().y - r_h),
                                Pos2::new(col_x + 1.6, right_rect.center().y + r_h),
                            ),
                            0.0,
                            delta_color,
                        );
                    }
                }
            }
        }
    } else {
        // Fallback before cache is ready: render raw channel slices
        if audition_mode != AuditionMode::NoiseDelta {
            for col in 0..num_columns {
                let col_x = canvas_rect.left() + (col as f32 * 2.0);
                let frame_idx = view_start_frame + col * frames_per_col;
                if frame_idx >= track.total_frames {
                    break;
                }

                let end_idx = (frame_idx + frames_per_col).min(track.total_frames);
                let column = &track.frames_internal[frame_idx..end_idx];
                let max_l = column.iter().map(|f| (f.0 as f32 / 16_777_216.0).abs()).fold(0.0f32, f32::max);
                let l_bar_h = (max_l * half_h).min(half_h);
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(col_x, left_rect.center().y - l_bar_h),
                        Pos2::new(col_x + 1.6, left_rect.center().y + l_bar_h),
                    ),
                    0.0,
                    bypass_color,
                );

                let max_r = column.iter().map(|f| (f.1 as f32 / 16_777_216.0).abs()).fold(0.0f32, f32::max);
                let r_bar_h = (max_r * half_h).min(half_h);
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(col_x, right_rect.center().y - r_bar_h),
                        Pos2::new(col_x + 1.6, right_rect.center().y + r_bar_h),
                    ),
                    0.0,
                    bypass_color,
                );
            }
        }
    }

    // 4. Draw Detected Clicks (Indicator Pins on Top Edge & Transient Lines, O(log N) binary search slice)
    // Exposed clicks the current settings miss: amber pins on the channel's bottom edge.
    for click in missed {
        if click.frame >= view_start_frame && click.frame <= view_end_frame {
            let frac = (click.frame - view_start_frame) as f32 / view_len as f32;
            let x = canvas_rect.left() + frac * canvas_rect.width();
            let target_rect = if click.channel == 0 { &left_rect } else { &right_rect };
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(x, target_rect.bottom() - 9.0),
                    Pos2::new(x - 4.0, target_rect.bottom()),
                    Pos2::new(x + 4.0, target_rect.bottom()),
                ],
                Color32::from_rgb(251, 191, 36),
                Stroke::NONE,
            ));
        }
    }

    for click in &cache.viewport_clicks {
        {
            let frac = (click.frame.saturating_sub(view_start_frame)) as f32 / view_len as f32;
            let x = canvas_rect.left() + frac * canvas_rect.width();
            let target_rect = if click.channel == 0 { &left_rect } else { &right_rect };

            // Indicator Pin on channel top edge
            let pin_color = match audition_mode {
                AuditionMode::OutClean => Color32::from_rgb(16, 185, 129), // emerald green
                AuditionMode::InBypass => Color32::from_rgb(239, 68, 68),  // red-500
                AuditionMode::NoiseDelta => Color32::from_rgb(239, 68, 68), // red-500
            };

            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x - 1.0, target_rect.top()), Vec2::new(3.0, 6.0)),
                0.0,
                pin_color,
            );

            match audition_mode {
                AuditionMode::InBypass | AuditionMode::NoiseDelta => {
                    let spike_h = half_h * 0.95;
                    painter.line_segment(
                        [Pos2::new(x, target_rect.center().y - spike_h), Pos2::new(x, target_rect.center().y + spike_h)],
                        Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(239, 68, 68, 180)),
                    );
                }
                AuditionMode::OutClean => {
                    let spike_h = half_h * 0.4;
                    painter.line_segment(
                        [Pos2::new(x, target_rect.top() + 6.0), Pos2::new(x, target_rect.top() + 6.0 + spike_h)],
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(16, 185, 129, 120)),
                    );
                }
            }
        }
    }

    // 5. User Selection Highlight (Translucent Indigo Box)
    let sel_min = (*selection_start).min(*selection_end);
    let sel_max = (*selection_start).max(*selection_end);

    if sel_max > sel_min && sel_max > view_start_frame && sel_min < view_end_frame {
        let sel_left_frac = ((sel_min.max(view_start_frame) - view_start_frame) as f32 / view_len as f32).clamp(0.0, 1.0);
        let sel_right_frac = ((sel_max.min(view_end_frame) - view_start_frame) as f32 / view_len as f32).clamp(0.0, 1.0);

        let sel_x1 = canvas_rect.left() + sel_left_frac * canvas_rect.width();
        let sel_x2 = canvas_rect.left() + sel_right_frac * canvas_rect.width();
        let sel_rect = Rect::from_x_y_ranges(sel_x1..=sel_x2, canvas_rect.y_range());

        painter.rect_filled(sel_rect, 0.0, Color32::from_rgba_unmultiplied(99, 102, 241, 35));
        painter.rect_stroke(sel_rect, 0.0, Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(129, 140, 248, 200)));

        painter.line_segment(
            [Pos2::new(sel_x1, canvas_rect.top()), Pos2::new(sel_x1, canvas_rect.bottom())],
            Stroke::new(2.0_f32, Color32::from_rgb(165, 180, 252)),
        );
        painter.line_segment(
            [Pos2::new(sel_x2, canvas_rect.top()), Pos2::new(sel_x2, canvas_rect.bottom())],
            Stroke::new(2.0_f32, Color32::from_rgb(165, 180, 252)),
        );
    }

    // 6. Playhead Position Marker (Solid White line + Top Flag)
    if playhead_frame >= view_start_frame && playhead_frame <= view_end_frame {
        let playhead_frac = (playhead_frame - view_start_frame) as f32 / view_len as f32;
        let px = canvas_rect.left() + playhead_frac * canvas_rect.width();

        painter.line_segment(
            [Pos2::new(px, canvas_rect.top()), Pos2::new(px, canvas_rect.bottom())],
            Stroke::new(2.0_f32, Color32::WHITE),
        );

        let flag_size = 5.0;
        let p_top = Pos2::new(px, canvas_rect.top() + 6.0);
        let p_l = Pos2::new(px - flag_size, canvas_rect.top());
        let p_r = Pos2::new(px + flag_size, canvas_rect.top());
        painter.add(egui::Shape::convex_polygon(vec![p_l, p_r, p_top], Color32::WHITE, Stroke::NONE));
    }

    // 7. Interactive Cursor Positioning & Drag Selection Handler
    // A single click positions the cursor and starts playback from that point.
    // A click-and-drag highlights a loop/audition section.
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let frac = ((pos.x - canvas_rect.left()) / canvas_rect.width()).clamp(0.0, 1.0);
            let frame = view_start_frame + (frac * view_len as f32) as usize;
            *selection_start = frame;
            *selection_end = frame;
            action = WaveformAction::SeekTo(frame);
        }
    } else if response.drag_started() {
        *is_dragging_selection = true;
        if let Some(pos) = response.interact_pointer_pos() {
            let frac = ((pos.x - canvas_rect.left()) / canvas_rect.width()).clamp(0.0, 1.0);
            let frame = view_start_frame + (frac * view_len as f32) as usize;
            *selection_start = frame;
            *selection_end = frame;
            action = WaveformAction::SeekTo(frame);
        }
    } else if response.dragged() && *is_dragging_selection {
        if let Some(pos) = response.interact_pointer_pos() {
            let frac = ((pos.x - canvas_rect.left()) / canvas_rect.width()).clamp(0.0, 1.0);
            let frame = view_start_frame + (frac * view_len as f32) as usize;
            *selection_end = frame;
        }
    } else if response.drag_stopped() {
        *is_dragging_selection = false;
        if *selection_start > *selection_end {
            let tmp = *selection_start;
            *selection_start = *selection_end;
            *selection_end = tmp;
        }
        action = WaveformAction::SetSelection(*selection_start, *selection_end);
    }

    action
}
