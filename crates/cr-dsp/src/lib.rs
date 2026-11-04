//! CleanGroove DSP Restoration Engine
//!
//! Independent reimplementation of Brian Davies' ClickRepair audio processing pipeline.
//! This crate contains pure DSP logic without I/O or platform dependencies.

pub mod analysis;
pub mod autotune;
pub mod burg;
pub mod channel;
pub mod detector;
pub mod flag;
pub mod kalman;
pub mod params;
pub mod post;
pub mod pitch;
pub mod process;
pub mod reconstruct;
pub mod rt;
pub mod subchannel;
pub mod thump;
pub mod wavelet;

pub use autotune::{
    autotune_audio, autotune_audio_with_analysis, autotune_audio_with_progress, AutoTuneReport, RecordCondition,
    SweepPointMetrics,
};
pub use analysis::{analyze_album, AlbumAnalysis, ProblemSpot, RepairEvent};
pub use process::ProcessorStats;
