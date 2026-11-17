pub mod bitstream;
pub mod ebml;
pub mod fmp4;

use std::fs::File;
use std::path::Path;
use vexrec_core::error::Result;
use vexrec_core::formats::{ContainerFormat, VideoCodec};

use ebml::EbmlMuxer;
use fmp4::FragmentedMp4Muxer;

pub enum MediaMuxer {
    Mp4(FragmentedMp4Muxer<File>),
    Ebml(EbmlMuxer<File>),
}

impl MediaMuxer {
    pub fn create(
        path: &Path,
        container: ContainerFormat,
        width: u32,
        height: u32,
        fps: u32,
        video_codec: VideoCodec,
        audio_channels: u8,
        audio_enabled: bool,
        opus_pre_skip: u16,
    ) -> Result<Self> {
        let file = File::create(path)?;
        match container {
            ContainerFormat::Mkv | ContainerFormat::Webm => Ok(Self::Ebml(EbmlMuxer::new(
                file,
                container,
                width,
                height,
                fps,
                audio_channels,
                audio_enabled,
                opus_pre_skip,
            )?)),
            ContainerFormat::Mp4 | ContainerFormat::Gif => Ok(Self::Mp4(FragmentedMp4Muxer::new(
                file,
                width,
                height,
                fps,
                video_codec,
                audio_channels,
                audio_enabled,
                opus_pre_skip,
            )?)),
        }
    }

    pub fn set_h264_params(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        match self {
            Self::Mp4(m) => m.set_h264_params(sps, pps),
            Self::Ebml(m) => m.set_h264_params(sps, pps),
        }
    }

    pub fn push_video_frame(&mut self, pts_ns: u64, is_key: bool, annexb: &[u8]) -> Result<()> {
        match self {
            Self::Mp4(m) => m.push_video_frame(pts_ns, is_key, annexb),
            Self::Ebml(m) => m.push_video_frame(pts_ns, is_key, annexb),
        }
    }

    pub fn push_audio_packet(&mut self, pts_ns: u64, opus_pkt: &[u8]) -> Result<()> {
        match self {
            Self::Mp4(m) => m.push_audio_packet(pts_ns, opus_pkt),
            Self::Ebml(m) => m.push_audio_packet(pts_ns, opus_pkt),
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        match self {
            Self::Mp4(m) => m.finish(),
            Self::Ebml(m) => m.finish(),
        }
    }
}
