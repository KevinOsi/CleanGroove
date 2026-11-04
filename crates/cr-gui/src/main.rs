use std::path::PathBuf;
use cr_gui::app::CleanGrooveApp;

/// Window, taskbar and Alt-Tab icon.
fn app_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon_256.png")).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width, height })
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();

    let mut initial_file: Option<PathBuf> = None;
    let mut screenshot_target: Option<PathBuf> = None;
    let mut live_mode = false;
    let mut tracks_mode = false;
    let mut live_demo: Option<PathBuf> = None;
    let mut window: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--screenshot" && i + 1 < args.len() {
            screenshot_target = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if args[i] == "--live" {
            live_mode = true;
            i += 1;
        } else if args[i] == "--tracks" {
            tracks_mode = true;
            i += 1;
        } else if args[i] == "--live-demo" && i + 1 < args.len() {
            // Development aid: show FILE in the live views as if it had been recorded.
            live_demo = Some(PathBuf::from(&args[i + 1]));
            live_mode = true;
            i += 2;
        } else if args[i] == "--window" && i + 1 < args.len() {
            // Development aid: open a menu window at start (settings, guide, about).
            window = Some(args[i + 1].clone());
            i += 2;
        } else if !args[i].starts_with("--") && initial_file.is_none() {
            initial_file = Some(PathBuf::from(&args[i]));
            i += 1;
        } else {
            i += 1;
        }
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 860.0])
        .with_min_inner_size([960.0, 600.0])
        .with_title("CleanGroove - Vinyl Restoration Workstation");
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    // Windows: CleanGroove registers its own drop target (cr_gui::drop), which also takes images
    // dragged from a browser; the window's built-in one would refuse them.
    if cfg!(windows) {
        viewport = viewport.with_drag_and_drop(false);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "CleanGroove Workstation",
        native_options,
        Box::new(move |cc| {
            #[cfg(windows)]
            let inbox = {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                match cc.window_handle().map(|h| h.as_raw()) {
                    Ok(RawWindowHandle::Win32(w)) => cr_gui::drop::register(w.hwnd.get(), cc.egui_ctx.clone())
                        .map_err(|e| eprintln!("Drag and drop is not available: {}", e))
                        .ok(),
                    _ => None,
                }
            };
            let mut app = CleanGrooveApp::new(cc);
            #[cfg(windows)]
            {
                app.drop_inbox = inbox;
            }
            if let Some(target) = screenshot_target {
                app.screenshot_target = Some(target);
            }
            if tracks_mode {
                app.set_engine_mode(cr_gui::state::EngineMode::Tracks);
            }
            if let Some(path) = initial_file {
                app.load_file_from_path(path);
            }
            if let Some(path) = live_demo {
                app.load_file_from_path(path);
                if let Some(t) = app.track.clone() {
                    if let Err(e) = app.live.load_demo(&t.frames_internal, t.sample_rate, &app.dsp_tuning) {
                        eprintln!("Live demo failed: {}", e);
                    }
                }
            }
            if live_mode {
                app.set_engine_mode(cr_gui::state::EngineMode::Live);
            }
            match window.as_deref() {
                Some("settings") => app.show_settings = true,
                Some("guide") => app.show_guide = true,
                Some("about") => app.show_about = true,
                Some("search") => app.tracks.open_search(),
                Some("export") => app.tracks.export_open = true,
                _ => {}
            }
            Ok(Box::new(app))
        }),
    )
}
