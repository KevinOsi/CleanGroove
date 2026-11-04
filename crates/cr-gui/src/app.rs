use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use crossbeam_channel::{Receiver, Sender};
use egui::{Color32, Stroke};

use crate::audio_player::AudioPlayer;
use crate::dsp_bridge::{block_context, merge_markers, render_settings, warm_up_frames, RepairRuns, FULL_SCALE};
use crate::state::{
    AudioTransportControl, AuditionMode, ClickMarker, DeclickMethod, DspTuning, EngineMode, JobMessage,
    ProgressState, TrackData, ViewportWaveformCache,
};
use crate::live::LiveController;
use crate::views::analysis_panel::{render_analysis_panel, PanelAction};
use crate::views::levels_panel::render_levels_panel;
use crate::views::help::{render_about, render_user_guide};
use crate::views::settings::render_settings_window;
use crate::views::live_wave::{render_live_waveform, render_take_timeline};
use cr_dsp::analysis::AlbumAnalysis;
use cr_rt::live::TapeState;
use cr_dsp::autotune::AutoTuneReport;
use crate::views::timeline::render_timeline_mini_map;
use crate::views::waveform::{render_waveform_stack, WaveformAction};
use crate::tracks::TracksState;
use crate::views::track_lane::{render_track_lane, LaneAction};
use crate::views::track_overview::render_track_overview;
use crate::views::tracks_panel::{render_tracks_panel, TracksAction};

/// Height kept below the waveform for its time ruler, the transport bar and the controls.
const BELOW_WAVEFORM_H: f32 = 175.0;
/// The same in Tracks mode: ruler, track lane and transport bar.
const BELOW_WAVEFORM_TRACKS_H: f32 = 70.0;
/// Tracks mode: "Hear Start" plays from this long before a boundary.
const HEAR_BEFORE_S: f64 = 3.0;

/// The app icon as a texture, for the About window.
fn load_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(include_bytes!("../assets/icon_256.png")).ok()?.into_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("app_icon", color, egui::TextureOptions::LINEAR))
}

/// Height of the Live mode levels panel.
const LEVELS_PANEL_H: f32 = 150.0;

/// Longest view, in seconds, previewed with repairs. Wider views show the original waveform
/// only, so zooming out on an album does not run the engine over it on the UI thread.
const PREVIEW_MAX_SECS: usize = 30;
/// After the view is panned or zoomed with the wheel, File Mode shows the original waveform
/// until the view has stayed put this long, then previews the repairs again: scrolling never
/// waits on the declicker.
const PREVIEW_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);
/// Tracks mode: Previous Track goes to the start of the track before when the cursor is
/// within this many seconds of the current track's start.
const PREV_TRACK_GRACE_S: f64 = 1.0;

pub struct CleanGrooveApp {
    pub track: Option<TrackData>,
    pub player: AudioPlayer,
    pub engine_mode: EngineMode,
    pub dsp_tuning: DspTuning,
    pub view_start_frame: usize,
    pub view_end_frame: usize,
    pub selection_start: usize,
    pub selection_end: usize,
    pub is_dragging_selection: bool,
    /// Album analysis of the loaded track, and the auto-tuner's suggestion built on it.
    pub analysis: Option<Arc<AlbumAnalysis>>,
    pub tune_report: Option<AutoTuneReport>,
    /// Exposed-click density per timeline bin (0..1), derived from the analysis.
    pub timeline_heatmap: Vec<f32>,
    /// Album gain and output levels of the last render through the post chain.
    pub last_render_levels: Option<(f64, cr_dsp::post::LevelReport)>,
    pub active_progress: Option<ProgressState>,
    pub job_sender: Sender<JobMessage>,
    pub job_receiver: Receiver<JobMessage>,
    /// Live mode: devices, monitoring session, recorder and meters.
    pub live: LiveController,
    pub status_message: Option<String>,
    pub screenshot_target: Option<PathBuf>,
    pub screenshot_frame_count: u32,
    /// Open windows: Settings, Help > User Guide, Help > About.
    pub show_settings: bool,
    pub show_guide: bool,
    pub show_about: bool,
    about_icon: Option<egui::TextureHandle>,
    pub viewport_cache: ViewportWaveformCache,
    /// Tracks mode: album metadata and track splits of the loaded file.
    pub tracks: TracksState,
    /// The preview chosen before entering Tracks mode (which plays the file as it is).
    audition_before_tracks: Option<AuditionMode>,
    /// When the wheel last moved the view (see `PREVIEW_SETTLE`).
    view_moved_at: Option<Instant>,
    /// Settings > Online services: Discogs token and MusicBrainz contact.
    pub services: crate::prefs::ServicesState,
    /// The Tracks panel's cover thumbnail.
    cover_texture: Option<egui::TextureHandle>,
    /// Windows: drops taken by CleanGroove's own drop target (see `crate::drop`).
    pub drop_inbox: Option<Arc<crate::drop::DropInbox>>,
}

impl CleanGrooveApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let transport = AudioTransportControl::new();
        let player = AudioPlayer::new(transport);
        let (job_sender, job_receiver) = crossbeam_channel::unbounded();

        Self {
            track: None,
            player,
            engine_mode: EngineMode::File,
            dsp_tuning: DspTuning::default(),
            view_start_frame: 0,
            view_end_frame: 0,
            selection_start: 0,
            selection_end: 0,
            is_dragging_selection: false,
            analysis: None,
            tune_report: None,
            timeline_heatmap: Vec::new(),
            last_render_levels: None,
            active_progress: None,
            job_sender,
            job_receiver,
            live: LiveController::new(),
            status_message: None,
            screenshot_target: None,
            screenshot_frame_count: 0,
            show_settings: false,
            show_guide: false,
            show_about: false,
            about_icon: None,
            viewport_cache: ViewportWaveformCache::default(),
            tracks: TracksState::new(),
            audition_before_tracks: None,
            view_moved_at: None,
            services: crate::prefs::ServicesState::new(),
            cover_texture: None,
            drop_inbox: None,
        }
    }

    /// File > Close: stops playback and clears the track, its analysis and the view.
    pub fn close_file(&mut self) {
        let Some(t) = self.track.take() else { return };
        self.tracks.close();
        self.player.unload();
        self.analysis = None;
        self.tune_report = None;
        self.dsp_tuning.dethump = false;
        self.dsp_tuning.thump_spans = Arc::new(Vec::new());
        self.player.update_tuning(self.dsp_tuning.clone());
        self.timeline_heatmap.clear();
        self.last_render_levels = None;
        self.view_start_frame = 0;
        self.view_end_frame = 0;
        self.selection_start = 0;
        self.selection_end = 0;
        self.viewport_cache = ViewportWaveformCache::default();
        self.status_message = Some(format!("Closed {}", t.filename));
    }

    /// Selects the whole track as the play / loop range.
    pub fn select_all(&mut self) {
        if let Some(ref t) = self.track {
            self.selection_start = 0;
            self.selection_end = t.total_frames;
            self.player.transport.loop_start_frame.store(0, Ordering::Relaxed);
            self.player.transport.loop_end_frame.store(t.total_frames, Ordering::Relaxed);
        }
    }

    pub fn zoom(&mut self, factor: f32) {
        if let Some(ref t) = self.track {
            let cur_len = self.view_end_frame.saturating_sub(self.view_start_frame);
            // At least 0.1 s, at most the whole track.
            let new_len = ((cur_len as f32 * factor) as usize)
                .max((t.sample_rate as f32 * 0.1) as usize)
                .min(t.total_frames);
            let mid = (self.view_start_frame + self.view_end_frame) / 2;
            self.view_start_frame = mid.saturating_sub(new_len / 2);
            self.view_end_frame = (self.view_start_frame + new_len).min(t.total_frames);
        }
    }

    pub fn fit_track(&mut self) {
        if let Some(ref t) = self.track {
            self.view_start_frame = 0;
            self.view_end_frame = t.total_frames;
        }
    }

    /// Applies a Presets menu entry.
    fn apply_preset(&mut self, declick: u32, decrackle: u32, reverse: bool, stereo_to_mono: bool) {
        self.dsp_tuning.declick = declick;
        self.dsp_tuning.decrackle = decrackle;
        self.dsp_tuning.pitch_protection = true;
        self.dsp_tuning.reverse = reverse;
        self.dsp_tuning.stereo_to_mono = stereo_to_mono;
        self.tuning_changed();
    }

    pub fn set_audition(&mut self, mode: AuditionMode) {
        self.dsp_tuning.audition_mode = mode;
        self.tuning_changed();
    }

    /// Pushes the tuning to the file preview and, when monitoring, to the live engine.
    pub fn tuning_changed(&mut self) {
        self.player.update_tuning(self.dsp_tuning.clone());
        self.live.apply_tuning(&self.dsp_tuning);
    }

    pub fn set_engine_mode(&mut self, mode: EngineMode) {
        if mode != EngineMode::Live && self.live.is_running() {
            // Leaving Live mode never cuts a recording short.
            if self.live.tape.state != TapeState::Idle {
                self.status_message = Some("Stop the recording before leaving Live Monitor".to_string());
                return;
            }
            self.stop_monitoring();
        }
        // Tracks mode plays the file as it is; the preview chosen before comes back after.
        if mode == EngineMode::Tracks && self.engine_mode != EngineMode::Tracks {
            if self.dsp_tuning.audition_mode != AuditionMode::InBypass {
                self.audition_before_tracks = Some(self.dsp_tuning.audition_mode);
                self.set_audition(AuditionMode::InBypass);
            }
            self.start_track_envelope();
        } else if mode != EngineMode::Tracks && self.engine_mode == EngineMode::Tracks {
            self.tracks.flush();
            if let Some(m) = self.audition_before_tracks.take() {
                self.set_audition(m);
            }
        }
        self.engine_mode = mode;
        if mode == EngineMode::Live && self.live.scan.is_none() {
            self.rescan_devices();
        }
    }

    fn start_track_envelope(&mut self) {
        if let Some(t) = self.track.as_ref() {
            self.tracks.ensure_envelope(&t.frames_internal, t.sample_rate);
        }
    }

    /// Puts the cursor at `frame`, scrolling the view to it when it is out of sight. Playback,
    /// if on, carries on from there.
    fn go_to(&mut self, frame: usize) {
        let Some(t) = self.track.as_ref() else { return };
        let frame = frame.min(t.total_frames);
        let len = self.view_end_frame.saturating_sub(self.view_start_frame).max(1);
        if frame < self.view_start_frame || frame >= self.view_end_frame {
            self.view_start_frame = frame.saturating_sub(len / 6);
            self.view_end_frame = (self.view_start_frame + len).min(t.total_frames);
        }
        self.selection_start = 0;
        self.selection_end = 0;
        self.player.transport.loop_start_frame.store(frame, Ordering::Relaxed);
        self.player.transport.loop_end_frame.store(frame, Ordering::Relaxed);
        self.player.transport.seek_to(frame);
    }

    /// Tracks mode: play from `frame`, scrolling the view to it when it is out of sight.
    fn play_from(&mut self, frame: usize) {
        self.go_to(frame);
        self.player.transport.play();
    }

    /// Tracks mode: the start of the next (or previous) track from the cursor, selected.
    fn jump_track(&mut self, forward: bool) {
        let Some(sheet) = self.tracks.sheet.as_ref() else { return };
        let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed) as u64;
        let grace = (PREV_TRACK_GRACE_S * sheet.sample_rate as f64) as u64;
        if let Some(i) = crate::tracks::track_jump(sheet, playhead, forward, grace) {
            let start = sheet.tracks[i].start as usize;
            self.tracks.selected = Some(i);
            self.go_to(start);
        }
    }

    /// Shift+wheel (or a sideways wheel or swipe) pans the zoomed view; Ctrl+wheel zooms it
    /// around the pointer. Only while the pointer is over `rect`.
    fn wheel_navigate(&mut self, ui: &egui::Ui, rect: egui::Rect) {
        let Some(t) = self.track.as_ref() else { return };
        let Some(pointer) = ui.ctx().pointer_hover_pos().filter(|p| rect.contains(*p)) else { return };
        let (pan, zoom) = ui.input(|i| (i.smooth_scroll_delta.x, i.zoom_delta()));
        if pan == 0.0 && zoom == 1.0 {
            return;
        }
        let at = ((pointer.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
        let min_len = (t.sample_rate as f64 * 0.1) as usize;
        (self.view_start_frame, self.view_end_frame) =
            crate::state::pan_zoom_view(self.view_start_frame, self.view_end_frame, t.total_frames, min_len, rect.width(), at, pan, zoom);
        self.view_moved_at = Some(Instant::now());
    }

    fn tracks_action(&mut self, action: TracksAction) {
        let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed);
        match action {
            TracksAction::None => {}
            TracksAction::LookUp => {
                if let Some(t) = self.track.as_ref() {
                    self.tracks.look_up(&t.frames_internal, t.sample_rate, &self.services.prefs.musicbrainz_contact);
                }
            }
            TracksAction::OpenSearch => self.tracks.open_search(),
            TracksAction::OpenExport => self.tracks.export_open = true,
            TracksAction::RemoveCover => self.tracks.remove_cover(),
            TracksAction::PasteCover => self.tracks.paste_cover(),
            TracksAction::FetchCover => {
                let contact = self.services.prefs.musicbrainz_contact.clone();
                self.tracks.fetch_cover(&contact);
            }
            TracksAction::ChooseCover => {
                if let Some(p) = rfd::FileDialog::new().add_filter("Images", &["jpg", "jpeg", "png"]).pick_file() {
                    self.tracks.choose_cover(&p);
                }
            }
            TracksAction::PlayFrom(f) => self.play_from(f as usize),
            TracksAction::GoTo(f) => self.go_to(f as usize),
            TracksAction::PlayAcross(f) => {
                let sr = self.track.as_ref().map_or(44_100, |t| t.sample_rate) as f64;
                self.play_from((f as usize).saturating_sub((HEAR_BEFORE_S * sr) as usize));
            }
            TracksAction::SplitAtCursor => {
                let at = self.tracks.settle(playhead as u64);
                let Some(sheet) = self.tracks.sheet.as_mut() else { return };
                match sheet.split_at(at) {
                    Some(i) => {
                        self.tracks.selected = Some(i);
                        self.tracks.changed();
                    }
                    None => self.status_message = Some("Put the cursor inside a track, at least a second from its ends, to split it".to_string()),
                }
            }
        }
    }

    /// Tracks mode centre: album overview with the track bands, the zoomed waveform and the
    /// track lane under it.
    fn tracks_center(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("ALBUM OVERVIEW & TRACKS").strong().color(Color32::from_rgb(203, 213, 225)).size(10.0));
            ui.label(egui::RichText::new("|").color(Color32::from_rgb(71, 85, 105)));
            let line = match self.tracks.sheet.as_ref() {
                Some(s) if !s.tracks.is_empty() => {
                    let a = &s.album;
                    let name = if a.title.is_empty() { String::new() } else { format!("{} - {}  |  ", a.album_artist, a.title) };
                    format!("{}{} tracks", name, s.tracks.len())
                }
                Some(_) => "No tracks yet".to_string(),
                None => "No file loaded".to_string(),
            };
            ui.label(egui::RichText::new(line).color(Color32::from_rgb(148, 163, 184)).size(10.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new("Playing the file as it is (no declick)").color(Color32::from_rgb(100, 116, 139)).size(10.0));
            });
        });
        render_track_overview(
            ui,
            self.track.as_ref(),
            self.tracks.sheet.as_ref(),
            self.tracks.selected,
            &mut self.view_start_frame,
            &mut self.view_end_frame,
        );
        ui.add_space(6.0);

        let num_columns = (ui.available_width() / 2.0).ceil() as usize;
        self.update_viewport_cache_if_needed(num_columns);
        let channel_h = ((ui.available_height() - BELOW_WAVEFORM_TRACKS_H - 42.0) / 2.0).clamp(70.0, 420.0);
        let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed);
        let wave_top = ui.cursor().top();
        let action = render_waveform_stack(
            ui,
            self.track.as_ref(),
            self.view_start_frame,
            self.view_end_frame,
            &mut self.selection_start,
            &mut self.selection_end,
            playhead,
            AuditionMode::InBypass,
            channel_h,
            &mut self.is_dragging_selection,
            &self.viewport_cache,
            &[],
        );
        self.waveform_action(action);
        if self.track.is_some() {
            ui.add_space(2.0);
            match render_track_lane(ui, &mut self.tracks, self.view_start_frame, self.view_end_frame, playhead) {
                LaneAction::PlayFrom(f) => self.play_from(f as usize),
                LaneAction::GoTo(f) => self.go_to(f as usize),
                LaneAction::None => {}
            }
        }
        let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), wave_top..=ui.cursor().top());
        self.wheel_navigate(ui, rect);
    }

    /// Tracks: exports the album in the background, with the progress window.
    fn start_export(&mut self) {
        let (Some(track), Some(sheet)) = (self.track.as_ref(), self.tracks.sheet.clone()) else { return };
        if let Err(e) = self.services.prefs.save() {
            self.status_message = Some(format!("Could not save the export settings: {}", e));
        }
        let opts = self.tracks.export_options(&self.services.prefs, track.bits_per_sample);
        let frames = Arc::clone(&track.frames_internal);
        let channels = track.channels.clamp(1, 2);
        let source = track.path.clone();
        let cover = self.tracks.cover.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancel_flag);
        let tx = self.job_sender.clone();
        self.active_progress = Some(ProgressState {
            title: "Exporting".to_string(),
            description: "Starting...".to_string(),
            progress: 0.0,
            cancel_flag,
        });
        self.tracks.export_open = false;
        std::thread::spawn(move || {
            let job = cr_tracks::export::ExportJob { sheet: &sheet, frames: &frames, channels, source_audio: source.as_deref(), cover: cover.as_ref() };
            let progress = |f: f32, m: &str| {
                let _ = tx.send(JobMessage::Progress { fraction: f, description: m.to_string() });
            };
            let _ = tx.send(match cr_tracks::export::export_album(&job, &opts, progress, &cancel) {
                Ok(r) => JobMessage::ExportFinished { album_dir: r.album_dir, files: r.files.len(), seconds: r.seconds },
                Err(cr_tracks::export::ExportError::Cancelled) => JobMessage::Cancelled,
                Err(e) => JobMessage::Error(format!("Export failed: {}", e)),
            });
        });
    }

    fn waveform_action(&mut self, action: WaveformAction) {
        match action {
            WaveformAction::OpenAudioFile => {
                self.open_audio_file();
            }
            WaveformAction::SeekTo(frame) => {
                self.player.transport.seek_to(frame);
                self.player.transport.loop_start_frame.store(frame, Ordering::Relaxed);
                self.player.transport.loop_end_frame.store(frame, Ordering::Relaxed);
            }
            WaveformAction::SetSelection(start, end) => {
                self.player.transport.loop_start_frame.store(start, Ordering::Relaxed);
                self.player.transport.loop_end_frame.store(end, Ordering::Relaxed);
                self.player.transport.seek_to(start);
            }
            WaveformAction::None => {}
        }
    }

    pub fn rescan_devices(&mut self) {
        if let Err(e) = self.live.rescan() {
            self.status_message = Some(format!("Could not list audio devices: {}", e));
        }
    }

    pub fn start_monitoring(&mut self) {
        // One thing plays at a time: the file preview stops while monitoring.
        self.player.transport.pause();
        match self.live.start(&self.dsp_tuning) {
            Ok(warnings) => {
                self.status_message = Some(match warnings.first() {
                    Some(w) => format!("Warning: {}", w),
                    None => format!("Monitoring, latency {:.0} ms", self.live.latency_ms),
                });
            }
            Err(e) => self.status_message = Some(format!("Could not start monitoring: {}", e)),
        }
    }

    pub fn stop_monitoring(&mut self) {
        self.status_message = Some(match self.live.stop() {
            Ok(Some(t)) => crate::live::take_summary(&t),
            Ok(None) => "Monitoring stopped".to_string(),
            Err(e) => format!("Error while stopping: {}", e),
        });
    }

    /// Live mode centre: the take timeline and the scrolling waveform with repair pins, the
    /// File mode timeline and waveform's live counterparts. Before the first start, a card
    /// saying what to do.
    fn live_center(&mut self, ui: &mut egui::Ui) {
        let dim = Color32::from_rgb(100, 116, 139);
        let running = self.live.is_running();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("LIVE CAPTURE").strong().color(Color32::from_rgb(203, 213, 225)).size(10.0));
            ui.label(egui::RichText::new("|").color(Color32::from_rgb(71, 85, 105)));
            let t = &self.live.tape;
            let clock = format!("{:02}:{:04.1}", (t.seconds / 60.0) as u64, t.seconds % 60.0);
            let (state, color) = match (running, t.state) {
                (false, _) => ("Not monitoring".to_string(), dim),
                (true, TapeState::Idle) => ("Monitoring, not recording".to_string(), Color32::from_rgb(52, 211, 153)),
                (true, TapeState::Recording) => (format!("REC {}  {}", clock, self.live_take_line()), Color32::from_rgb(248, 113, 113)),
                (true, TapeState::Paused) => (format!("PAUSED {}  {}", clock, self.live_take_line()), Color32::from_rgb(251, 191, 36)),
            };
            ui.label(egui::RichText::new(state).monospace().strong().color(color).size(10.0));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let w = &mut self.live.wave;
                egui::ComboBox::from_id_source("combo_wave_view")
                    .width(60.0)
                    .selected_text(format!("{} s", w.view_secs))
                    .show_ui(ui, |ui| {
                        for s in crate::live::WAVE_VIEW_SECS {
                            ui.selectable_value(&mut w.view_secs, s, format!("{} s", s));
                        }
                    });
                ui.label(egui::RichText::new("View:").color(dim).size(10.0));
                let mut frozen = w.frozen_at.is_some();
                if ui
                    .checkbox(&mut frozen, egui::RichText::new("Freeze").size(10.0))
                    .on_hover_text("Hold the view to look at a spot; audio, recording and the buffer carry on (last 2 minutes kept)")
                    .changed()
                {
                    w.set_frozen(frozen);
                }
            });
        });
        ui.add_space(4.0);

        render_take_timeline(ui, &self.live.timeline, self.live.tape.state == TapeState::Recording);
        ui.add_space(4.0);

        let h = (ui.available_height() - BELOW_WAVEFORM_H).max(120.0);
        if running || !self.live.wave.columns.is_empty() {
            render_live_waveform(ui, &self.live.wave, h);
            return;
        }
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 4.0, Color32::from_rgb(15, 23, 42));
        p.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)));

        p.text(
            rect.center() - egui::vec2(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            "NOT MONITORING",
            egui::FontId::monospace(30.0),
            Color32::from_rgb(148, 163, 184),
        );
        p.text(
            rect.center() + egui::vec2(0.0, 18.0),
            egui::Align2::CENTER_CENTER,
            "Pick the input and output above, then Start Monitoring. Set the level with the meters below, then press Rec before dropping the needle.",
            egui::FontId::proportional(13.0),
            dim,
        );
    }

    fn live_take_line(&self) -> String {
        let t = &self.live.tape;
        let mut s = t.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        if t.pauses > 0 {
            s += &format!("   |   {} pause(s)", t.pauses);
        }
        if t.dropped > 0 {
            s += &format!("   |   {} FRAMES LOST", t.dropped);
        }
        s
    }

    /// File mode centre: album timeline and the stacked waveforms. Returns the exposed clicks
    /// the current settings miss in view.
    fn file_center(&mut self, ui: &mut egui::Ui) -> Vec<ClickMarker> {
        // Timeline Mini-Map Header
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("FULL ALBUM TIMELINE & CLICK MAP").strong().color(Color32::from_rgb(203, 213, 225)).size(10.0));
            ui.label(egui::RichText::new("|").color(Color32::from_rgb(71, 85, 105)));

            if let Some(ref t) = self.track {
                ui.label(egui::RichText::new(format!("{} frames", t.total_frames)).monospace().color(Color32::from_rgb(148, 163, 184)).size(10.0));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match self.analysis.as_ref() {
                        Some(a) => {
                            ui.label(egui::RichText::new(format!("{}  |  {} problem spots", a.condition, a.problem_spots.len())).color(Color32::from_rgb(129, 140, 248)).strong().size(11.0));
                            ui.label(egui::RichText::new("Heat = exposed clicks  |  Triangles = problem spots").color(Color32::from_rgb(100, 116, 139)).size(10.0));
                        }
                        None => {
                            ui.label(egui::RichText::new("Not analyzed").color(Color32::from_rgb(100, 116, 139)).size(10.0));
                        }
                    }
                });
            } else {
                ui.label(egui::RichText::new("No file loaded").color(Color32::from_rgb(100, 116, 139)).size(10.0));
            }
        });

        // Timeline Mini-Map View
        render_timeline_mini_map(
            ui,
            self.track.as_ref(),
            &mut self.view_start_frame,
            &mut self.view_end_frame,
            &self.timeline_heatmap,
            self.analysis.as_deref(),
        );

        ui.add_space(6.0);

        // Audacity Stacked Waveforms View
        let available_w = ui.available_width();
        let num_columns = (available_w / 2.0).ceil() as usize;
        self.update_viewport_cache_if_needed(num_columns);
        let missed = self.missed_clicks_in_view();

        // The waveform takes the height the transport bar and controls below it leave.
        let channel_h = ((ui.available_height() - BELOW_WAVEFORM_H) / 2.0).clamp(90.0, 420.0);

        let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed);
        let wave_top = ui.cursor().top();
        let action = render_waveform_stack(
            ui,
            self.track.as_ref(),
            self.view_start_frame,
            self.view_end_frame,
            &mut self.selection_start,
            &mut self.selection_end,
            playhead,
            self.dsp_tuning.audition_mode,
            channel_h,
            &mut self.is_dragging_selection,
            &self.viewport_cache,
            &missed,
        );
        self.waveform_action(action);
        let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), wave_top..=ui.cursor().top());
        self.wheel_navigate(ui, rect);
        if self.view_moved_at.is_some_and(|t| t.elapsed() < PREVIEW_SETTLE) {
            ui.ctx().request_repaint_after(PREVIEW_SETTLE);
        }
        missed
    }

    /// Live mode transport bar: the recorder's Rec / Pause / Stop, where takes go, and the
    /// current take's clock. Same buttons and colours as the file transport.
    fn live_transport(&mut self, ui: &mut egui::Ui) {
        let running = self.live.is_running();
        let state = self.live.tape.state;
        let text = |s: &str| egui::RichText::new(s).color(Color32::WHITE).strong().size(13.0);
        let idle_fill = Color32::from_rgb(51, 65, 85);

        ui.horizontal(|ui| {
            // Rec
            let rec_fill = if state == TapeState::Recording { Color32::from_rgb(220, 38, 38) } else { Color32::from_rgb(153, 27, 27) };
            if ui
                .add_enabled(running && state == TapeState::Idle, egui::Button::new(text(" ⏺ Rec ")).fill(rec_fill))
                .on_hover_text("Record the raw input (before declick) to a new WAV")
                .on_disabled_hover_text(if running { "Already recording" } else { "Start monitoring first" })
                .clicked()
            {
                match self.live.record_start() {
                    Ok(p) => self.status_message = Some(format!("Recording to {}", p.display())),
                    Err(e) => self.status_message = Some(format!("Could not record: {}", e)),
                }
            }

            // Pause / Resume
            let paused = state == TapeState::Paused;
            let pause_fill = if paused { Color32::from_rgb(217, 119, 6) } else { idle_fill };
            if ui
                .add_enabled(state != TapeState::Idle, egui::Button::new(text(if paused { " ▶ Resume " } else { " ⏸ Pause " })).fill(pause_fill))
                .on_hover_text("Pause skips audio and keeps the same file; the join is marked with an Audacity label")
                .clicked()
            {
                if let Err(e) = self.live.record_pause_toggle() {
                    self.status_message = Some(format!("Error: {}", e));
                }
            }

            // Stop
            if ui
                .add_enabled(state != TapeState::Idle, egui::Button::new(text(" ⏹ Stop ")).fill(Color32::from_rgb(71, 85, 105)))
                .on_hover_text("Finish the take: WAV, settings sidecar and labels")
                .clicked()
            {
                self.status_message = Some(match self.live.record_stop() {
                    Ok(t) => crate::live::take_summary(&t),
                    Err(e) => format!("Error: {}", e),
                });
            }

            ui.separator();

            // Where the next take goes
            ui.add_enabled_ui(state == TapeState::Idle, |ui| {
                ui.label(egui::RichText::new("Name:").color(Color32::from_rgb(203, 213, 225)));
                ui.add(egui::TextEdit::singleline(&mut self.live.take_name).desired_width(140.0).hint_text("album / record"));
                if ui
                    .add(egui::Button::new(egui::RichText::new("Folder...").color(Color32::WHITE)).fill(idle_fill))
                    .on_hover_text(self.live.folder.display().to_string())
                    .clicked()
                {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&self.live.folder).pick_folder() {
                        self.live.folder = dir;
                    }
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let t = &self.live.tape;
                let file_of = |p: &std::path::Path| p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                match state {
                    TapeState::Idle => {
                        let next = self.live.next_take();
                        ui.label(egui::RichText::new(file_of(&next)).monospace().color(Color32::from_rgb(148, 163, 184)).size(11.0))
                            .on_hover_text(next.display().to_string());
                        ui.label(egui::RichText::new("Next take:").color(Color32::from_rgb(148, 163, 184)));
                    }
                    TapeState::Recording | TapeState::Paused => {
                        let clock = format!("{:02}:{:04.1}", (t.seconds / 60.0) as u64, t.seconds % 60.0);
                        let (label, color) = if state == TapeState::Recording {
                            ("REC", Color32::from_rgb(248, 113, 113))
                        } else {
                            ("PAUSED", Color32::from_rgb(251, 191, 36))
                        };
                        if t.dropped > 0 {
                            ui.label(egui::RichText::new(format!("{} frames lost!", t.dropped)).strong().color(Color32::from_rgb(248, 113, 113)));
                        }
                        if let Some(p) = &t.path {
                            ui.label(egui::RichText::new(file_of(p)).monospace().color(Color32::from_rgb(148, 163, 184)).size(11.0))
                                .on_hover_text(p.display().to_string());
                        }
                        ui.label(egui::RichText::new(clock).monospace().strong().color(Color32::WHITE).size(13.0));
                        ui.label(egui::RichText::new(label).strong().color(color).size(13.0));
                    }
                }
            });
        });
    }

    /// Loads the last recording in File mode for analysis and rendering.
    pub fn open_last_take(&mut self) {
        if let Some(t) = self.live.last_take.clone() {
            self.set_engine_mode(EngineMode::File);
            self.load_file_from_path(t.wav_path);
        }
    }

    pub fn open_audio_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Audio Files", &["wav", "flac", "aiff"])
            .pick_file()
        {
            self.load_file_from_path(path);
        }
    }

    pub fn load_file_from_path(&mut self, path: PathBuf) {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.wav")
            .to_string();

        match cr_audio_io::AudioFileReader::open(&path) {
            Ok(mut reader) => {
                let frames = match reader.read_all_frames() {
                    Ok(f) => f,
                    Err(e) => {
                        self.status_message = Some(format!("Failed to read audio frames: {}", e));
                        return;
                    }
                };

                let total_frames = frames.len();
                if total_frames == 0 {
                    self.status_message = Some("Audio file contains no frames".to_string());
                    return;
                }

                let sample_rate = reader.meta.sample_rate;
                let (ov_l, ov_r) = TrackData::compute_overview(&frames, 1200);

                let frames_arc = Arc::new(frames);

                // Update audio player with the new buffer and sample rate
                self.player.load_audio(Arc::clone(&frames_arc), sample_rate);
                self.player.update_tuning(self.dsp_tuning.clone());

                let sheet_msg = self.tracks.open_for(Some(&path), &filename, sample_rate, total_frames as u64);
                self.track = Some(TrackData {
                    path: Some(path),
                    filename: filename.clone(),
                    sample_rate,
                    channels: reader.meta.channels,
                    bits_per_sample: reader.meta.bits_per_sample,
                    total_frames,
                    frames_internal: frames_arc,
                    overview_min_max_l: ov_l,
                    overview_min_max_r: ov_r,
                });
                self.analysis = None;
                self.tune_report = None;
                self.dsp_tuning.dethump = false;
                self.dsp_tuning.thump_spans = Arc::new(Vec::new());
                self.player.update_tuning(self.dsp_tuning.clone());
                self.timeline_heatmap.clear();
                self.last_render_levels = None;

                self.view_start_frame = 0;
                self.view_end_frame = (sample_rate as usize * 15).min(total_frames);
                self.selection_start = 0;
                self.selection_end = 0;
                self.viewport_cache = ViewportWaveformCache::default();

                self.status_message = Some(match (self.engine_mode, sheet_msg) {
                    (EngineMode::Tracks, Some(m)) => format!("Loaded: {}. {}", filename, m),
                    (EngineMode::Tracks, None) => format!("Loaded: {}. Look up its Discogs release to place the tracks.", filename),
                    _ => format!(
                        "Loaded: {} ({} frames, {} Hz). Play it, or Analyze Album to grade the record.",
                        filename, total_frames, sample_rate
                    ),
                });
                if self.engine_mode == EngineMode::Tracks {
                    self.start_track_envelope();
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to open audio file: {}", e));
            }
        }
    }

    /// Computes the original and output waveform columns and repair markers for the visible
    /// window, through the same declick engine and post chain as the render. Re-evaluates
    /// whenever the view or the settings change. Views longer than `PREVIEW_MAX_SECS` show the
    /// original only.
    pub fn update_viewport_cache_if_needed(&mut self, num_columns: usize) {
        if num_columns == 0 {
            return;
        }
        let track = match self.track.as_ref() {
            Some(t) if t.total_frames > 0 => t,
            _ => {
                self.viewport_cache = ViewportWaveformCache::default();
                return;
            }
        };

        let v_start = self.view_start_frame;
        let v_end = self.view_end_frame.min(track.total_frames);
        if v_end <= v_start {
            return;
        }
        let view_len = v_end - v_start;
        // Tracks mode shows and plays the file as it is.
        let settling = self.view_moved_at.is_some_and(|t| t.elapsed() < PREVIEW_SETTLE);
        let previewed = self.engine_mode == EngineMode::File && !settling && view_len <= PREVIEW_MAX_SECS * track.sample_rate as usize;
        let params = if previewed { Some(render_settings(&self.dsp_tuning)) } else { None };

        // Cache hit check
        if self.viewport_cache.view_start == v_start
            && self.viewport_cache.view_end == v_end
            && self.viewport_cache.width_cols == num_columns
            && self.viewport_cache.params == params
            && !self.viewport_cache.orig_l.is_empty()
        {
            return;
        }

        // Declicked (and dethumped) and final audio for the view. Before it: warm-up history
        // (the same pad the auto-tuner uses) so detection has settled, plus the context of the
        // thump repair and the post chain; after it, that context (both look ahead).
        let processed: Option<(Vec<(i32, i32)>, Vec<(f64, f64)>)> = params.as_ref().map(|(p, thumps, post)| {
            let sr = track.sample_rate;
            let ctx = block_context(sr, thumps, post);
            let lead = (warm_up_frames(sr) + ctx).min(v_start);
            let tail_end = (v_end + ctx).min(track.total_frames);
            let slice = &track.frames_internal[v_start - lead..tail_end];
            let (mut declicked, _) = cr_dsp::process::Processor::process_slice_aligned(p.clone(), sr, slice);
            cr_dsp::thump::repair_spans(&mut declicked, v_start - lead, thumps);
            let keep = lead..lead + view_len;
            let out = cr_dsp::post::process_block(&declicked, sr, post, keep.clone());
            (declicked[keep].to_vec(), out)
        });

        let frames_per_col = view_len as f64 / num_columns as f64;
        let mut orig_l = vec![0.0f32; num_columns];
        let mut rep_l = vec![0.0f32; num_columns];
        let mut orig_r = vec![0.0f32; num_columns];
        let mut rep_r = vec![0.0f32; num_columns];
        let mut runs_l = RepairRuns::new(0);
        let mut runs_r = RepairRuns::new(1);

        for i in 0..view_len {
            let frame = v_start + i;
            let col = ((i as f64 / frames_per_col) as usize).min(num_columns - 1);
            let (ol, or) = track.frames_internal[frame];
            let (outl, outr) = processed.as_ref().map_or((ol as f64, or as f64), |p| p.1[i]);
            orig_l[col] = orig_l[col].max((ol as f32 / FULL_SCALE).abs());
            orig_r[col] = orig_r[col].max((or as f32 / FULL_SCALE).abs());
            rep_l[col] = rep_l[col].max((outl as f32 / FULL_SCALE).abs());
            rep_r[col] = rep_r[col].max((outr as f32 / FULL_SCALE).abs());
            // Markers show the declicker's repairs, not the filters' gentle changes.
            if let Some((declicked, _)) = processed.as_ref() {
                runs_l.push(frame, ol, declicked[i].0);
                runs_r.push(frame, or, declicked[i].1);
            }
        }

        self.viewport_cache = ViewportWaveformCache {
            view_start: v_start,
            view_end: v_end,
            width_cols: num_columns,
            params,
            repairs_previewed: previewed,
            orig_l,
            rep_l,
            orig_r,
            rep_r,
            viewport_clicks: merge_markers(runs_l, runs_r),
        };
    }

    /// Background render: declick, then the post chain with its level check, to a WAV file,
    /// with a progress popup and cancellation. Same engine and settings as preview and audition.
    pub fn render_album_batch(&mut self) {
        let track = match self.track.as_ref() {
            Some(t) => t,
            None => {
                self.status_message = Some("No audio track loaded to render".to_string());
                return;
            }
        };

        let stem = std::path::Path::new(&track.filename).file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let default_name = format!("{}_repaired.wav", stem);
        let dest_path = match rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("WAV Audio", &["wav"])
            .save_file()
        {
            Some(p) => p,
            None => return,
        };

        let frames_internal = Arc::clone(&track.frames_internal);
        let sample_rate = track.sample_rate;
        let total_frames = track.total_frames;
        let meta = cr_audio_io::AudioMetadata {
            channels: track.channels,
            sample_rate,
            bits_per_sample: track.bits_per_sample,
            is_float: false,
            total_frames,
        };
        let (params, thumps, post) = render_settings(&self.dsp_tuning);
        let note = self.render_note(&dest_path);

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_cb = Arc::clone(&cancel_flag);
        let tx = self.job_sender.clone();

        self.active_progress = Some(ProgressState {
            title: "Rendering".to_string(),
            description: "Declicking...".to_string(),
            progress: 0.0,
            cancel_flag,
        });

        let tx_panic = tx.clone();
        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let start_time = Instant::now();
                let fail = |msg: String| {
                    let _ = tx.send(JobMessage::Error(msg));
                };
                let cancelled = || {
                    let _ = std::fs::remove_file(&dest_path);
                    let _ = tx.send(JobMessage::Cancelled);
                };
                let mut writer = match cr_audio_io::AudioFileWriter::create(&dest_path, meta) {
                    Ok(w) => w,
                    Err(e) => return fail(format!("Failed to create destination file: {}", e)),
                };

                // 1. Declick. With the thump repair or the post chain the result stays in memory:
                //    thumps are repaired over the whole of it, and the level check measures the
                //    chain's output before deciding on an album gain.
                let use_post = !post.is_identity();
                let buffered = use_post || !thumps.is_empty();
                let declick_share = if use_post { 0.8 } else { 1.0 };
                let mut processor = cr_dsp::process::Processor::new(params, sample_rate, Some(total_frames));
                let mut declicked: Vec<(i32, i32)> = Vec::with_capacity(if buffered { total_frames } else { 0 });
                let mut out_chunk = Vec::with_capacity(65536);
                let mut idx = 0;
                while idx < total_frames {
                    if cancel_cb.load(Ordering::Relaxed) {
                        drop(writer);
                        return cancelled();
                    }
                    let end = (idx + 65536).min(total_frames);
                    processor.process_chunk(&frames_internal[idx..end], &mut out_chunk);
                    if buffered {
                        declicked.extend_from_slice(&out_chunk);
                    } else if let Err(e) = writer.write_chunk(&out_chunk) {
                        return fail(format!("Error writing frames: {}", e));
                    }
                    idx = end;
                    let frac = idx as f32 / total_frames as f32;
                    let speed = (idx as f64 / sample_rate as f64) / start_time.elapsed().as_secs_f64().max(0.01);
                    let _ = tx.send(JobMessage::Progress {
                        fraction: declick_share * frac,
                        description: format!("Declicking: {:.0}% ({:.0}x real time)", frac * 100.0, speed),
                    });
                }
                let flushed = processor.flush();
                if buffered {
                    declicked.extend(flushed);
                    declicked.resize(total_frames, (0, 0));
                    cr_dsp::thump::repair_spans(&mut declicked, 0, &thumps);
                    if !use_post {
                        if let Err(e) = writer.write_chunk(&declicked) {
                            return fail(format!("Error writing frames: {}", e));
                        }
                    }
                } else {
                    for (l, r) in flushed {
                        if let Err(e) = writer.write_frame(l, r) {
                            return fail(format!("Error writing frames: {}", e));
                        }
                    }
                }

                // 2. Post chain: write while measuring; if it pushed peaks over full scale,
                //    write again with one album gain.
                let mut levels = None;
                if use_post {
                    let write_post = |writer: &mut cr_audio_io::AudioFileWriter, settings: &cr_dsp::post::PostSettings, base: f32| {
                        let mut meter = cr_dsp::post::LevelMeter::new(sample_rate);
                        let mut result = Ok(());
                        let mut done = 0usize;
                        cr_dsp::post::run_post(&declicked, sample_rate, settings, |block| {
                            meter.feed(block);
                            for &(l, r) in block {
                                if result.is_ok() {
                                    result = writer.write_frame(cr_dsp::post::to_internal(l), cr_dsp::post::to_internal(r));
                                }
                            }
                            done += block.len();
                            let _ = tx.send(JobMessage::Progress {
                                fraction: base + 0.1 * done as f32 / total_frames as f32,
                                description: "Corrections and level check...".to_string(),
                            });
                        });
                        result.map(|_| meter.finish())
                    };
                    let mut report = match write_post(&mut writer, &post, declick_share) {
                        Ok(r) => r,
                        Err(e) => return fail(format!("Error writing frames: {}", e)),
                    };
                    let gain_db = cr_dsp::post::album_gain_db(&report);
                    if gain_db < 0.0 {
                        drop(writer);
                        writer = match cr_audio_io::AudioFileWriter::create(&dest_path, meta) {
                            Ok(w) => w,
                            Err(e) => return fail(format!("Failed to recreate destination file: {}", e)),
                        };
                        let mut with_gain = post.clone();
                        with_gain.gain_db = gain_db;
                        report = match write_post(&mut writer, &with_gain, declick_share + 0.1) {
                            Ok(r) => r,
                            Err(e) => return fail(format!("Error writing frames: {}", e)),
                        };
                    }
                    levels = Some((gain_db, report));
                }

                if let Err(e) = writer.finalize() {
                    return fail(format!("Error finalizing file: {}", e));
                }
                let elapsed = start_time.elapsed().as_secs_f64();
                let speedup = (total_frames as f64 / sample_rate as f64) / elapsed.max(0.001);
                let level_line = match levels.as_ref() {
                    Some((gain, r)) => format!(
                        "Output:       peak {:.2} dBFS, true peak {:.2} dBTP, {:.1} LUFS, album gain {:.2} dB\n",
                        r.sample_peak_dbfs, r.true_peak_dbtp, r.loudness_lufs, gain
                    ),
                    None => String::new(),
                };
                let mut note_path = dest_path.as_os_str().to_os_string();
                note_path.push(".cleangroove-render.txt");
                let _ = std::fs::write(note_path, format!("{}{}Time:         {:.1} s\n", note, level_line, elapsed));
                let dest_name = dest_path.file_name().unwrap_or_default().to_string_lossy().to_string();
                let _ = tx.send(JobMessage::RenderFinished { dest_filename: dest_name, elapsed_secs: elapsed, speedup, levels });
            }));

            if let Err(e) = res {
                let err_msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Worker thread panic".to_string()
                };
                let _ = tx_panic.send(JobMessage::Error(format!("Rendering failed: {}", err_msg)));
            }
        });
    }

    /// What a render does to the record, written next to the output as `<file>.cleangroove-render.txt`
    /// (and copied into the album folder when its tracks are exported).
    fn render_note(&self, dest: &std::path::Path) -> String {
        let t = &self.dsp_tuning;
        let on = |b: bool| if b { "on" } else { "off" };
        let mut s = format!("CleanGroove {} render\n\n", env!("CARGO_PKG_VERSION"));
        s += &format!("Date:         {} (UTC)\n", cr_rt::tape::format_system_time_iso8601(std::time::SystemTime::now()));
        if let Some(src) = self.track.as_ref().and_then(|tr| tr.path.as_ref()) {
            s += &format!("Source:       {}\n", src.display());
        }
        s += &format!("Output:       {}\n", dest.display());
        s += &format!("Method:       {:?}\n", t.method);
        s += &format!("DeClick:      {}   DeCrackle: {}   Pitch protection: {}   Reverse: {}\n", t.declick, t.decrackle, on(t.pitch_protection), on(t.reverse));
        s += &format!("Stereo>Mono:  {}   Thump repair: {} ({} spans)\n", on(t.stereo_to_mono), on(t.dethump), if t.dethump { t.thump_spans.len() } else { 0 });
        if t.post_enabled {
            s += &format!(
                "Corrections:  polarity flip {}, balance {}, azimuth {}, subsonic {}, elliptical {}\n",
                on(t.flip_polarity),
                if t.balance_on { format!("{:+.2} dB", t.balance_db) } else { "off".into() },
                if t.azimuth_on { format!("{:+.1} us", t.azimuth_us) } else { "off".into() },
                on(t.subsonic_on),
                if t.elliptical_on { format!("{:.0} Hz", t.elliptical_hz) } else { "off".into() },
            );
        } else {
            s += "Corrections:  off\n";
        }
        if let Some(a) = self.analysis.as_ref() {
            s += &format!("Analysis:     {}, {:.2} audible clicks/s between tracks\n", a.condition, a.audible_clicks_per_sec);
        }
        s
    }

    /// Background album analysis followed by the auto-tuner (which reuses it), with progress and
    /// cancellation. Results land in the record analysis panel.
    pub fn start_album_analysis(&mut self) {
        let track = match self.track.as_ref() {
            Some(t) => t,
            None => {
                self.status_message = Some("Load a record first to analyze it".to_string());
                return;
            }
        };

        let frames = Arc::clone(&track.frames_internal);
        let sample_rate = track.sample_rate;
        let channels = track.channels;
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_cb = Arc::clone(&cancel_flag);
        let tx = self.job_sender.clone();

        self.active_progress = Some(ProgressState {
            title: "Analyze Album".to_string(),
            description: "Measuring levels...".to_string(),
            progress: 0.0,
            cancel_flag,
        });

        std::thread::spawn(move || {
            let start = Instant::now();
            // The full pass is about 85% of the work, the auto-tuner the rest.
            let tx_a = tx.clone();
            let analysis = cr_dsp::analysis::analyze_album(
                &frames,
                sample_rate,
                channels,
                move |frac, msg| {
                    let _ = tx_a.send(JobMessage::Progress { fraction: 0.85 * frac, description: msg.to_string() });
                },
                &cancel_cb,
            );
            let Some(analysis) = analysis else {
                let _ = tx.send(JobMessage::Cancelled);
                return;
            };
            let tx_t = tx.clone();
            let report = cr_dsp::autotune::autotune_audio_with_analysis(
                &frames,
                sample_rate,
                channels,
                Some(&analysis),
                move |frac, msg| {
                    let _ = tx_t.send(JobMessage::Progress {
                        fraction: 0.85 + 0.15 * frac,
                        description: format!("Suggesting settings: {}", msg),
                    });
                },
            );
            if cancel_cb.load(Ordering::Relaxed) {
                let _ = tx.send(JobMessage::Cancelled);
                return;
            }
            let _ = tx.send(JobMessage::AnalysisFinished {
                analysis: Box::new(analysis),
                report: Box::new(report),
                elapsed_secs: start.elapsed().as_secs_f64(),
            });
        });
    }

    /// Applies the auto-tuner's suggested settings and pushes them to the audio player.
    pub fn apply_suggested_settings(&mut self) {
        let Some(report) = self.tune_report.as_ref() else { return };
        let s = &report.recommended;
        self.dsp_tuning.declick = s.dcl.max(0) as u32;
        self.dsp_tuning.decrackle = s.dcr.max(0) as u32;
        self.dsp_tuning.pitch_protection = s.detect_pitch;
        self.dsp_tuning.reverse = s.reverse;
        self.dsp_tuning.method = match s.method {
            cr_dsp::params::Method::Simple => DeclickMethod::Simple,
            cr_dsp::params::Method::Wavelet => DeclickMethod::Wavelet,
            cr_dsp::params::Method::WaveletX2 | cr_dsp::params::Method::WaveletX3 => DeclickMethod::WaveletX2,
        };
        // Corrections: the analysis' suggestion (a mono record is summed after aligning).
        match self.analysis.as_ref() {
            Some(a) => {
                let p = a.suggested_post();
                let t = &mut self.dsp_tuning;
                t.post_enabled = true;
                t.flip_polarity = p.flip_right_polarity;
                t.balance_on = p.balance_db != 0.0;
                t.azimuth_on = p.azimuth_us != 0.0;
                t.subsonic_on = p.subsonic_hz.is_some();
                t.elliptical_on = p.elliptical_hz.is_some();
                t.elliptical_hz = p.elliptical_hz.unwrap_or(cr_dsp::analysis::ELLIPTICAL_HZ);
                t.stereo_to_mono = p.mono_sum;
            }
            None => self.dsp_tuning.stereo_to_mono = s.mode != cr_dsp::params::ChannelMode::Stereo,
        }
        self.player.update_tuning(self.dsp_tuning.clone());
        self.status_message = Some("Applied the suggested settings.".to_string());
    }

    /// Shows a problem spot with a little context, selects it as the loop region and puts the
    /// playhead at its start, ready to audition.
    pub fn jump_to_spot(&mut self, start: usize, len: usize) {
        let Some(track) = self.track.as_ref() else { return };
        let sr = track.sample_rate as usize;
        let context = 2 * sr;
        self.view_start_frame = start.saturating_sub(context);
        self.view_end_frame = (start + len + context).min(track.total_frames);
        self.selection_start = start;
        self.selection_end = (start + len).min(track.total_frames);
        self.player.transport.loop_start_frame.store(self.selection_start, Ordering::Relaxed);
        self.player.transport.loop_end_frame.store(self.selection_end, Ordering::Relaxed);
        self.player.transport.seek_to(self.selection_start);
        let t = start as f64 / sr as f64;
        self.status_message = Some(format!(
            "Problem spot at {:02}:{:02} selected for looping. Space plays it; N goes to the next one.",
            (t / 60.0).floor() as u32,
            (t % 60.0).floor() as u32
        ));
    }

    /// Jumps to the first problem spot after the current selection, wrapping to the first.
    pub fn next_problem_spot(&mut self) {
        let Some(analysis) = self.analysis.as_ref() else {
            self.status_message = Some("Analyze the album first to find its problem spots.".to_string());
            return;
        };
        let after = self.selection_start.max(self.selection_end);
        let spot = analysis
            .problem_spots
            .iter()
            .find(|s| s.start_frame >= after)
            .or_else(|| analysis.problem_spots.first())
            .map(|s| (s.start_frame, s.len_frames));
        match spot {
            Some((start, len)) => self.jump_to_spot(start, len),
            None => self.status_message = Some("No problem spots: no exposed clicks in the music.".to_string()),
        }
    }

    /// Exposed clicks found by the analysis in the visible window that the current settings do
    /// not repair (only when the window's repairs are previewed).
    pub fn missed_clicks_in_view(&self) -> Vec<ClickMarker> {
        let Some(analysis) = self.analysis.as_ref() else { return Vec::new() };
        if !self.viewport_cache.repairs_previewed || self.viewport_cache.orig_l.is_empty() {
            return Vec::new();
        }
        let repaired = &self.viewport_cache.viewport_clicks;
        analysis
            .events_in(self.viewport_cache.view_start, self.viewport_cache.view_end)
            .iter()
            .filter(|e| analysis.is_exposed(e))
            .filter(|e| {
                let (s, end) = (e.frame.saturating_sub(8), e.frame + e.len as usize + 8);
                !repaired
                    .iter()
                    .any(|c| c.channel == e.channel as usize && c.frame < end && s < c.frame + c.length)
            })
            .map(|e| ClickMarker {
                frame: e.frame,
                channel: e.channel as usize,
                length: e.len as usize,
                amplitude: 10f32.powf(e.peak_db / 20.0),
            })
            .collect()
    }

    /// Exposed clicks per timeline bin, normalised to the busiest bin. Silence between tracks
    /// is left out so the map shows where damage sits in the music.
    pub fn compute_timeline_heatmap(analysis: &AlbumAnalysis, bins: usize) -> Vec<f32> {
        let n = analysis.block_exposed.len();
        if n == 0 {
            return Vec::new();
        }
        let mut heat = vec![0.0f32; bins];
        for (b, &count) in analysis.block_exposed.iter().enumerate() {
            if count > 0 && analysis.gap_blocks.binary_search(&b).is_err() {
                heat[(b * bins / n).min(bins - 1)] += count as f32;
            }
        }
        let max = heat.iter().cloned().fold(0.0, f32::max);
        if max > 0.0 {
            for h in &mut heat {
                *h /= max;
            }
        }
        heat
    }
}

impl eframe::App for CleanGrooveApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle screenshot requests
        for event in &ctx.input(|i| i.raw.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                if let Some(ref path) = self.screenshot_target {
                    let w = image.size[0] as u32;
                    let h = image.size[1] as u32;
                    let mut bytes = Vec::with_capacity((w * h * 4) as usize);
                    for p in &image.pixels {
                        bytes.push(p.r());
                        bytes.push(p.g());
                        bytes.push(p.b());
                        bytes.push(p.a());
                    }
                    if let Err(e) = image::save_buffer(path, &bytes, w, h, image::ColorType::Rgba8) {
                        eprintln!("Failed to save screenshot: {}", e);
                    } else {
                        println!("Screenshot successfully captured to: {:?}", path);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        if self.screenshot_target.is_some() {
            self.screenshot_frame_count += 1;
            // Allow 6 frames for layout & waveform canvas to complete initial render
            if self.screenshot_frame_count == 6 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            ctx.request_repaint();
        }

        // Process background task messages from worker threads
        while let Ok(msg) = self.job_receiver.try_recv() {
            match msg {
                JobMessage::Progress { fraction, description } => {
                    if let Some(ref mut p) = self.active_progress {
                        p.progress = fraction;
                        p.description = description;
                    }
                }
                JobMessage::AnalysisFinished { analysis, report, elapsed_secs } => {
                    self.active_progress = None;
                    self.timeline_heatmap = Self::compute_timeline_heatmap(&analysis, 300);
                    self.status_message = Some(format!(
                        "Analysis complete in {:.1} s: {}. {} problem spots.",
                        elapsed_secs,
                        analysis.condition,
                        analysis.problem_spots.len()
                    ));
                    // Measured correction values (switched on by Apply, or by hand).
                    self.dsp_tuning.balance_db = analysis.channels.balance_db;
                    self.dsp_tuning.azimuth_us = analysis.channels.azimuth_us;
                    // Thump spans are kept; the repair stays off until switched on.
                    self.dsp_tuning.thump_spans = Arc::new(cr_dsp::thump::thump_spans(&analysis.thumps, analysis.sample_rate));
                    self.player.update_tuning(self.dsp_tuning.clone());
                    self.analysis = Some(Arc::from(analysis));
                    self.tune_report = Some(*report);
                }
                JobMessage::RenderFinished { dest_filename, elapsed_secs, speedup, levels } => {
                    self.active_progress = None;
                    let level_text = match levels {
                        Some((gain, r)) => format!(
                            " Peak {:.2} dBFS, true peak {:.2} dBTP, {:.1} LUFS{}.",
                            r.sample_peak_dbfs,
                            r.true_peak_dbtp,
                            r.loudness_lufs,
                            if gain < 0.0 { format!(", album gain {:.2} dB applied", gain) } else { String::new() }
                        ),
                        None => String::new(),
                    };
                    self.status_message = Some(format!(
                        "Rendered {:?} in {:.1} s ({:.0}x real time).{}",
                        dest_filename, elapsed_secs, speedup, level_text
                    ));
                    self.last_render_levels = levels;
                }
                JobMessage::ExportFinished { album_dir, files, seconds } => {
                    self.active_progress = None;
                    self.status_message = Some(format!(
                        "Exported {} tracks to {} in {:.1} s, each checked against the rip sample for sample.",
                        files,
                        album_dir.display(),
                        seconds
                    ));
                    self.tracks.last_export = Some(album_dir);
                    self.tracks.export_open = true;
                }
                JobMessage::Cancelled => {
                    self.active_progress = None;
                    self.status_message = Some("Operation cancelled by user.".to_string());
                }
                JobMessage::Error(err) => {
                    self.active_progress = None;
                    self.status_message = Some(format!("Error: {}", err));
                }
            }
        }

        // DAW Page-Scrolling: Keeps waveform view stationary for visual stability and zero DSP overhead.
        // Pages forward by ~60% only when the playhead approaches the 75% boundary of the screen.
        if self.player.transport.is_playing.load(Ordering::Relaxed) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));

            if let Some(ref track) = self.track {
                let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed);
                let view_len = self.view_end_frame.saturating_sub(self.view_start_frame);
                if view_len > 0 && view_len < track.total_frames {
                    let page_threshold = self.view_start_frame + (view_len * 3) / 4;

                    if playhead >= page_threshold && self.view_end_frame < track.total_frames {
                        // Page forward by ~60% of visible duration
                        let advance = (view_len * 6) / 10;
                        let new_start = (self.view_start_frame + advance).min(track.total_frames.saturating_sub(view_len));
                        let new_end = (new_start + view_len).min(track.total_frames);
                        self.view_start_frame = new_start;
                        self.view_end_frame = new_end;
                    } else if playhead < self.view_start_frame {
                        // Playhead looped back or sought backwards before current view
                        let new_start = playhead.saturating_sub(view_len / 6);
                        let new_end = (new_start + view_len).min(track.total_frames);
                        self.view_start_frame = new_start;
                        self.view_end_frame = new_end;
                    }
                }
            }
        } else if self.active_progress.is_some() {
            ctx.request_repaint();
        }

        // Live session: meters, recorder status and messages.
        for msg in self.live.poll() {
            self.status_message = Some(msg);
        }
        if self.live.is_running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        for msg in self.tracks.poll() {
            self.status_message = Some(msg);
        }
        if self.tracks.is_working() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Drops anywhere on the window: an audio file opens it; in Tracks mode an image (a file,
        // one dragged from a browser as in Mp3tag, or a link to one) becomes the cover.
        let files_hovering = self.drop_inbox.as_ref().is_some_and(|d| d.hovering()) || ctx.input(|i| !i.raw.hovered_files.is_empty());
        if files_hovering {
            ctx.request_repaint();
        }
        let mut dropped: Vec<crate::drop::DropItem> = self.drop_inbox.as_ref().map(|d| d.take()).unwrap_or_default();
        dropped.extend(ctx.input(|i| i.raw.dropped_files.iter().filter_map(crate::drop::DropItem::from_egui).collect::<Vec<_>>()));
        for item in dropped {
            let audio = match &item {
                crate::drop::DropItem::File(p) => p
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .filter(|e| ["wav", "flac", "aif", "aiff"].contains(&e.as_str()))
                    .map(|_| p.clone()),
                _ => None,
            };
            if let Some(p) = audio {
                if self.engine_mode == EngineMode::Live {
                    self.set_engine_mode(EngineMode::File);
                }
                self.load_file_from_path(p);
            } else if self.engine_mode == EngineMode::Tracks && self.track.is_some() {
                if !self.tracks.drop_cover(&item) {
                    self.status_message = Some("Drop a JPEG, PNG, WebP or GIF image (or a link to one) to use it as the cover".to_string());
                }
            } else {
                self.status_message = Some("Drop an audio file to open it; images become the cover in Tracks mode".to_string());
            }
        }

        // Handle keyboard shortcuts. None while a text field is being typed in; the file
        // transport keys in File and Tracks mode.
        let keys = !ctx.wants_keyboard_input();
        let file_keys = keys && self.engine_mode != EngineMode::Live;
        let tracks_keys = keys && self.engine_mode == EngineMode::Tracks;
        if tracks_keys {
            let playhead = self.tracks.settle(self.player.transport.playhead_frame.load(Ordering::Relaxed) as u64);
            let sel = self.tracks.selected.filter(|&i| self.tracks.sheet.as_ref().is_some_and(|s| i < s.tracks.len()));
            if ctx.input(|i| i.key_pressed(egui::Key::S) && !i.modifiers.command) {
                self.tracks_action(TracksAction::SplitAtCursor);
            }
            if let (Some(i), Some(sheet)) = (sel, self.tracks.sheet.as_mut()) {
                if ctx.input(|inp| inp.key_pressed(egui::Key::OpenBracket)) {
                    sheet.set_start(i, playhead);
                    self.tracks.changed();
                } else if ctx.input(|inp| inp.key_pressed(egui::Key::CloseBracket)) {
                    sheet.set_end(i, playhead);
                    self.tracks.changed();
                } else if ctx.input(|inp| inp.key_pressed(egui::Key::B)) {
                    let f = sheet.tracks[i].start;
                    self.tracks_action(TracksAction::PlayAcross(f));
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::PageDown)) {
                self.jump_track(true);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::PageUp)) {
                self.jump_track(false);
            }
            // Tab / Shift+Tab: next / previous track, heard from just before its start.
            if let Some(n) = self.tracks.sheet.as_ref().map(|s| s.tracks.len()).filter(|&n| n > 0) {
                if ctx.input(|i| i.key_pressed(egui::Key::Tab)) {
                    let back = ctx.input(|i| i.modifiers.shift);
                    let i = match (sel, back) {
                        (None, _) => 0,
                        (Some(i), false) => (i + 1).min(n - 1),
                        (Some(i), true) => i.saturating_sub(1),
                    };
                    self.tracks.selected = Some(i);
                    let f = self.tracks.sheet.as_ref().unwrap().tracks[i].start;
                    self.tracks_action(TracksAction::PlayAcross(f));
                }
            }
        }
        if file_keys && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            let playing = self.player.transport.is_playing.load(Ordering::Relaxed);
            if playing {
                self.player.transport.pause();
            } else {
                self.player.transport.play();
            }
        }
        if file_keys && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.player.transport.stop();
            let start = self.selection_start.min(self.selection_end);
            self.player.transport.seek_to(start);
        }
        if file_keys && ctx.input(|i| i.key_pressed(egui::Key::Home)) {
            self.player.transport.seek_to(0);
            self.selection_start = 0;
            self.selection_end = 0;
            self.player.transport.loop_start_frame.store(0, Ordering::Relaxed);
            self.player.transport.loop_end_frame.store(0, Ordering::Relaxed);
        }
        if file_keys && ctx.input(|i| i.key_pressed(egui::Key::End)) {
            if let Some(ref t) = self.track {
                self.player.transport.seek_to(t.total_frames);
                self.player.transport.pause();
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.open_audio_file();
        }
        if keys && !tracks_keys && ctx.input(|i| i.key_pressed(egui::Key::Num1)) {
            self.set_audition(AuditionMode::OutClean);
        }
        if keys && !tracks_keys && ctx.input(|i| i.key_pressed(egui::Key::Num2)) {
            self.set_audition(AuditionMode::InBypass);
        }
        if keys && !tracks_keys && ctx.input(|i| i.key_pressed(egui::Key::Num3)) {
            self.set_audition(AuditionMode::NoiseDelta);
        }
        if file_keys && !tracks_keys && ctx.input(|i| i.key_pressed(egui::Key::N)) {
            self.next_problem_spot();
        }

        if keys && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            self.close_file();
        }
        if file_keys && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) {
            self.select_all();
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Q)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            self.show_guide = true;
        }

        // 1. Menu bar. Each entry shows its shortcut; entries that do not apply right now
        // (no file loaded, not monitoring) are greyed out rather than hidden.
        egui::TopBottomPanel::top("top_menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.label(egui::RichText::new("CleanGroove").strong().color(Color32::WHITE));
                ui.separator();
                let has_track = self.track.is_some();
                let live = self.engine_mode == EngineMode::Live;
                let monitoring = self.live.is_running();
                let tape = self.live.tape.state;
                // A menu entry with its shortcut on the right; true when clicked (and closes the menu).
                let item = |ui: &mut egui::Ui, enabled: bool, label: &str, shortcut: &str| -> bool {
                    let clicked = ui
                        .add_enabled(enabled, egui::Button::new(label).shortcut_text(shortcut))
                        .clicked();
                    if clicked {
                        ui.close_menu();
                    }
                    clicked
                };

                ui.menu_button("File", |ui| {
                    if item(ui, true, "Open Audio File...", "Ctrl+O") {
                        if live {
                            self.set_engine_mode(EngineMode::File);
                        }
                        self.open_audio_file();
                    }
                    let last = self.live.last_take.is_some() && tape == TapeState::Idle;
                    if item(ui, last, "Open Last Take", "") {
                        self.open_last_take();
                    }
                    if item(ui, has_track, "Close File", "Ctrl+W") {
                        self.close_file();
                    }
                    ui.separator();
                    if item(ui, has_track, "Render Album / Save Repaired...", "") {
                        self.render_album_batch();
                    }
                    ui.separator();
                    if item(ui, true, "Exit", "Ctrl+Q") {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("Settings", |ui| {
                    if item(ui, true, "Settings...", "") {
                        self.show_settings = true;
                    }
                    ui.separator();
                    ui.menu_button("Presets", |ui| {
                        if item(ui, true, "Vinyl LP 33 RPM (default)", "") {
                            self.apply_preset(30, 0, false, false);
                        }
                        if item(ui, true, "Shellac 78 RPM (heavy crackle)", "") {
                            self.apply_preset(70, 45, false, true);
                        }
                        if item(ui, true, "Brass / reed protection", "") {
                            self.apply_preset(40, 15, true, false);
                        }
                    });
                });

                ui.menu_button("View", |ui| {
                    for (m, label) in [(EngineMode::File, "File Mode"), (EngineMode::Live, "Live Monitor"), (EngineMode::Tracks, "Tracks")] {
                        if ui.radio(self.engine_mode == m, label).clicked() {
                            self.set_engine_mode(m);
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                    if live {
                        ui.label(egui::RichText::new("Live waveform").color(Color32::from_rgb(148, 163, 184)).size(11.0));
                        for s in crate::live::WAVE_VIEW_SECS {
                            if ui.radio(self.live.wave.view_secs == s, format!("Last {} s", s)).clicked() {
                                self.live.wave.view_secs = s;
                                ui.close_menu();
                            }
                        }
                        let mut frozen = self.live.wave.frozen_at.is_some();
                        if ui.checkbox(&mut frozen, "Freeze").changed() {
                            self.live.wave.set_frozen(frozen);
                            ui.close_menu();
                        }
                    } else {
                        if item(ui, has_track, "Zoom In", "") {
                            self.zoom(1.0 / 1.4);
                        }
                        if item(ui, has_track, "Zoom Out", "") {
                            self.zoom(1.4);
                        }
                        if item(ui, has_track, "Fit Entire Track", "") {
                            self.fit_track();
                        }
                    }
                });

                ui.menu_button("Transport", |ui| {
                    if live {
                        if monitoring {
                            if item(ui, true, "Stop Monitoring", "") {
                                self.stop_monitoring();
                            }
                        } else if item(ui, self.live.input.is_some() && self.live.output.is_some(), "Start Monitoring", "") {
                            self.start_monitoring();
                        }
                        ui.separator();
                        if item(ui, monitoring && tape == TapeState::Idle, "Record", "") {
                            match self.live.record_start() {
                                Ok(p) => self.status_message = Some(format!("Recording to {}", p.display())),
                                Err(e) => self.status_message = Some(format!("Could not record: {}", e)),
                            }
                        }
                        let pause_label = if tape == TapeState::Paused { "Resume" } else { "Pause" };
                        if item(ui, tape != TapeState::Idle, pause_label, "") {
                            if let Err(e) = self.live.record_pause_toggle() {
                                self.status_message = Some(format!("Error: {}", e));
                            }
                        }
                        if item(ui, tape != TapeState::Idle, "Stop Take", "") {
                            self.status_message = Some(match self.live.record_stop() {
                                Ok(t) => crate::live::take_summary(&t),
                                Err(e) => format!("Error: {}", e),
                            });
                        }
                    } else {
                        let playing = self.player.transport.is_playing.load(Ordering::Relaxed);
                        if item(ui, has_track, if playing { "Pause" } else { "Play" }, "Space") {
                            if playing {
                                self.player.transport.pause();
                            } else {
                                self.player.transport.play();
                            }
                        }
                        if item(ui, has_track, "Stop", "Esc") {
                            self.player.transport.stop();
                        }
                        if item(ui, has_track, "Go to Start", "Home") {
                            self.player.transport.seek_to(0);
                        }
                        if item(ui, has_track, "Go to End", "End") {
                            if let Some(ref t) = self.track {
                                self.player.transport.seek_to(t.total_frames);
                                self.player.transport.pause();
                            }
                        }
                        ui.separator();
                        if item(ui, has_track, "Select Entire Track", "Ctrl+A") {
                            self.select_all();
                        }
                        let mut looping = self.player.transport.is_looping.load(Ordering::Relaxed);
                        if ui.add_enabled(has_track, egui::Checkbox::new(&mut looping, "Loop Selection")).changed() {
                            self.player.transport.is_looping.store(looping, Ordering::Relaxed);
                        }
                        if item(ui, has_track, "Next Problem Spot", "N") {
                            self.next_problem_spot();
                        }
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Preview").color(Color32::from_rgb(148, 163, 184)).size(11.0));
                    let mode = self.dsp_tuning.audition_mode;
                    for (m, label, key) in [
                        (AuditionMode::OutClean, "Out (repaired)", "1"),
                        (AuditionMode::InBypass, "In (original)", "2"),
                        (AuditionMode::NoiseDelta, "Noise (removed clicks)", "3"),
                    ] {
                        let text = if mode == m { format!("● {}", label) } else { format!("   {}", label) };
                        if item(ui, true, &text, key) {
                            self.set_audition(m);
                        }
                    }
                });

                ui.menu_button("Tools", |ui| {
                    if item(ui, has_track && !live, "Analyze Album", "") {
                        self.start_album_analysis();
                    }
                    if item(ui, true, "Rescan Audio Devices", "") {
                        self.rescan_devices();
                    }
                });

                ui.menu_button("Help", |ui| {
                    if item(ui, true, "User Guide", "F1") {
                        self.show_guide = true;
                    }
                    if item(ui, true, "About CleanGroove", "") {
                        self.show_about = true;
                    }
                });
            });
        });

        // Menu windows.
        if self.show_settings {
            let mut open = true;
            self.services.poll();
            if self.services.is_working() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            render_settings_window(ctx, &mut open, &mut self.live, &mut self.services);
            self.show_settings = open;
        }
        if self.show_guide {
            let mut open = true;
            render_user_guide(ctx, &mut open);
            self.show_guide = open;
        }
        if self.show_about {
            if self.about_icon.is_none() {
                self.about_icon = load_icon(ctx);
            }
            let mut open = true;
            render_about(ctx, &mut open, self.about_icon.as_ref());
            self.show_about = open;
        }

        // 2. Header Strip: Track Info, Mode Switcher & Primary Action Buttons
        egui::TopBottomPanel::top("header_strip").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if self.engine_mode == EngineMode::Live {
                    match self.live.info.as_ref().filter(|_| self.live.is_running()) {
                        Some(info) => {
                            ui.label(egui::RichText::new("Monitoring:").color(Color32::from_rgb(148, 163, 184)));
                            ui.label(egui::RichText::new(format!("{}  ->  {}", info.input_device, info.output_device)).strong().color(Color32::WHITE));
                            ui.label(egui::RichText::new("|").color(Color32::from_rgb(71, 85, 105)));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{:.1} kHz | latency {:.0} ms",
                                    info.plan.input_rate as f32 / 1000.0,
                                    self.live.latency_ms
                                ))
                                .monospace()
                                .color(Color32::from_rgb(148, 163, 184))
                                .size(11.0),
                            );
                        }
                        None => {
                            ui.label(egui::RichText::new("Live: pick the input and output, then Start Monitoring").color(Color32::from_rgb(148, 163, 184)));
                        }
                    }
                } else if let Some(ref track) = self.track {
                    ui.label(egui::RichText::new("Track:").color(Color32::from_rgb(148, 163, 184)));
                    ui.label(egui::RichText::new(&track.filename).strong().color(Color32::WHITE));
                    ui.label(egui::RichText::new("|").color(Color32::from_rgb(71, 85, 105)));

                    let total_sec = track.total_frames as f32 / track.sample_rate as f32;
                    let mins = (total_sec / 60.0).floor() as u32;
                    let secs = total_sec % 60.0;
                    let meta_str = format!("{:02}:{:06.3} | {:.1} kHz | {}-bit", mins, secs, track.sample_rate as f32 / 1000.0, track.bits_per_sample);
                    ui.label(egui::RichText::new(meta_str).monospace().color(Color32::from_rgb(148, 163, 184)).size(11.0));
                } else {
                    ui.label(egui::RichText::new("No track loaded | Press Ctrl+O or click 'Open Audio File' to begin").color(Color32::from_rgb(148, 163, 184)));
                }

                // Right actions
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.engine_mode == EngineMode::Live {
                        // The bridge to File mode: analyse and render what was just recorded.
                        let can_open = self.live.last_take.is_some() && self.live.tape.state == TapeState::Idle;
                        let open = ui.add_enabled(
                            can_open,
                            egui::Button::new(egui::RichText::new("Open Last Take").color(Color32::WHITE).strong())
                                .fill(Color32::from_rgb(16, 185, 129)),
                        );
                        let hover = match self.live.last_take.as_ref() {
                            Some(t) => format!("Stop monitoring and open {} in File Mode to analyse and render it", t.wav_path.display()),
                            None => "Record a take first".to_string(),
                        };
                        if open.on_hover_text(hover).on_disabled_hover_text("Record a take first, and stop it").clicked() {
                            self.open_last_take();
                        }
                    } else if self.engine_mode == EngineMode::File && ui.add(
                        egui::Button::new(egui::RichText::new("Render Album").color(Color32::WHITE).strong())
                            .fill(Color32::from_rgb(16, 185, 129)) // emerald-600
                    ).clicked() {
                        self.render_album_batch();
                    }

                    ui.separator();

                    // Mode switcher: [ File Mode ] [ Live Monitor ] [ Tracks ], right to left.
                    if ui
                        .selectable_label(self.engine_mode == EngineMode::Tracks, "Tracks")
                        .on_hover_text("Look up the album, place the track splits and fill in the tags")
                        .clicked()
                    {
                        self.set_engine_mode(EngineMode::Tracks);
                    }
                    if ui.selectable_label(self.engine_mode == EngineMode::Live, "Live Monitor").clicked() {
                        self.set_engine_mode(EngineMode::Live);
                    }
                    if ui.selectable_label(self.engine_mode == EngineMode::File, "File Mode").clicked() {
                        self.set_engine_mode(EngineMode::File);
                    }
                });
            });
            ui.add_space(2.0);
        });

        // Live Monitor Banner (shown only when in Live Monitor mode)
        if self.engine_mode == EngineMode::Live {
            egui::TopBottomPanel::top("live_monitor_banner").show(ctx, |ui| {
                ui.add_space(3.0);
                let running = self.live.is_running();
                let mut devices_changed = false;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("LIVE INPUT").strong().color(Color32::from_rgb(165, 180, 252)));
                    ui.separator();

                    // Devices and rate are fixed while monitoring: stop to change them.
                    ui.add_enabled_ui(!running, |ui| {
                        let (inputs, outputs) = match self.live.scan.as_ref() {
                            Some(s) => (s.inputs.clone(), s.outputs.clone()),
                            None => (Vec::new(), Vec::new()),
                        };
                        // Readable labels; busy devices are listed but marked.
                        let shown = |d: &cr_rt::DeviceInfo| {
                            if d.available { d.label.clone() } else { format!("{} - busy", d.label) }
                        };
                        let selected = |list: &[cr_rt::DeviceInfo], chosen: &Option<String>| {
                            list.iter()
                                .find(|d| Some(&d.name) == chosen.as_ref())
                                .map(shown)
                                .or_else(|| chosen.clone())
                                .unwrap_or_else(|| "(none found)".to_string())
                        };
                        ui.label("Input:");
                        egui::ComboBox::from_id_source("combo_input")
                            .width(260.0)
                            .selected_text(selected(&inputs, &self.live.input))
                            .show_ui(ui, |ui| {
                                for d in &inputs {
                                    devices_changed |= ui
                                        .selectable_value(&mut self.live.input, Some(d.name.clone()), shown(d))
                                        .on_hover_text(&d.name)
                                        .changed();
                                }
                            });

                        ui.label("Output:");
                        egui::ComboBox::from_id_source("combo_output")
                            .width(260.0)
                            .selected_text(selected(&outputs, &self.live.output))
                            .show_ui(ui, |ui| {
                                for d in &outputs {
                                    devices_changed |= ui
                                        .selectable_value(&mut self.live.output, Some(d.name.clone()), shown(d))
                                        .on_hover_text(&d.name)
                                        .changed();
                                }
                            });

                        ui.label("Rate:");
                        let rates = self.live.rates();
                        egui::ComboBox::from_id_source("combo_rate")
                            .width(80.0)
                            .selected_text(if rates.is_empty() {
                                "-".to_string()
                            } else {
                                format!("{:.1} kHz", self.live.sample_rate as f32 / 1000.0)
                            })
                            .show_ui(ui, |ui| {
                                for r in rates {
                                    ui.selectable_value(&mut self.live.sample_rate, r, format!("{:.1} kHz", r as f32 / 1000.0));
                                }
                            })
                            .response
                            .on_hover_text(format!(
                                "Rates both devices support. For another rate, change it in {}.",
                                crate::live::DEVICE_FORMAT_HINT
                            ));

                        if ui.button("Rescan").on_hover_text("List the audio devices again").clicked() {
                            self.rescan_devices();
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running {
                            if ui.add(
                                egui::Button::new(egui::RichText::new(" Stop Monitoring ").color(Color32::WHITE).strong())
                                    .fill(Color32::from_rgb(71, 85, 105)),
                            )
                            .on_hover_text("Stops input and output; an open recording is finished first")
                            .clicked()
                            {
                                self.stop_monitoring();
                            }
                        } else {
                            let can_start = self.live.input.is_some() && self.live.output.is_some();
                            if ui.add_enabled(
                                can_start,
                                egui::Button::new(egui::RichText::new(" Start Monitoring ").color(Color32::WHITE).strong())
                                    .fill(Color32::from_rgb(37, 99, 235)),
                            )
                            .clicked()
                            {
                                self.start_monitoring();
                            }
                        }
                    });
                });
                if devices_changed {
                    self.live.fit_rate();
                }
                if let Some(problem) = self.live.rate_problem() {
                    ui.label(egui::RichText::new(problem).color(Color32::from_rgb(251, 191, 36)).size(11.0));
                }
                if let Some(w) = self.live.info.as_ref().filter(|_| running).and_then(|i| i.warnings.first()) {
                    ui.label(egui::RichText::new(w).color(Color32::from_rgb(251, 191, 36)).size(11.0));
                }
                ui.add_space(3.0);
            });
        }

        // Status bar (bottom edge): the latest status message, one line.
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            let msg = self.status_message.as_deref().unwrap_or("Ready");
            ui.label(egui::RichText::new(msg).color(Color32::from_rgb(52, 211, 153)).size(11.0));
        });

        // Bottom panel. Live mode: levels and stream health.
        // File mode: record analysis (condition, suggested settings, recording, problem spots).
        if self.engine_mode == EngineMode::Live {
            // Fixed height: the vertical separators inside would otherwise grow it every frame.
            egui::TopBottomPanel::bottom("live_levels_panel").resizable(false).exact_height(LEVELS_PANEL_H).show(ctx, |ui| {
                ui.add_space(4.0);
                render_levels_panel(ui, &mut self.live);
                ui.add_space(4.0);
            });
        } else if self.engine_mode == EngineMode::Tracks {
            let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed) as u64;
            let has_track = self.track.is_some();
            let dragging = has_track && files_hovering;
            let action = egui::TopBottomPanel::bottom("tracks_panel")
                .resizable(true)
                .default_height(300.0)
                .min_height(160.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    render_tracks_panel(ui, &mut self.tracks, has_track, playhead, self.cover_texture.as_ref(), dragging)
                })
                .inner;
            self.tracks_action(action);
            if self.tracks.cover_changed {
                self.tracks.cover_changed = false;
                self.cover_texture = self.tracks.cover.as_ref().and_then(|c| {
                    let img = image::load_from_memory(&c.bytes).ok()?.thumbnail(192, 192).into_rgba8();
                    let size = [img.width() as usize, img.height() as usize];
                    Some(ctx.load_texture("album_cover", egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), egui::TextureOptions::LINEAR))
                });
            }
            if self.tracks.export_open {
                let rip_bits = self.track.as_ref().map_or(24, |t| t.bits_per_sample);
                let busy = self.active_progress.is_some();
                match crate::views::export_window::render_export_window(ctx, &mut self.tracks, &mut self.services.prefs, rip_bits, busy) {
                    crate::views::export_window::ExportAction::Export => self.start_export(),
                    crate::views::export_window::ExportAction::PrefsChanged => {
                        if let Err(e) = self.services.prefs.save() {
                            self.status_message = Some(format!("Could not save the export settings: {}", e));
                        }
                    }
                    crate::views::export_window::ExportAction::None => {}
                }
            }
            if self.tracks.search.open {
                let has_token = self.services.has_token();
                match crate::views::search_window::render_search_window(ctx, &mut self.tracks.search, has_token) {
                    crate::views::search_window::SearchAction::Search => self.tracks.search.run(&self.services.prefs.musicbrainz_contact),
                    crate::views::search_window::SearchAction::Use(c) => {
                        if let Some(t) = self.track.as_ref() {
                            self.tracks.use_candidate(&c, &t.frames_internal, t.sample_rate, &self.services.prefs.musicbrainz_contact);
                        }
                    }
                    crate::views::search_window::SearchAction::None => {}
                }
            }
        } else {
        egui::TopBottomPanel::bottom("record_analysis_panel").show(ctx, |ui| {
            ui.add_space(4.0);
            let action = render_analysis_panel(
                ui,
                self.analysis.as_deref(),
                self.tune_report.as_ref(),
                self.track.is_some(),
                self.active_progress.is_some(),
                &mut self.dsp_tuning,
                self.last_render_levels.as_ref(),
            );
            ui.add_space(4.0);
            match action {
                PanelAction::Analyze => self.start_album_analysis(),
                PanelAction::ApplySuggested => self.apply_suggested_settings(),
                PanelAction::JumpTo(start, len) => self.jump_to_spot(start, len),
                PanelAction::TuningChanged => self.tuning_changed(),
                PanelAction::None => {}
            }
        });
        }

        // 3. Central Panel: Timeline Mini-Map, Stacked Waveforms, Transport & Bottom Dock
        egui::CentralPanel::default().show(ctx, |ui| {
            let missed = match self.engine_mode {
                EngineMode::Live => {
                    self.live_center(ui);
                    Vec::new()
                }
                EngineMode::Tracks => {
                    self.tracks_center(ui);
                    Vec::new()
                }
                EngineMode::File => self.file_center(ui),
            };

            ui.add_space(6.0);

            // 4. Transport & Zoom Bar (Directly below waveform view)
            egui::Frame::none()
                .fill(Color32::from_rgb(30, 41, 59))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(51, 65, 85)))
                .inner_margin(6.0)
                .rounding(4.0)
                .show(ui, |ui| {
                    if self.engine_mode == EngineMode::Live {
                        self.live_transport(ui);
                        return;
                    }
                    ui.horizontal(|ui| {
                        let is_playing = self.player.transport.is_playing.load(Ordering::Relaxed);
                        let playhead = self.player.transport.playhead_frame.load(Ordering::Relaxed);

                        // 1. Rewind to Start (|<)
                        if ui.add(
                            egui::Button::new(egui::RichText::new(" |< ").color(Color32::WHITE).strong().size(13.0))
                                .fill(Color32::from_rgb(51, 65, 85))
                        )
                        .on_hover_text("Rewind to Start (Home)")
                        .clicked()
                        {
                            self.player.transport.seek_to(0);
                            self.selection_start = 0;
                            self.selection_end = 0;
                            self.player.transport.loop_start_frame.store(0, Ordering::Relaxed);
                            self.player.transport.loop_end_frame.store(0, Ordering::Relaxed);
                        }

                        // 2. Play (▶ Play)
                        let play_fill = if is_playing {
                            Color32::from_rgb(16, 185, 129) // emerald green
                        } else {
                            Color32::from_rgb(37, 99, 235) // royal blue
                        };
                        if ui.add(
                            egui::Button::new(egui::RichText::new(" ▶ Play ").color(Color32::WHITE).strong().size(13.0))
                                .fill(play_fill)
                        )
                        .on_hover_text("Play (Space)")
                        .clicked()
                        {
                            self.player.transport.play();
                        }

                        // 3. Pause (⏸ Pause)
                        let is_paused = !is_playing && playhead > 0;
                        let pause_fill = if is_paused {
                            Color32::from_rgb(217, 119, 6) // amber-600
                        } else {
                            Color32::from_rgb(51, 65, 85)
                        };
                        if ui.add(
                            egui::Button::new(egui::RichText::new(" ⏸ Pause ").color(Color32::WHITE).strong().size(13.0))
                                .fill(pause_fill)
                        )
                        .on_hover_text("Pause (Space)")
                        .clicked()
                        {
                            self.player.transport.pause();
                        }

                        // 4. Stop (⏹ Stop)
                        if ui.add(
                            egui::Button::new(egui::RichText::new(" ⏹ Stop ").color(Color32::WHITE).strong().size(13.0))
                                .fill(Color32::from_rgb(71, 85, 105))
                        )
                        .on_hover_text("Stop (Escape)")
                        .clicked()
                        {
                            self.player.transport.stop();
                            let start = self.selection_start.min(self.selection_end);
                            self.player.transport.seek_to(start);
                        }

                        // 5. Forward to End (>|)
                        if ui.add(
                            egui::Button::new(egui::RichText::new(" >| ").color(Color32::WHITE).strong().size(13.0))
                                .fill(Color32::from_rgb(51, 65, 85))
                        )
                        .on_hover_text("Forward to End (End)")
                        .clicked()
                        {
                            if let Some(ref t) = self.track {
                                self.player.transport.seek_to(t.total_frames);
                                self.player.transport.pause();
                            }
                        }

                        if self.engine_mode == EngineMode::Tracks {
                            ui.separator();
                            let has_tracks = self.tracks.sheet.as_ref().is_some_and(|s| !s.tracks.is_empty());
                            if ui
                                .add_enabled(has_tracks, egui::Button::new(egui::RichText::new(" |<< Track ").color(Color32::WHITE).size(13.0)).fill(Color32::from_rgb(51, 65, 85)))
                                .on_hover_text("Start of this track, or of the previous one when already there (Page Up)")
                                .clicked()
                            {
                                self.jump_track(false);
                            }
                            if ui
                                .add_enabled(has_tracks, egui::Button::new(egui::RichText::new(" Track >>| ").color(Color32::WHITE).size(13.0)).fill(Color32::from_rgb(51, 65, 85)))
                                .on_hover_text("Start of the next track (Page Down)")
                                .clicked()
                            {
                                self.jump_track(true);
                            }
                        }

                        ui.separator();

                        // Loop Selection Checkbox
                        let mut looping = self.player.transport.is_looping.load(Ordering::Relaxed);
                        if ui.checkbox(&mut looping, egui::RichText::new("Loop Selection").color(Color32::from_rgb(203, 213, 225))).changed() {
                            self.player.transport.is_looping.store(looping, Ordering::Relaxed);
                        }

                        ui.separator();

                        // Zoom Controls
                        if let Some(ref t) = self.track {
                            if ui.add(egui::Button::new(egui::RichText::new("Zoom +").color(Color32::WHITE)).fill(Color32::from_rgb(51, 65, 85))).clicked() {
                                let cur_len = self.view_end_frame.saturating_sub(self.view_start_frame);
                                let new_len = (cur_len as f32 / 1.4).max(t.sample_rate as f32 * 0.1) as usize;
                                let mid = (self.view_start_frame + self.view_end_frame) / 2;
                                self.view_start_frame = mid.saturating_sub(new_len / 2);
                                self.view_end_frame = (self.view_start_frame + new_len).min(t.total_frames);
                            }
                            if ui.add(egui::Button::new(egui::RichText::new("Zoom -").color(Color32::WHITE)).fill(Color32::from_rgb(51, 65, 85))).clicked() {
                                let cur_len = self.view_end_frame.saturating_sub(self.view_start_frame);
                                let new_len = ((cur_len as f32 * 1.4) as usize).min(t.total_frames);
                                let mid = (self.view_start_frame + self.view_end_frame) / 2;
                                self.view_start_frame = mid.saturating_sub(new_len / 2);
                                self.view_end_frame = (self.view_start_frame + new_len).min(t.total_frames);
                            }
                            if ui.add(egui::Button::new(egui::RichText::new("Fit Track").color(Color32::WHITE)).fill(Color32::from_rgb(51, 65, 85))).clicked() {
                                self.view_start_frame = 0;
                                self.view_end_frame = t.total_frames;
                            }
                        }

                        // Selection Readout Information
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(ref t) = self.track {
                                let min_sel = self.selection_start.min(self.selection_end);
                                let max_sel = self.selection_start.max(self.selection_end);

                                if min_sel < max_sel {
                                    let dur_sec = (max_sel.saturating_sub(min_sel) as f32) / (t.sample_rate as f32);
                                    let t1 = (min_sel as f32) / (t.sample_rate as f32);
                                    let t2 = (max_sel as f32) / (t.sample_rate as f32);

                                    let sel_label = format!("{:02}:{:06.3} - {:02}:{:06.3} | Length: {:.3}s ({} samples)",
                                        (t1 / 60.0).floor() as u32, t1 % 60.0,
                                        (t2 / 60.0).floor() as u32, t2 % 60.0,
                                        dur_sec, max_sel.saturating_sub(min_sel)
                                    );
                                    ui.label(egui::RichText::new(sel_label).monospace().color(Color32::from_rgb(129, 140, 248)).size(11.0));
                                    ui.label(egui::RichText::new("Loop Selection:").color(Color32::from_rgb(148, 163, 184)));
                                } else {
                                    let cur_p = self.player.transport.playhead_frame.load(Ordering::Relaxed);
                                    let t_cur = (cur_p as f32) / (t.sample_rate as f32);
                                    let cur_label = format!("{:02}:{:06.3} ({} samples)",
                                        (t_cur / 60.0).floor() as u32, t_cur % 60.0, cur_p
                                    );
                                    ui.label(egui::RichText::new(cur_label).monospace().color(Color32::from_rgb(129, 140, 248)).size(11.0));
                                    ui.label(egui::RichText::new("Cursor Position:").color(Color32::from_rgb(148, 163, 184)));
                                }
                            } else {
                                ui.label(egui::RichText::new("No Selection").monospace().color(Color32::from_rgb(100, 116, 139)).size(11.0));
                            }
                        });
                    });
                });

            ui.add_space(8.0);
            if self.engine_mode == EngineMode::Tracks {
                return;
            }

            // 5. Bottom Dock: Audition Triad & Fluid Sliders
            ui.columns(2, |columns| {
                // Left Column: Audition Triad
                columns[0].group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("AUDITION PREVIEW TRIAD").strong().color(Color32::from_rgb(148, 163, 184)).size(10.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new("Shortcuts: 1, 2, 3").color(Color32::from_rgb(129, 140, 248)).size(10.0).monospace());
                        });
                    });
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if ui.selectable_label(self.dsp_tuning.audition_mode == AuditionMode::OutClean, "Out (Clean)").clicked() {
                            self.set_audition(AuditionMode::OutClean);
                        }
                        if ui.selectable_label(self.dsp_tuning.audition_mode == AuditionMode::InBypass, "In (Bypass)").clicked() {
                            self.set_audition(AuditionMode::InBypass);
                        }
                        if ui.selectable_label(self.dsp_tuning.audition_mode == AuditionMode::NoiseDelta, "Noise (Delta)").clicked() {
                            self.set_audition(AuditionMode::NoiseDelta);
                        }
                    });

                });

                // Right Column: Fluid Sliders & DSP Wavelet Scan
                columns[1].group(|ui| {
                    // The live engine has no DeCrackle, Reverse or Wavelet x2.
                    let live = self.engine_mode == EngineMode::Live;
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("DETECTION SENSITIVITY TUNING").strong().color(Color32::from_rgb(148, 163, 184)).size(10.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if live {
                                ui.label(egui::RichText::new("Changes apply to the live engine at once").color(Color32::from_rgb(52, 211, 153)).monospace().size(10.0));
                            } else if self.track.is_some() {
                                let text = if self.viewport_cache.repairs_previewed {
                                    format!("{} repairs in view, {} exposed clicks missed", self.viewport_cache.viewport_clicks.len(), missed.len())
                                } else {
                                    "Zoom in to 30 s or less to preview repairs".to_string()
                                };
                                ui.label(egui::RichText::new(text).color(Color32::from_rgb(52, 211, 153)).monospace().size(10.0));
                            }
                        });
                    });
                    ui.add_space(2.0);

                    let mut tuning_changed = false;

                    ui.horizontal(|ui| {
                        ui.label("DeClick:");
                        let declick_resp = ui.add(egui::Slider::new(&mut self.dsp_tuning.declick, 0..=100));
                        if declick_resp.changed() {
                            tuning_changed = true;
                        }

                        ui.add_space(6.0);
                        ui.label("DeCrackle:");
                        let decrackle_resp = ui
                            .add_enabled(!live, egui::Slider::new(&mut self.dsp_tuning.decrackle, 0..=100))
                            .on_disabled_hover_text("File Mode only: the live engine has no decrackler");
                        if decrackle_resp.changed() {
                            tuning_changed = true;
                        }

                    });

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut self.dsp_tuning.pitch_protection, "Pitch Protection").on_hover_text("Safeguard periodic harmonic transients (brass, reeds, and vocal formants)").changed() {
                            tuning_changed = true;
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!live, egui::Checkbox::new(&mut self.dsp_tuning.reverse, "Reverse"))
                            .on_hover_text("Scan backwards in time: drum hits and other sharp attacks are compared with their own decay and left alone, while real clicks are still found. Suggested for most records")
                            .on_disabled_hover_text("File Mode only: the live engine scans one way")
                            .changed()
                        {
                            tuning_changed = true;
                        }
                        ui.separator();
                        ui.label("Method:");
                        let prev_method = self.dsp_tuning.method;
                        egui::ComboBox::from_id_source("combo_method")
                            .selected_text(match self.dsp_tuning.method {
                                DeclickMethod::Wavelet => "Wavelet",
                                DeclickMethod::Simple => "Simple",
                                DeclickMethod::WaveletX2 if live => "Wavelet x2 (live: Wavelet)",
                                DeclickMethod::WaveletX2 => "Wavelet x2",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.dsp_tuning.method, DeclickMethod::Wavelet, "Wavelet");
                                ui.selectable_value(&mut self.dsp_tuning.method, DeclickMethod::Simple, "Simple");
                                if !live {
                                    ui.selectable_value(&mut self.dsp_tuning.method, DeclickMethod::WaveletX2, "Wavelet x2");
                                }
                            })
                            .response
                            .on_hover_text(if live {
                                "Changing the method restarts the live declicker: the monitor goes quiet for a moment, a recording carries on"
                            } else {
                                "Repair method"
                            });

                        if self.dsp_tuning.method != prev_method {
                            tuning_changed = true;
                        }
                    });

                    if tuning_changed {
                        self.tuning_changed();
                        ctx.request_repaint();
                    }
                });
            });
        });

        // 7. Active Task Progress Modal Popup
        if let Some(ref progress) = self.active_progress {
            egui::Window::new(&progress.title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .min_width(440.0)
                .show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&progress.description)
                            .color(Color32::from_rgb(203, 213, 225))
                            .size(13.0),
                    );
                    ui.add_space(10.0);

                    // Progress bar with percentage
                    let p_bar = egui::ProgressBar::new(progress.progress)
                        .show_percentage()
                        .animate(true)
                        .fill(Color32::from_rgb(59, 130, 246));
                    ui.add(p_bar);

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(
                                egui::Button::new(egui::RichText::new("Cancel").color(Color32::WHITE).strong())
                                    .fill(Color32::from_rgb(225, 29, 72)) // crimson
                            ).clicked() {
                                progress.cancel_flag.store(true, Ordering::SeqCst);
                            }
                        });
                    });
                    ui.add_space(4.0);
                });
        }
    }
}
