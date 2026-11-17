use std::fs::File;
use openh264::encoder::{
    BitRate, Encoder as OhEncoder, EncoderConfig as OhConfig, FrameRate, FrameType,
    IntraFramePeriod, RateControlMode, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use rav1e::prelude::*;
use vexrec_core::buffer::{FrameBacking, PixelFormat, VideoFrame};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::VideoQuality;
use vexrec_os::simd::bgra_to_nv12_bt709;

use super::{EncodedVideoPacket, VideoEncoder};
use crate::mux::bitstream::extract_h264_sps_pps;

/// Fast multi-threaded CPU H.264 encoder powered by OpenH264 + SIMD BT.709 I420 converter.
pub struct OpenH264Encoder {
    enc: OhEncoder,
    width: u32,
    height: u32,
    y_buf: Vec<u8>,
    uv_scratch: Vec<u8>,
    u_buf: Vec<u8>,
    v_buf: Vec<u8>,
    cached_sps_pps: Option<(Vec<u8>, Vec<u8>)>,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, quality: VideoQuality) -> Result<Self> {
        let w = (width & !1).max(32);
        let h = (height & !1).max(32);
        let bps = quality.target_bitrate_bps();

        let cfg = OhConfig::new()
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1) * 2))
            .num_threads(0)
            .vui(VuiConfig::bt709());

        let enc = OhEncoder::with_api_config(OpenH264API::from_source(), cfg)
            .map_err(|e| VexrecError::Encoding(format!("OpenH264 init error: {e}")))?;

        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        Ok(Self {
            enc,
            width: w,
            height: h,
            y_buf: vec![0u8; y_len],
            uv_scratch: vec![0u8; y_len / 2],
            u_buf: vec![0u8; uv_len],
            v_buf: vec![0u8; uv_len],
            cached_sps_pps: None,
        })
    }

    fn fill_i420_from_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let FrameBacking::Host { stride, ref data } = frame.backing else {
            return Err(VexrecError::Encoding("OpenH264 requires host pixel buffer".into()));
        };
        let src = data.as_slice();
        let w = self.width as usize;
        let h = self.height as usize;

        match frame.format {
            PixelFormat::Bgrx8 | PixelFormat::Bgra8 => {
                bgra_to_nv12_bt709(
                    src,
                    self.width,
                    self.height,
                    stride,
                    &mut self.y_buf,
                    self.width,
                    &mut self.uv_scratch,
                    self.width,
                );
                // De-interleave packed NV12 UV plane into planar U + V slices
                for (pair, (u, v)) in self
                    .uv_scratch
                    .chunks_exact(2)
                    .zip(self.u_buf.iter_mut().zip(self.v_buf.iter_mut()))
                {
                    *u = pair[0];
                    *v = pair[1];
                }
            }
            PixelFormat::Nv12 => {
                let y_len = w * h;
                if src.len() < y_len + y_len / 2 {
                    return Err(VexrecError::Encoding("Truncated NV12 buffer".into()));
                }
                self.y_buf.copy_from_slice(&src[..y_len]);
                for (pair, (u, v)) in src[y_len..y_len + y_len / 2]
                    .chunks_exact(2)
                    .zip(self.u_buf.iter_mut().zip(self.v_buf.iter_mut()))
                {
                    *u = pair[0];
                    *v = pair[1];
                }
            }
            PixelFormat::Rgba8 => {
                return Err(VexrecError::Encoding("Convert RGBA8 to BGRA8 before encode".into()));
            }
        }
        Ok(())
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn backend_name(&self) -> &'static str {
        "OpenH264 (CPU Multi-Threaded)"
    }

    fn sps_pps(&self) -> Option<(&[u8], &[u8])> {
        self.cached_sps_pps
            .as_ref()
            .map(|(s, p)| (s.as_slice(), p.as_slice()))
    }

    fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedVideoPacket>> {
        self.fill_i420_from_frame(frame)?;
        if force_keyframe {
            self.enc.force_intra_frame();
        }
        let w = self.width as usize;
        let h = self.height as usize;
        let yuv = YUVSlices::new(
            (&self.y_buf, &self.u_buf, &self.v_buf),
            (w, h),
            (w, w / 2, w / 2),
        );

        let bs = self
            .enc
            .encode(&yuv)
            .map_err(|e| VexrecError::Encoding(format!("OpenH264 encode failed: {e}")))?;
        let data = bs.to_vec();
        if data.is_empty() {
            return Ok(Vec::new());
        }

        if self.cached_sps_pps.is_none() {
            let (s, p) = extract_h264_sps_pps(&data);
            if let (Some(sps), Some(pps)) = (s, p) {
                self.cached_sps_pps = Some((sps, pps));
            }
        }

        let is_key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        Ok(vec![EncodedVideoPacket {
            pts_ns: frame.pts_ns,
            is_keyframe: is_key,
            data,
        }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedVideoPacket>> {
        Ok(Vec::new())
    }
}

/// Pure-Rust CPU AV1 encoder powered by `rav1e` (preset 10 low-latency).
pub struct Rav1eAv1Encoder {
    ctx: Context<u8>,
    width: usize,
    height: usize,
    y_buf: Vec<u8>,
    uv_scratch: Vec<u8>,
    u_buf: Vec<u8>,
    v_buf: Vec<u8>,
    pts_queue: std::collections::VecDeque<u64>,
}

impl Rav1eAv1Encoder {
    pub fn new(width: u32, height: u32, fps: u32, quality: VideoQuality) -> Result<Self> {
        let w = (width & !1).max(32) as usize;
        let h = (height & !1).max(32) as usize;
        let mut enc_cfg = EncoderConfig::with_speed_preset(10);
        enc_cfg.width = w;
        enc_cfg.height = h;
        enc_cfg.bit_depth = 8;
        enc_cfg.chroma_sampling = ChromaSampling::Cs420;
        enc_cfg.time_base = Rational::new(1, fps.max(1) as u64);
        enc_cfg.low_latency = true;
        enc_cfg.bitrate = quality.target_bitrate_bps() as i32;
        enc_cfg.min_key_frame_interval = fps.max(1) as u64;
        enc_cfg.max_key_frame_interval = (fps.max(1) * 2) as u64;

        let cfg = Config::new().with_encoder_config(enc_cfg);
        let ctx: Context<u8> = cfg
            .new_context()
            .map_err(|e| VexrecError::Encoding(format!("rav1e context error: {e}")))?;

        let y_len = w * h;
        Ok(Self {
            ctx,
            width: w,
            height: h,
            y_buf: vec![0u8; y_len],
            uv_scratch: vec![0u8; y_len / 2],
            u_buf: vec![0u8; y_len / 4],
            v_buf: vec![0u8; y_len / 4],
            pts_queue: std::collections::VecDeque::new(),
        })
    }
}

impl VideoEncoder for Rav1eAv1Encoder {
    fn backend_name(&self) -> &'static str {
        "rav1e (Pure Rust AV1)"
    }

    fn sps_pps(&self) -> Option<(&[u8], &[u8])> {
        None
    }

    fn encode(&mut self, frame: &VideoFrame, _force_keyframe: bool) -> Result<Vec<EncodedVideoPacket>> {
        let FrameBacking::Host { stride, ref data } = frame.backing else {
            return Err(VexrecError::Encoding("rav1e requires host buffer".into()));
        };
        bgra_to_nv12_bt709(
            data.as_slice(),
            self.width as u32,
            self.height as u32,
            stride,
            &mut self.y_buf,
            self.width as u32,
            &mut self.uv_scratch,
            self.width as u32,
        );
        for (pair, (u, v)) in self
            .uv_scratch
            .chunks_exact(2)
            .zip(self.u_buf.iter_mut().zip(self.v_buf.iter_mut()))
        {
            *u = pair[0];
            *v = pair[1];
        }

        let mut rf = self.ctx.new_frame();
        rf.planes[0].copy_from_raw_u8(&self.y_buf, self.width, 1);
        rf.planes[1].copy_from_raw_u8(&self.u_buf, self.width / 2, 1);
        rf.planes[2].copy_from_raw_u8(&self.v_buf, self.width / 2, 1);

        self.pts_queue.push_back(frame.pts_ns);
        self.ctx
            .send_frame(rf)
            .map_err(|e| VexrecError::Encoding(format!("rav1e send_frame error: {e}")))?;

        let mut packets = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    let pts_ns = self.pts_queue.pop_front().unwrap_or(frame.pts_ns);
                    packets.push(EncodedVideoPacket {
                        pts_ns,
                        is_keyframe: pkt.frame_type == rav1e::data::FrameType::KEY,
                        data: pkt.data,
                    });
                }
                Err(EncoderStatus::NeedMoreData) | Err(EncoderStatus::Encoded) => break,
                Err(EncoderStatus::LimitReached) => break,
                Err(e) => {
                    return Err(VexrecError::Encoding(format!("rav1e receive_packet error: {e}")));
                }
            }
        }
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<EncodedVideoPacket>> {
        self.ctx.flush();
        let mut packets = Vec::new();
        while let Ok(pkt) = self.ctx.receive_packet() {
            let pts_ns = self.pts_queue.pop_front().unwrap_or(0);
            packets.push(EncodedVideoPacket {
                pts_ns,
                is_keyframe: pkt.frame_type == rav1e::data::FrameType::KEY,
                data: pkt.data,
            });
        }
        Ok(packets)
    }
}

/// Self-contained animated GIF stream encoder for `ContainerFormat::Gif`.
pub struct GifRecordingSink {
    enc: gif::Encoder<File>,
    width: u16,
    height: u16,
    rgba_scratch: Vec<u8>,
    last_pts_ns: Option<u64>,
}

impl GifRecordingSink {
    pub fn new(path: &std::path::Path, width: u32, height: u32) -> Result<Self> {
        let w = width.min(u16::MAX as u32) as u16;
        let h = height.min(u16::MAX as u32) as u16;
        let file = File::create(path)?;
        let mut enc = gif::Encoder::new(file, w, h, &[])
            .map_err(|e| VexrecError::Encoding(format!("GIF encoder error: {e}")))?;
        enc.set_repeat(gif::Repeat::Infinite)
            .map_err(|e| VexrecError::Encoding(e.to_string()))?;
        Ok(Self {
            enc,
            width: w,
            height: h,
            rgba_scratch: vec![0u8; w as usize * h as usize * 4],
            last_pts_ns: None,
        })
    }

    pub fn push_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let FrameBacking::Host { stride, ref data } = frame.backing else {
            return Ok(());
        };
        let row_bytes = self.width as usize * 4;
        let src = data.as_slice();
        for row in 0..self.height as usize {
            let s = &src[row * stride as usize..row * stride as usize + row_bytes];
            let d = &mut self.rgba_scratch[row * row_bytes..row * row_bytes + row_bytes];
            vexrec_os::simd::bgrx_to_rgba(s, d);
        }

        let delay_cs = self
            .last_pts_ns
            .map(|prev| ((frame.pts_ns.saturating_sub(prev)) / 10_000_000).clamp(2, 100) as u16)
            .unwrap_or(5);
        self.last_pts_ns = Some(frame.pts_ns);

        let mut gf = gif::Frame::from_rgba_speed(self.width, self.height, &mut self.rgba_scratch, 15);
        gf.delay = delay_cs;
        self.enc
            .write_frame(&gf)
            .map_err(|e| VexrecError::Encoding(format!("GIF write_frame failed: {e}")))?;
        Ok(())
    }
}
