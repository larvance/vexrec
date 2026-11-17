use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContainerFormat {
    #[default]
    Mp4,
    Mkv,
    Webm,
    Gif,
}

impl ContainerFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
            Self::Gif => "gif",
        }
    }

    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mkv => "video/x-matroska",
            Self::Webm => "video/webm",
            Self::Gif => "image/gif",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VideoCodec {
    #[default]
    Auto,
    H264,
    H265,
    Vp9,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioCodec {
    #[default]
    Auto,
    Aac,
    Opus,
    Flac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VideoQuality {
    Low,
    #[default]
    Medium,
    High,
    Lossless,
}

impl VideoQuality {
    /// Target constant quantization parameter (CQP/QP) for hardware H.264/HEVC encoders.
    pub const fn target_qp(self) -> u32 {
        match self {
            Self::Low => 28,
            Self::Medium => 21,
            Self::High => 16,
            Self::Lossless => 0,
        }
    }

    /// Target bitrate in bits/sec for CBR/VBR rate control at 1080p60 baseline.
    pub const fn target_bitrate_bps(self) -> u32 {
        match self {
            Self::Low => 3_500_000,
            Self::Medium => 8_000_000,
            Self::High => 18_000_000,
            Self::Lossless => 45_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EncoderBackendPreference {
    #[default]
    Auto,
    VulkanVideo,
    VaApi,
    Nvenc,
    Cpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    pub fn from_path(path: &std::path::Path) -> Self {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("jpg") | Some("jpeg") => Self::Jpeg,
            Some("webp") => Self::Webp,
            _ => Self::Png,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CompressionSpeed {
    #[default]
    Fast,
    Balanced,
    Compact,
}
