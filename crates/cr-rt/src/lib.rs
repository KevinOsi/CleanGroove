//! CleanGroove Real-Time Streaming Audio Engine

pub mod backend;
pub mod live;
pub mod tape;

pub use backend::devices::{common_rates, list_devices, scan_devices, DeviceInfo, DeviceScan};
pub use backend::pipe_host::{run_pipe_stream, run_pipe_stream_io};
pub use live::{LevelSnapshot, LiveConfig, LiveEvent, LiveSession, StopReport, StreamInfo};
pub use tape::{TapeHandle, TapeMonitoringSettings, TapeProducer, TapeRecorder, TapeSidecarMetadata};
