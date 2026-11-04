//! Live monitoring and capture: input device -> RT declicker -> output device, with the raw
//! input recorded on request. `LiveSession` is what the GUI and CLI drive.

pub mod engine;
pub mod recorder;
pub mod session;
pub mod waveform;

pub use engine::{LevelSnapshot, StreamPlan};
pub use recorder::{next_take_path, TapeReport, TapeState, TapeStatus, DEFAULT_TEMPLATE};
pub use session::{LiveConfig, LiveEvent, LiveSession, StopReport, StreamInfo};
pub use waveform::{WaveColumn, COLUMNS_PER_SEC};
