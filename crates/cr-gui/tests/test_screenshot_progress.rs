// Uses the Windows event loop extension to run eframe off the main thread.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use cr_gui::app::CleanGrooveApp;
use cr_gui::state::ProgressState;
use winit::platform::windows::EventLoopBuilderExtWindows;

#[test]
fn test_screenshot_progress() {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_visible(false),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };

    let target_progress = std::env::temp_dir().join("cleangroove_progress.png");
    let input_audio = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scratch/demo/vinyl_demo_track.wav");

    let res = eframe::run_native(
        "CleanGroove Progress Test",
        native_options,
        Box::new(move |cc| {
            let mut app = CleanGrooveApp::new(cc);
            app.screenshot_target = Some(target_progress);
            app.load_file_from_path(input_audio);
            app.active_progress = Some(ProgressState {
                title: "Scanning Audio for Clicks...".to_string(),
                description: "Scanning audio: 425,000 / 960,000 frames (44%)".to_string(),
                progress: 0.44,
                cancel_flag: Arc::new(AtomicBool::new(false)),
            });
            Ok(Box::new(app))
        }),
    );

    assert!(res.is_ok(), "eframe run_native failed: {:?}", res.err());
}