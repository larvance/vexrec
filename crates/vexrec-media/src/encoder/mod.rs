pub mod nvenc;
pub mod sw;
pub mod vaapi;
pub mod vulkan_video;

use tracing::{info, warn};
use vexrec_core::buffer::VideoFrame;
use vexrec_core::config::RecordingConfig;
use vexrec_core::error::Result;
use vexrec_core::formats::{EncoderBackendPreference, VideoCodec};

use sw::{OpenH264Encoder, Rav1eAv1Encoder};
use vaapi::VaApiH264Encoder;

pub struct EncodedVideoPacket {
    pub pts_ns: u64,
    pub is_keyframe: bool,
    pub data: Vec<u8>,
}

pub trait VideoEncoder: Send {
    fn backend_name(&self) -> &'static str;
    fn sps_pps(&self) -> Option<(&[u8], &[u8])>;
    fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedVideoPacket>>;
    fn flush(&mut self) -> Result<Vec<EncodedVideoPacket>>;
}

/// Creates the highest-throughput encoder supported cleanly on this Linux system,
/// falling back gracefully from hardware VA-API to OpenH264 / rav1e without failing.
pub fn create_best_encoder(
    width: u32,
    height: u32,
    cfg: &RecordingConfig,
) -> Result<Box<dyn VideoEncoder>> {
    let fps = cfg.framerate.max(1);
    let w = (width & !1).max(32);
    let h = (height & !1).max(32);

    if cfg.video_codec == VideoCodec::Av1 {
        info!("Initializing pure-Rust multi-threaded AV1 encoder (rav1e)");
        return Ok(Box::new(Rav1eAv1Encoder::new(w, h, fps, cfg.quality)?));
    }

    let try_vaapi = cfg.hardware_accel
        && matches!(
            cfg.encoder_preference,
            EncoderBackendPreference::Auto | EncoderBackendPreference::VaApi
        );

    if try_vaapi {
        match VaApiH264Encoder::new(w, h, fps, cfg.quality) {
            Ok(va) => {
                // Verify one dummy/capability probe? Keep direct return and wrap runtime fallback in recorder!
                return Ok(Box::new(va));
            }
            Err(e) => {
                warn!("VA-API DRM init skipped ({e}), selecting fast CPU OpenH264");
            }
        }
    }

    let enc = OpenH264Encoder::new(w, h, fps, cfg.quality)?;
    info!(backend = enc.backend_name(), w, h, fps, "Initialized video encoder");
    Ok(Box::new(enc))
}
