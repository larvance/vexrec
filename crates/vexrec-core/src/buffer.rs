use std::alloc::{self, Layout};
use std::os::fd::OwnedFd;
use std::ptr::NonNull;

/// Pixel formats flowing through display capture, UI rendering, and video encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// X11 native 32-bit ZPixmap / DRM XRGB8888 (memory byte order: B, G, R, X).
    Bgrx8,
    /// Little-endian 32-bit BGRA (memory byte order: B, G, R, A).
    Bgra8,
    /// Canonical PNG / Wayland clipboard RGBA (memory byte order: R, G, B, A).
    Rgba8,
    /// Hardware video encoder planar Y + interleaved UV (YUV420 8-bit).
    Nv12,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Bgrx8 | Self::Bgra8 | Self::Rgba8 => Some(4),
            Self::Nv12 => None,
        }
    }
}

/// 64-byte aligned heap buffer for AVX2 / DRM zero-fault vectorized kernels.
pub struct AlignedVec {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: AlignedVec owns its exclusively allocated memory block cleanly.
unsafe impl Send for AlignedVec {}
// SAFETY: Shared references only expose immutable byte slices.
unsafe impl Sync for AlignedVec {}

impl AlignedVec {
    pub const ALIGN: usize = 64;

    /// Allocates a zeroed 64-byte aligned byte buffer of length `len`.
    pub fn zeroed(len: usize) -> Self {
        if len == 0 {
            return Self {
                ptr: NonNull::dangling(),
                len: 0,
                layout: Layout::from_size_align(0, Self::ALIGN).unwrap(),
            };
        }
        let layout = Layout::from_size_align(len, Self::ALIGN)
            .expect("AlignedVec layout overflow");
        // SAFETY: size > 0 verified above; align is power of two (64).
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Self { ptr, len, layout }
    }

    /// Allocates an uninitialized 64-byte aligned byte buffer initialized immediately by caller.
    ///
    /// # Safety
    /// Caller must initialize bytes before reading them, or use `zeroed` if unsure.
    pub fn uninit_zeroed(len: usize) -> Self {
        Self::zeroed(len)
    }

    pub fn from_slice(src: &[u8]) -> Self {
        let mut out = Self::zeroed(src.len());
        out.as_mut_slice().copy_from_slice(src);
        out
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: ptr is valid for `self.len` bytes for the lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: ptr is exclusively owned by `&mut self` for `self.len` bytes.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.as_slice().to_vec()
    }
}

impl Clone for AlignedVec {
    fn clone(&self) -> Self {
        Self::from_slice(self.as_slice())
    }
}

impl std::fmt::Debug for AlignedVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedVec")
            .field("len", &self.len)
            .finish()
    }
}

impl Drop for AlignedVec {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: self.ptr was allocated via alloc_zeroed with self.layout.
            unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }
}

/// CPU-owned tightly or strided pixel image suitable for capture, preview, and PNG export.
#[derive(Clone, Debug)]
pub struct PixelImage {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub data: AlignedVec,
}

impl PixelImage {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        let bpp = format.bytes_per_pixel().unwrap_or(4) as u32;
        let stride = width.saturating_mul(bpp);
        let total = (stride as usize).saturating_mul(height as usize);
        Self {
            width,
            height,
            stride,
            format,
            data: AlignedVec::zeroed(total),
        }
    }

    pub fn from_tight_bytes(
        width: u32,
        height: u32,
        format: PixelFormat,
        bytes: &[u8],
    ) -> Option<Self> {
        let bpp = format.bytes_per_pixel()? as u32;
        let stride = width.saturating_mul(bpp);
        let expected = (stride as usize).checked_mul(height as usize)?;
        if bytes.len() < expected {
            return None;
        }
        Some(Self {
            width,
            height,
            stride,
            format,
            data: AlignedVec::from_slice(&bytes[..expected]),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// Crops a sub-rectangle cleanly in a single strided row copy.
    pub fn crop_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Option<Self> {
        let bpp = self.format.bytes_per_pixel()? as u32;
        let cx = x.min(self.width);
        let cy = y.min(self.height);
        let cw = w.min(self.width.saturating_sub(cx));
        let ch = h.min(self.height.saturating_sub(cy));
        if cw == 0 || ch == 0 {
            return None;
        }
        let dst_stride = cw * bpp;
        let mut out = AlignedVec::zeroed((dst_stride as usize) * (ch as usize));
        let src = self.data.as_slice();
        let dst = out.as_mut_slice();
        let src_stride = self.stride as usize;
        let row_bytes = dst_stride as usize;
        let src_x_off = (cx * bpp) as usize;

        for row in 0..(ch as usize) {
            let s_off = (cy as usize + row) * src_stride + src_x_off;
            let d_off = row * row_bytes;
            if s_off + row_bytes <= src.len() {
                dst[d_off..d_off + row_bytes].copy_from_slice(&src[s_off..s_off + row_bytes]);
            }
        }

        Some(Self {
            width: cw,
            height: ch,
            stride: dst_stride,
            format: self.format,
            data: out,
        })
    }
}

/// Backing storage for a live captured video frame (GPU DMA-BUF or host SHM/pooled RAM).
#[derive(Debug)]
pub enum FrameBacking {
    /// Zero-copy Linux kernel DMA-BUF descriptor + DRM FourCC + modifier.
    DmaBuf {
        fd: OwnedFd,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
    },
    /// CPU-accessible memory buffer.
    Host {
        stride: u32,
        data: AlignedVec,
    },
}

/// Timestamped captured frame routed through VPP + hardware encoders.
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pts_ns: u64,
    pub damaged: bool,
    pub backing: FrameBacking,
}
