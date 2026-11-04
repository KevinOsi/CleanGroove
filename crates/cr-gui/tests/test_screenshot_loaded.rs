// Uses the Windows event loop extension to run eframe off the main thread.
#![cfg(windows)]

use std::path::PathBuf;
use cr_gui::app::CleanGrooveApp;
use winit::platform::windows::EventLoopBuilderExtWindows;

#[test]
fn test_screenshot_loaded() {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_visible(false),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };

    let target_loaded = std::env::temp_dir().join("cleangroove_loaded.png");
    let input_audio = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scratch/demo/vinyl_demo_track.wav");

    let res = eframe::run_native(
        "CleanGroove Workstation Test",
        native_options,
        Box::new(move |cc| {
            let mut app = CleanGrooveApp::new(cc);
            app.screenshot_target = Some(target_loaded);
            app.load_file_from_path(input_audio);
            Ok(Box::new(app))
        }),
    );

    assert!(res.is_ok(), "eframe run_native failed: {:?}", res.err());
}