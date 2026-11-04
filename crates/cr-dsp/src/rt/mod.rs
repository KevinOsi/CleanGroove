//! CleanGroove Real-Time (RT) DSP Engine
//!
//! Reimplements the methods of ClickRepairRT; see docs/engine/REALTIME_ENGINE.md.

pub mod channel;
pub mod detector;
pub mod pitch;
pub mod process;

pub use channel::RtChannel;
pub use detector::{CHANNEL_BUFFER_LEN, CHANNEL_BUFFER_MASK, SUPER_BUFFER_LEN, SUPER_BUFFER_MASK};
pub use process::{RtProcessor, RtTelemetry};
