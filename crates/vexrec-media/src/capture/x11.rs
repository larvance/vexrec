use std::time::Instant;
use vexrec_core::buffer::{FrameBacking, PixelFormat, PixelImage, VideoFrame};
use vexrec_core::error::Result;
use vexrec_core::geometry::Rect;
use vexrec_display::{DisplayBackend, X11Display};

/// Ultra-fast X11 MIT-SHM + XFixes cursor capture source for recording.
/// Uses a dedicated X11 connection separate from the UI overlay connection.
pub struct X11CaptureSource {
    disp: X11Display,
    crop: Option<Rect>,
    width: u32,
    height: u32,
    include_cursor: bool,
    start: Instant,
}

impl X11CaptureSource {
    pub fn open(crop: Option<Rect>, include_cursor: bool) -> Result<Self> {
        let disp = X11Display::connect()?;
        let screen = disp.primary_size();
        let (effective_crop, width, height) = match crop {
            Some(r) => {
                let c = r
                    .clamp_bounds(screen.width, screen.height)
                    .snap_even();
                let w = c.width.max(32) & !1;
                let h = c.height.max(32) & !1;
                let adjusted = Rect::new(c.x, c.y, w.min(screen.width), h.min(screen.height));
                (Some(adjusted), adjusted.width & !1, adjusted.height & !1)
            }
            None => (None, (screen.width & !1).max(32), (screen.height & !1).max(32)),
        };
        Ok(Self {
            disp,
            crop: effective_crop,
            width,
            height,
            include_cursor,
            start: Instant::now(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn capture_frame(&mut self) -> Result<VideoFrame> {
        let mut img = self.disp.capture_screen(self.crop, self.include_cursor)?;
        if img.width != self.width || img.height != self.height {
            let mut normalized = PixelImage::new(self.width, self.height, PixelFormat::Bgrx8);
            let copy_w = img.width.min(self.width) as usize;
            let copy_h = img.height.min(self.height) as usize;
            let src_stride = img.stride as usize;
            let dst_stride = normalized.stride as usize;
            let s = img.as_bytes();
            let d = normalized.as_mut_bytes();
            for row in 0..copy_h {
                d[row * dst_stride..row * dst_stride + copy_w * 4]
                    .copy_from_slice(&s[row * src_stride..row * src_stride + copy_w * 4]);
            }
            img = normalized;
        }
        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            format: img.format,
            pts_ns: self.start.elapsed().as_nanos() as u64,
            damaged: true,
            backing: FrameBacking::Host {
                stride: img.stride,
                data: img.data,
            },
        })
    }
}
