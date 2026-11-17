pub mod buffer;
pub mod config;
pub mod error;
pub mod formats;
pub mod geometry;
pub mod session;
pub mod xdg;

pub use buffer::{AlignedVec, FrameBacking, PixelFormat, PixelImage, VideoFrame};
pub use config::{ensure_unique_path, LuxrecConfig, VexrecConfig};
pub use error::{LuxrecError, Result, VexrecError};
pub use formats::{
    AudioCodec, CompressionSpeed, ContainerFormat, EncoderBackendPreference, ImageFormat,
    VideoCodec, VideoQuality,
};
pub use geometry::{Point, Rect, Size};
pub use session::{ActiveSession, SessionMetadata, SessionState};
pub use xdg::expand_tilde;
