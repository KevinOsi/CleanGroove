// Uses the Windows event loop extension to run eframe off the main thread.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use cr_gui::app::CleanGrooveApp;
use winit::platform::windows::EventLoopBuilderExtWindows;

/// Renders the main window with a completed album analysis in the bottom panel and saves a
/// screenshot to the temp folder (cleangroove_analysis.png).
#[test]
fn test_screenshot_analysis() {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 860.0])
            .with_visible(false),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };

    let target = std::env::temp_dir().join("cleangroove_analysis.png");
    let input_audio = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scratch/demo/vinyl_demo_track.wav");

    let res = eframe::run_native(
        "CleanGroove Analysis Test",
        native_options,
        Box::new(move |cc| {
            let mut app = CleanGrooveApp::new(cc);
            app.screenshot_target = Some(target);
            app.load_file_from_path(input_audio);
            let track = app.track.as_ref().expect("demo track loads");
            let analysis = cr_dsp::analysis::analyze_album(
                &track.frames_internal,
                track.sample_rate,
                track.channels,
                |_, _| {},
                &AtomicBool::new(false),
            )
            .expect("not cancelled");
            let report = cr_dsp::autotune::autotune_audio_with_analysis(
                &track.frames_internal,
                track.sample_rate,
                track.channels,
                Some(&analysis),
                |_, _| {},
            );
            app.timeline_heatmap = CleanGrooveApp::compute_timeline_heatmap(&analysis, 300);
            app.analysis = Some(Arc::new(analysis));
            app.tune_report = Some(report);
            Ok(Box::new(app))
        }),
    );

    assert!(res.is_ok(), "eframe run_native failed: {:?}", res.err());
}
