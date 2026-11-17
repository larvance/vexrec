pub mod audio;
pub mod capture;
pub mod encoder;
pub mod mux;
pub mod recorder;
pub mod snapshot;

pub use audio::{AudioMixerEngine, EncodedAudioPacket};
pub use capture::{is_pipewire_available, VideoCaptureSource};
pub use encoder::{create_best_encoder, EncodedVideoPacket, VideoEncoder};
pub use mux::MediaMuxer;
pub use recorder::{rms_to_dbfs, RecordingPipeline, RecordingSummary};
pub use snapshot::save_snapshot;
