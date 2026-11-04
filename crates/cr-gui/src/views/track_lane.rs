//! Track lane under the zoomed waveform: a block per track, boundary handles to drag, and a
//! chain icon on each boundary to link or unlink the tracks either side.
//!
//! A linked boundary is one handle moving the end of one track and the start of the next. An
//! unlinked one shows the end bracket and the start bracket with the dead space between them;
//! dropping one bracket onto the other links them again.

use egui::{Color32, CursorIcon, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::tracks::{Handle, TracksState};

/// Lane height: the link icon strip above, the track blocks below.
const ICON_H: f32 = 16.0;
const BLOCK_H: f32 = 26.0;
/// A handle grabs the pointer this many pixels either side.
const GRAB_PX: f32 = 6.0;
/// Snapping and bracket joining reach, pixels.
const SNAP_PX: f32 = 8.0;

/// Fill colours for the tracks, in turn.
pub const TRACK_COLORS: [Color32; 6] = [
    Color32::from_rgb(99, 102, 241),
    Color32::from_rgb(20, 184, 166),
    Color32::from_rgb(245, 158, 11),
    Color32::from_rgb(236, 72, 153),
    Color32::from_rgb(14, 165, 233),
    Color32::from_rgb(132, 204, 22),
];

pub fn track_color(i: usize) -> Color32 {
    TRACK_COLORS[i % TRACK_COLORS.len()]
}

pub enum LaneAction {
    None,
    /// Play from this frame.
    PlayFrom(u64),
    /// Put the cursor at this frame (a track's start).
    GoTo(u64),
}

/// Draws the lane for the view `view_start..view_end` (frames) across the full width, the same
/// mapping as the waveform above it, and handles dragging, selecting and linking.
pub fn render_track_lane(ui: &mut Ui, tracks: &mut TracksState, view_start: usize, view_end: usize, playhead: usize) -> LaneAction {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, ICON_H + BLOCK_H), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let block_rect = Rect::from_min_max(Pos2::new(rect.left(), rect.top() + ICON_H), rect.max);
    painter.rect_filled(block_rect, 3.0, Color32::from_rgb(15, 23, 42));
    painter.rect_stroke(block_rect, 3.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));

    let Some(sheet) = tracks.sheet.as_ref() else {
        return LaneAction::None;
    };
    let view_len = view_end.saturating_sub(view_start).max(1) as f64;
    let x_of = |f: u64| rect.left() + ((f as f64 - view_start as f64) / view_len * width as f64) as f32;
    let frame_of = |x: f32| ((x - rect.left()) as f64 / width as f64 * view_len + view_start as f64).max(0.0) as u64;
    let px_frames = (view_len / width as f64) as u64;

    if sheet.tracks.is_empty() {
        painter.text(
            block_rect.center(),
            egui::Align2::CENTER_CENTER,
            "No tracks yet: look up the Discogs release below, or press S to split at the cursor",
            egui::FontId::proportional(11.0),
            Color32::from_rgb(100, 116, 139),
        );
    }

    // Blocks.
    for (i, t) in sheet.tracks.iter().enumerate() {
        let (x0, x1) = (x_of(t.start), x_of(t.end));
        if x1 < rect.left() || x0 > rect.right() {
            continue;
        }
        let r = Rect::from_min_max(Pos2::new(x0.max(rect.left()), block_rect.top() + 2.0), Pos2::new(x1.min(rect.right()), block_rect.bottom() - 2.0));
        let c = track_color(i);
        let selected = tracks.selected == Some(i);
        let alpha = if selected { 150 } else { 80 };
        painter.rect_filled(r, 2.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha));
        if selected {
            painter.rect_stroke(r, 2.0, Stroke::new(1.5_f32, Color32::WHITE));
        }
        let label = if t.title.is_empty() { sheet.number_label(i) } else { format!("{} {}", sheet.number_label(i), t.title) };
        painter.with_clip_rect(r.shrink(2.0)).text(
            Pos2::new(r.left() + 6.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(11.0),
            Color32::WHITE,
        );
    }

    // Handles and link icons.
    let handle_stroke = |active: bool| Stroke::new(if active { 2.5_f32 } else { 1.5_f32 }, Color32::WHITE);
    let mut icons: Vec<(Rect, usize, bool)> = Vec::new();
    for (i, t) in sheet.tracks.iter().enumerate() {
        let xs = x_of(t.start);
        let start_active = tracks.drag == Some(Handle::Start(i));
        if t.linked {
            // One shared boundary: a line with a diamond.
            painter.line_segment([Pos2::new(xs, block_rect.top()), Pos2::new(xs, block_rect.bottom())], handle_stroke(start_active));
            let c = Pos2::new(xs, block_rect.center().y);
            let d = 4.0;
            painter.add(egui::Shape::convex_polygon(
                vec![c + Vec2::new(0.0, -d), c + Vec2::new(d, 0.0), c + Vec2::new(0.0, d), c + Vec2::new(-d, 0.0)],
                Color32::WHITE,
                Stroke::NONE,
            ));
        } else {
            bracket(&painter, xs, block_rect, true, handle_stroke(start_active));
        }
        let next_linked = sheet.tracks.get(i + 1).is_some_and(|n| n.linked);
        if !next_linked {
            bracket(&painter, x_of(t.end), block_rect, false, handle_stroke(tracks.drag == Some(Handle::End(i))));
        }
        // The link icon of the boundary before track i, between the previous end and this start.
        if i > 0 {
            let xm = (x_of(sheet.tracks[i - 1].end) + xs) / 2.0;
            if xm >= rect.left() && xm <= rect.right() {
                icons.push((Rect::from_center_size(Pos2::new(xm, rect.top() + ICON_H / 2.0), Vec2::new(14.0, 14.0)), i, t.linked));
            }
        }
    }
    let pointer = response.hover_pos();
    for &(r, _, linked) in &icons {
        let hot = pointer.is_some_and(|p| r.contains(p));
        let color = match (linked, hot) {
            (_, true) => Color32::WHITE,
            (true, false) => Color32::from_rgb(148, 163, 184),
            (false, false) => Color32::from_rgb(251, 191, 36),
        };
        chain_icon(&painter, r.center(), linked, color);
    }

    // Playhead.
    let xp = x_of(playhead as u64);
    if xp >= rect.left() && xp <= rect.right() {
        painter.line_segment([Pos2::new(xp, block_rect.top()), Pos2::new(xp, block_rect.bottom())], Stroke::new(1.0_f32, Color32::from_rgb(250, 204, 21)));
    }

    // The handle under the pointer, if any.
    let handle_at = |x: f32| -> Option<Handle> {
        let mut best: Option<(f32, Handle)> = None;
        for (i, t) in sheet.tracks.iter().enumerate() {
            let mut cands = vec![(x_of(t.start), Handle::Start(i))];
            if !sheet.tracks.get(i + 1).is_some_and(|n| n.linked) {
                cands.push((x_of(t.end), Handle::End(i)));
            }
            for (hx, h) in cands {
                let d = (hx - x).abs();
                if d <= GRAB_PX && !best.is_some_and(|(bd, _)| d >= bd) {
                    best = Some((d, h));
                }
            }
        }
        best.map(|(_, h)| h)
    };
    let in_blocks = |p: Pos2| p.y >= block_rect.top();
    if let Some(p) = pointer {
        if tracks.drag.is_some() || (in_blocks(p) && handle_at(p.x).is_some()) {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        } else if icons.iter().any(|(r, _, _)| r.contains(p)) {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
    let track_at = |f: u64| sheet.tracks.iter().position(|t| f >= t.start && f < t.end);

    // Everything read from the sheet is worked out before it is changed. The handle is the
    // one under the press: egui reports a drag only once the pointer has moved a few pixels.
    let press = ui.input(|i| i.pointer.press_origin());
    let grabbed = if response.drag_started() { press.filter(|&p| in_blocks(p)).and_then(|p| handle_at(p.x)) } else { None };
    let click_at = if response.clicked() { response.interact_pointer_pos() } else { None };
    let icon_hit = click_at.and_then(|p| icons.iter().find(|(r, _, _)| r.contains(p)).map(|&(_, i, linked)| (i, linked)));
    let clicked_track = click_at
        .filter(|&p| icon_hit.is_none() && in_blocks(p))
        .map(|p| track_at(frame_of(p.x)).map(|i| (i, sheet.tracks[i].start)));
    let played = if response.double_clicked() {
        response.interact_pointer_pos().and_then(|p| track_at(frame_of(p.x))).map(|i| (i, sheet.tracks[i].start))
    } else {
        None
    };

    let mut action = LaneAction::None;
    if response.drag_started() {
        tracks.drag = grabbed;
    }
    if response.dragged() {
        if let (Some(h), Some(p)) = (tracks.drag, response.interact_pointer_pos()) {
            let mut f = frame_of(p.x);
            let alt = ui.input(|i| i.modifiers.alt);
            if tracks.snap && !alt {
                f = tracks.snap(f, (SNAP_PX as f64 * px_frames as f64) as u64, playhead as u64, h);
            }
            let sheet = tracks.sheet.as_mut().unwrap();
            match h {
                Handle::Start(i) => sheet.set_start(i, f),
                Handle::End(i) => sheet.set_end(i, f),
            }
            tracks.changed();
        }
    }
    if response.drag_stopped() {
        if let Some(h) = tracks.drag.take() {
            // The boundary settles on the quietest sample nearby.
            let at = match h {
                Handle::Start(i) => tracks.sheet.as_ref().unwrap().tracks[i].start,
                Handle::End(i) => tracks.sheet.as_ref().unwrap().tracks[i].end,
            };
            let settled = tracks.settle(at);
            let sheet = tracks.sheet.as_mut().unwrap();
            match h {
                Handle::Start(i) => sheet.set_start(i, settled),
                Handle::End(i) => sheet.set_end(i, settled),
            }
            // A bracket dropped onto its neighbour's links them.
            let join = (SNAP_PX as f64 * px_frames as f64) as u64;
            match h {
                Handle::Start(i) if i > 0 && !sheet.tracks[i].linked && sheet.tracks[i].start - sheet.tracks[i - 1].end <= join => sheet.link(i),
                Handle::End(i) if i + 1 < sheet.tracks.len() && !sheet.tracks[i + 1].linked && sheet.tracks[i + 1].start - sheet.tracks[i].end <= join => {
                    sheet.tracks[i + 1].start = sheet.tracks[i].end;
                    sheet.tracks[i + 1].linked = true;
                }
                _ => {}
            }
            tracks.changed();
        }
    }
    if let Some((i, linked)) = icon_hit {
        let sheet = tracks.sheet.as_mut().unwrap();
        if linked {
            sheet.unlink(i);
        } else {
            sheet.link(i);
        }
        tracks.changed();
    }
    if let Some(hit) = clicked_track {
        tracks.selected = hit.map(|(i, _)| i);
        if let Some((_, start)) = hit {
            action = LaneAction::GoTo(start);
        }
    }
    if let Some((i, start)) = played {
        tracks.selected = Some(i);
        action = LaneAction::PlayFrom(start);
    }
    action
}

/// "[" at a track start, "]" at an end.
fn bracket(painter: &egui::Painter, x: f32, r: Rect, start: bool, stroke: Stroke) {
    let arm = if start { 5.0 } else { -5.0 };
    let (top, bottom) = (r.top() + 1.0, r.bottom() - 1.0);
    painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom)], stroke);
    painter.line_segment([Pos2::new(x, top), Pos2::new(x + arm, top)], stroke);
    painter.line_segment([Pos2::new(x, bottom), Pos2::new(x + arm, bottom)], stroke);
}

/// Two chain links, apart when unlinked.
fn chain_icon(painter: &egui::Painter, c: Pos2, linked: bool, color: Color32) {
    let gap = if linked { 0.0 } else { 2.5 };
    let stroke = Stroke::new(1.5_f32, color);
    let link = |cx: f32| {
        painter.rect_stroke(Rect::from_center_size(Pos2::new(cx, c.y), Vec2::new(7.0, 5.0)), 2.5, stroke);
    };
    link(c.x - 2.5 - gap);
    link(c.x + 2.5 + gap);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cr_tracks::TrackSheet;
    use egui::{Event, PointerButton, RawInput};

    const SR: u32 = 1000;

    /// Two linked tracks and one after a gap, 100 s of audio, all in view (100 frames/px over
    /// the 1000 px lane).
    fn state() -> TracksState {
        let mut sheet = TrackSheet::new("x.wav", SR, 100_000);
        sheet.split_at(40_000);
        sheet.split_at(70_000);
        sheet.tracks[2].start = 75_000;
        sheet.tracks[2].linked = false;
        let mut t = TracksState::new();
        t.sheet = Some(sheet);
        t.snap = false;
        t
    }

    /// Runs one frame with `events`; returns the lane's rect.
    fn frame(ctx: &egui::Context, tracks: &mut TracksState, events: Vec<Event>) -> Rect {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 200.0))),
            events,
            ..Default::default()
        };
        let mut lane = Rect::NOTHING;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                lane = Rect::from_min_size(ui.next_widget_position(), Vec2::new(ui.available_width(), ICON_H + BLOCK_H));
                render_track_lane(ui, tracks, 0, 100_000, 0);
            });
        });
        lane
    }

    fn press(p: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Default::default() }
    }

    /// Presses at `from`, moves to `to` in steps, releases.
    fn drag(ctx: &egui::Context, tracks: &mut TracksState, from: Pos2, to: Pos2) {
        frame(ctx, tracks, vec![Event::PointerMoved(from)]);
        frame(ctx, tracks, vec![press(from, true)]);
        for k in 1..=10 {
            let p = from + (to - from) * (k as f32 / 10.0);
            frame(ctx, tracks, vec![Event::PointerMoved(p)]);
        }
        frame(ctx, tracks, vec![press(to, false)]);
        frame(ctx, tracks, vec![]);
    }

    fn x_at(lane: Rect, frame: u64) -> f32 {
        lane.left() + frame as f32 / 100_000.0 * lane.width()
    }

    #[test]
    fn test_dragging_a_linked_boundary_moves_both_tracks() {
        let ctx = egui::Context::default();
        let mut t = state();
        let lane = frame(&ctx, &mut t, vec![]);
        let y = lane.bottom() - BLOCK_H / 2.0;
        drag(&ctx, &mut t, Pos2::new(x_at(lane, 40_000), y), Pos2::new(x_at(lane, 45_000), y));
        let s = t.sheet.as_ref().unwrap();
        assert!((s.tracks[0].end as i64 - 45_000).abs() <= 200, "end {}", s.tracks[0].end);
        assert_eq!(s.tracks[0].end, s.tracks[1].start);
        assert!(s.check().is_empty());
    }

    #[test]
    fn test_dragging_an_unlinked_start_leaves_the_previous_end() {
        let ctx = egui::Context::default();
        let mut t = state();
        let lane = frame(&ctx, &mut t, vec![]);
        let y = lane.bottom() - BLOCK_H / 2.0;
        drag(&ctx, &mut t, Pos2::new(x_at(lane, 75_000), y), Pos2::new(x_at(lane, 78_000), y));
        let s = t.sheet.as_ref().unwrap();
        assert!((s.tracks[2].start as i64 - 78_000).abs() <= 200, "start {}", s.tracks[2].start);
        assert_eq!(s.tracks[1].end, 70_000);
        assert!(!s.tracks[2].linked);
    }

    #[test]
    fn test_dropping_a_start_bracket_on_the_previous_end_links_them() {
        let ctx = egui::Context::default();
        let mut t = state();
        let lane = frame(&ctx, &mut t, vec![]);
        let y = lane.bottom() - BLOCK_H / 2.0;
        // 75 000 -> 70 300: within the join reach of the end at 70 000.
        drag(&ctx, &mut t, Pos2::new(x_at(lane, 75_000), y), Pos2::new(x_at(lane, 70_300), y));
        let s = t.sheet.as_ref().unwrap();
        assert!(s.tracks[2].linked);
        assert_eq!(s.tracks[1].end, s.tracks[2].start);
    }

    #[test]
    fn test_chain_icon_click_toggles_the_link() {
        let ctx = egui::Context::default();
        let mut t = state();
        let lane = frame(&ctx, &mut t, vec![]);
        let icon = |at: u64| Pos2::new(x_at(lane, at), lane.top() + ICON_H / 2.0);
        let click = |t: &mut TracksState, p: Pos2| {
            frame(&ctx, t, vec![Event::PointerMoved(p)]);
            frame(&ctx, t, vec![press(p, true)]);
            frame(&ctx, t, vec![press(p, false)]);
            frame(&ctx, t, vec![]);
        };
        click(&mut t, icon(40_000));
        assert!(!t.sheet.as_ref().unwrap().tracks[1].linked);
        // The gap's icon sits midway between 70 000 and 75 000; linking extends track 2.
        click(&mut t, icon(72_500));
        let s = t.sheet.as_ref().unwrap();
        assert!(s.tracks[2].linked);
        assert_eq!(s.tracks[1].end, 75_000);
    }

    #[test]
    fn test_click_selects_the_track() {
        let ctx = egui::Context::default();
        let mut t = state();
        let lane = frame(&ctx, &mut t, vec![]);
        let p = Pos2::new(x_at(lane, 55_000), lane.bottom() - BLOCK_H / 2.0);
        frame(&ctx, &mut t, vec![Event::PointerMoved(p)]);
        frame(&ctx, &mut t, vec![press(p, true)]);
        frame(&ctx, &mut t, vec![press(p, false)]);
        assert_eq!(t.selected, Some(1));
    }
}
