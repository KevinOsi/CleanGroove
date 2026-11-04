use cr_dsp::analysis::{AlbumAnalysis, ANALYSIS_DCL};
use cr_dsp::autotune::{AutoTuneReport, RecordCondition};
use cr_dsp::params::ChannelMode;
use cr_dsp::post::LevelReport;
use egui::{Color32, RichText, Ui};

use crate::dsp_bridge::{cr_params, post_settings};
use crate::state::DspTuning;

/// What the user asked the record analysis panel to do this frame.
pub enum PanelAction {
    None,
    Analyze,
    ApplySuggested,
    /// Jump to and loop a problem spot: (start frame, length in frames).
    JumpTo(usize, usize),
    /// A correction was switched or changed: push the settings to the audio player.
    TuningChanged,
}

const LABEL: Color32 = Color32::from_rgb(148, 163, 184);
const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
const DIM: Color32 = Color32::from_rgb(100, 116, 139);
const GOOD: Color32 = Color32::from_rgb(52, 211, 153);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);
const BAD: Color32 = Color32::from_rgb(248, 113, 113);

/// Capture levels peaking below this leave bits unused (ClickRepair manual).
const LOW_CAPTURE_DBFS: f32 = -12.0;

fn grade_color(c: RecordCondition) -> Color32 {
    match c {
        RecordCondition::NearMint => GOOD,
        RecordCondition::VeryGoodPlus => Color32::from_rgb(34, 211, 238),
        RecordCondition::VeryGood => WARN,
        RecordCondition::GoodPlus => Color32::from_rgb(251, 146, 60),
    }
}

fn clock(frame: usize, sample_rate: u32) -> String {
    let t = frame as f64 / sample_rate.max(1) as f64;
    format!("{:02}:{:02}", (t / 60.0).floor() as u32, (t % 60.0).floor() as u32)
}

fn one_in(fraction: f64) -> String {
    if fraction > 0.0 { format!("1 in {:.0}", 1.0 / fraction) } else { "none".to_string() }
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).strong().color(LABEL).size(10.0));
    ui.add_space(2.0);
}

/// True when the current settings already equal the suggestion (declick and corrections).
fn matches_suggestion(tuning: &DspTuning, report: &AutoTuneReport, analysis: &AlbumAnalysis) -> bool {
    let current = cr_params(tuning);
    let s = &report.recommended;
    current.dcl == s.dcl
        && current.dcr == s.dcr
        && current.method == s.method
        && current.detect_pitch == s.detect_pitch
        && current.reverse == s.reverse
        && post_settings(tuning) == analysis.suggested_post()
}

/// Needle thumps the analysis found: where (click to loop a series) and the repair switch.
/// Returns true when the switch changed.
fn needle_thumps(ui: &mut Ui, a: &AlbumAnalysis, tuning: &mut DspTuning, action: &mut PanelAction) -> bool {
    ui.add_space(2.0);
    let count: usize = a.thumps.iter().map(|s| s.events.len()).sum();
    ui.label(RichText::new(format!("Needle thumps: {} in {} series", count, a.thumps.len())).color(WARN).size(11.0))
        .on_hover_text("Loud thumps once per turn of the record: debris throwing the needle. The declicker removes only their onset. Listen to a series; if you hear thumps, switch on the repair.");
    for s in &a.thumps {
        let (first, last) = (s.events[0].frame, s.events[s.events.len() - 1].frame);
        let text = format!("{} - {}  {} thumps", clock(first, a.sample_rate), clock(last, a.sample_rate), s.events.len());
        let pad = a.sample_rate as usize;
        if ui.selectable_label(false, RichText::new(text).monospace().size(11.0).color(TEXT)).on_hover_text("Click to loop this series").clicked() {
            let start = first.saturating_sub(pad);
            *action = PanelAction::JumpTo(start, (last + pad).min(a.total_frames) - start);
        }
    }
    ui.checkbox(&mut tuning.dethump, RichText::new("Repair needle thumps").color(TEXT))
        .on_hover_text("Replaces each thump of these series with the declicker's AR interpolation, after declicking and before the corrections, in render, preview and audition")
        .changed()
}

/// The post-declick corrections: switches, measured values and the last render's levels.
fn corrections(ui: &mut Ui, analysis: Option<&AlbumAnalysis>, tuning: &mut DspTuning, last_render: Option<&(f64, LevelReport)>) -> bool {
    let mut changed = false;
    heading(ui, "CORRECTIONS (after declick)");
    changed |= ui
        .checkbox(&mut tuning.post_enabled, RichText::new("Apply corrections").color(TEXT))
        .on_hover_text("Run the post-declick chain in render, preview and audition")
        .changed();
    ui.add_enabled_ui(tuning.post_enabled, |ui| {
        let measured = analysis.map(|a| &a.channels);
        changed |= ui
            .checkbox(&mut tuning.flip_polarity, "Flip right polarity")
            .on_hover_text(match analysis {
                Some(a) => format!("L/R correlation above 150 Hz: {:.2} (strongly negative means one channel is inverted)", a.lr_midband_correlation),
                None => "Analyze the album to check polarity".to_string(),
            })
            .changed();
        let balance = ui.add_enabled(measured.is_some(), egui::Checkbox::new(&mut tuning.balance_on, format!("Balance {:+.2} dB", tuning.balance_db)));
        changed |= balance
            .on_hover_text(match measured {
                Some(c) => format!(
                    "Left-minus-right level above 150 Hz: median {:+.2} dB, spread {:.2} dB. Suggested only for mono records: on stereo a level difference is usually the mix.",
                    c.balance_db, c.balance_spread_db
                ),
                None => "Analyze the album to measure the balance".to_string(),
            })
            .changed();
        let azimuth = ui.add_enabled(measured.is_some(), egui::Checkbox::new(&mut tuning.azimuth_on, format!("Azimuth {:+.1} us", tuning.azimuth_us)));
        changed |= azimuth
            .on_hover_text(match measured {
                Some(c) => format!(
                    "Right channel behind left: {:+.2} us +- {:.2} over {} windows (halves {:+.1} / {:+.1} us). Corrected by delaying each channel half of it.",
                    c.azimuth_us, c.azimuth_se_us, c.azimuth_windows, c.azimuth_halves_us.0, c.azimuth_halves_us.1
                ),
                None => "Analyze the album to measure the azimuth".to_string(),
            })
            .changed();
        changed |= ui
            .checkbox(&mut tuning.subsonic_on, format!("Subsonic high-pass {:.0} Hz", cr_dsp::analysis::SUBSONIC_HZ))
            .on_hover_text(match measured {
                Some(c) => format!("Removes DC, warp and tonearm resonance. Below 18 Hz in the music: {:.1} dBFS.", c.subsonic_music_db),
                None => "Removes DC, warp and tonearm resonance".to_string(),
            })
            .changed();
        let elliptical = ui.add_enabled(!tuning.stereo_to_mono, egui::Checkbox::new(&mut tuning.elliptical_on, format!("Elliptical EQ {:.0} Hz", tuning.elliptical_hz)));
        changed |= elliptical
            .on_hover_text(match measured {
                Some(c) => format!(
                    "Sums the bass below the corner to mono, removing vertical rumble. Side below 150 Hz: {:.1} dBFS between tracks, {:.1} dBFS in the music.",
                    c.side_rumble_gap_db, c.side_lf_music_db
                ),
                None => "Sums the bass below the corner to mono, removing vertical rumble".to_string(),
            })
            .changed();
        changed |= ui
            .checkbox(&mut tuning.stereo_to_mono, "Stereo>Mono")
            .on_hover_text("Mono record captured in stereo: declick both channels, align them, then sum to mono")
            .changed();
    });
    if let Some((gain, r)) = last_render {
        ui.add_space(2.0);
        ui.label(
            RichText::new(format!(
                "Last render: peak {:.1} dBFS, true peak {:.1} dBTP, {:.1} LUFS (RG {:+.1} dB){}",
                r.sample_peak_dbfs,
                r.true_peak_dbtp,
                r.loudness_lufs,
                r.replaygain_db(),
                if *gain < 0.0 { format!(", gain {:.1} dB", gain) } else { String::new() }
            ))
            .color(DIM)
            .size(11.0),
        );
    }
    changed
}

/// Renders the record analysis panel: click condition, suggested settings, recording levels and
/// channels, problem spots, and the post-declick corrections.
pub fn render_analysis_panel(
    ui: &mut Ui,
    analysis: Option<&AlbumAnalysis>,
    report: Option<&AutoTuneReport>,
    track_loaded: bool,
    running: bool,
    tuning: &mut DspTuning,
    last_render: Option<&(f64, LevelReport)>,
) -> PanelAction {
    let mut action = PanelAction::None;

    ui.horizontal(|ui| {
        ui.label(RichText::new("RECORD ANALYSIS").strong().color(Color32::from_rgb(203, 213, 225)).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if analysis.is_some() { "Re-analyze" } else { "Analyze Album" };
            let button = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(Color32::from_rgb(79, 70, 229));
            if ui.add_enabled(track_loaded && !running, button).clicked() {
                action = PanelAction::Analyze;
            }
        });
    });
    ui.separator();

    let (Some(a), Some(r)) = (analysis, report) else {
        ui.columns(2, |cols| {
            let text = if !track_loaded {
                "Load a record to analyze it."
            } else if running {
                "Analyzing..."
            } else {
                "Analyze the album to grade the record's condition, map where its clicks are, measure its channels and suggest settings. Takes about as long as a render."
            };
            cols[0].label(RichText::new(text).color(LABEL));
            if corrections(&mut cols[1], None, tuning, last_render) {
                action = PanelAction::TuningChanged;
            }
        });
        return action;
    };

    ui.columns(5, |cols| {
        // 1. Click condition
        let ui = &mut cols[0];
        heading(ui, "CLICK CONDITION");
        let grade = if a.condition_estimated { format!("{} (estimated)", a.condition) } else { a.condition.to_string() };
        ui.label(RichText::new(grade).strong().size(15.0).color(grade_color(a.condition)));
        ui.label(
            RichText::new(format!("{:.1} audible clicks/s, {:.1} loud pops/s", a.audible_clicks_per_sec, a.loud_pops_per_sec))
                .color(TEXT),
        );
        let basis = if a.condition_estimated {
            format!("No silence between tracks: counted in the quietest {:.0} s of music, so it may under-read", a.grade_seconds)
        } else {
            format!("Measured in {:.0} s of silence between tracks", a.grade_seconds)
        };
        ui.label(RichText::new(basis).color(DIM).size(11.0));
        if a.long_repairs > 0 {
            ui.label(RichText::new(format!("{} long repairs (possible scratches)", a.long_repairs)).color(WARN).size(11.0));
        }
        if !a.thumps.is_empty() {
            if needle_thumps(ui, a, tuning, &mut action) {
                action = PanelAction::TuningChanged;
            }
        }

        // 2. Suggested settings
        let ui = &mut cols[1];
        heading(ui, "SUGGESTED START");
        let s = &r.recommended;
        let p = a.suggested_post();
        ui.label(RichText::new(format!("DeClick {}  |  {:?}", s.dcl, s.method)).strong().color(TEXT));
        ui.label(
            RichText::new(format!(
                "Reverse {}  |  Pitch {}  |  DeCrackle {}",
                if s.reverse { "on" } else { "off" },
                if s.detect_pitch { "on" } else { "off" },
                if s.dcr > 0 { s.dcr.to_string() } else { "off".to_string() }
            ))
            .color(TEXT),
        );
        let mut fixes = Vec::new();
        if p.flip_right_polarity {
            fixes.push("flip polarity".to_string());
        }
        if p.balance_db != 0.0 {
            fixes.push(format!("balance {:+.1} dB", p.balance_db));
        }
        if p.azimuth_us != 0.0 {
            fixes.push(format!("azimuth {:+.1} us", p.azimuth_us));
        }
        if p.mono_sum {
            fixes.push("Stereo>Mono".to_string());
        } else if let Some(f) = p.elliptical_hz {
            fixes.push(format!("elliptical {f:.0} Hz"));
        }
        fixes.push("subsonic 18 Hz".to_string());
        ui.label(RichText::new(format!("Corrections: {}", fixes.join(", "))).color(TEXT).size(11.0));
        let pickup = match r.music_pickup_dcl {
            Some(d) if d >= s.dcl => format!("Repairs stay on the surface up to about DeClick {}", d),
            Some(d) => format!("Above about DeClick {} some repairs catch music: listen to the Noise", d),
            None => "Repairs catch some music at every setting: listen to the Noise".to_string(),
        };
        let pickup_color = if r.music_pickup_dcl.map_or(false, |d| d >= s.dcl) { DIM } else { WARN };
        ui.label(RichText::new(pickup).color(pickup_color).size(11.0));
        ui.horizontal(|ui| {
            if matches_suggestion(tuning, r, a) {
                ui.label(RichText::new("Current settings match").color(GOOD).size(11.0));
            } else if ui.add(egui::Button::new(RichText::new("Apply").color(Color32::WHITE)).fill(Color32::from_rgb(16, 185, 129))).clicked() {
                action = PanelAction::ApplySuggested;
            }
            ui.collapsing(RichText::new("Why").color(LABEL).size(11.0), |ui| {
                for line in &r.rationale {
                    ui.label(RichText::new(line).color(TEXT).size(11.0));
                }
            });
        });

        // 3. Recording
        let ui = &mut cols[2];
        heading(ui, "RECORDING");
        let (peak_text, peak_color) = if a.clipped_samples > 0 {
            (format!("Peak {:.1} dBFS, {} clipped samples", a.peak_dbfs, a.clipped_samples), BAD)
        } else if a.peak_dbfs < LOW_CAPTURE_DBFS {
            (format!("Peak {:.1} dBFS: low capture level, bits unused", a.peak_dbfs), WARN)
        } else {
            (format!("Peak {:.1} dBFS, no clipping", a.peak_dbfs), TEXT)
        };
        ui.label(RichText::new(peak_text).color(peak_color));
        ui.label(RichText::new(format!("Music level {:.1} dBFS RMS", a.music_rms_db)).color(TEXT));
        let channels = match a.mode {
            ChannelMode::Stereo => "Stereo record",
            ChannelMode::Merge => "Mono record captured in stereo",
            ChannelMode::Mono => "Mono (identical channels)",
        };
        ui.label(RichText::new(channels).color(TEXT));
        ui.label(
            RichText::new(format!(
                "L/R correlation {:.3}, side/mid {:.3}",
                a.lr_midband_correlation, a.side_to_mid_ratio
            ))
            .color(DIM)
            .size(11.0),
        );
        ui.label(
            RichText::new(format!("Repairs at DeClick {}: {} samples", ANALYSIS_DCL, one_in(a.repair_fraction)))
                .color(DIM)
                .size(11.0),
        );

        // 4. Problem spots
        let ui = &mut cols[3];
        heading(ui, "PROBLEM SPOTS  (N = next)");
        if a.problem_spots.is_empty() {
            ui.label(RichText::new("No exposed clicks in the music.").color(GOOD));
        }
        egui::ScrollArea::vertical().id_source("problem_spots").max_height(110.0).show(ui, |ui| {
            for spot in &a.problem_spots {
                let mut text = format!("{}  {:>3} clicks", clock(spot.start_frame, a.sample_rate), spot.clicks);
                if spot.has_long_repair() {
                    text.push_str("  long");
                }
                if spot.near_silence {
                    text.push_str("  edge");
                }
                let resp = ui.selectable_label(false, RichText::new(text).monospace().size(11.0).color(TEXT));
                let detail = format!(
                    "{} clicks ({} loud pops) standing clear of the music (15 dB above its level of {:.0} dBFS, 20 dB above the audio either side){}. Click to loop it.",
                    spot.clicks,
                    spot.pops,
                    spot.level_db,
                    if spot.has_long_repair() { format!("; longest repair {:.1} ms", spot.longest_repair_ms) } else { String::new() }
                );
                let resp = if spot.near_silence {
                    resp.on_hover_text(format!("{detail} Next to silence or the start/end of the file: may be a needle drop or run-out groove."))
                } else {
                    resp.on_hover_text(detail)
                };
                if resp.clicked() {
                    action = PanelAction::JumpTo(spot.start_frame, spot.len_frames);
                }
            }
        });

        // 5. Corrections
        if corrections(&mut cols[4], Some(a), tuning, last_render) {
            action = PanelAction::TuningChanged;
        }
    });

    action
}
