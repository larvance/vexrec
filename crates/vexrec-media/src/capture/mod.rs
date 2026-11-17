pub mod wayland_pipewire;
pub mod x11;

use vexrec_core::buffer::VideoFrame;
use vexrec_core::error::Result;
use vexrec_core::geometry::Rect;
use vexrec_display::detect_wayland_session_kind;

use wayland_pipewire::WaylandCaptureSource;
use x11::X11CaptureSource;

pub enum VideoCaptureSource {
    X11(X11CaptureSource),
    Wayland(WaylandCaptureSource),
}

impl VideoCaptureSource {
    pub fn open_auto(crop: Option<Rect>, fps: u32, include_cursor: bool) -> Result<Self> {
        let is_wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|s| s.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
            || std::env::var_os("WAYLAND_DISPLAY").is_some();

        if is_wayland {
            let kind = detect_wayland_session_kind();
            if let Ok(wl) = WaylandCaptureSource::open(kind, crop, fps, include_cursor) {
                return Ok(Self::Wayland(wl));
            }
        }

        match X11CaptureSource::open(crop, include_cursor) {
            Ok(x11) => Ok(Self::X11(x11)),
            Err(x_err) => {
                if !is_wayland {
                    let kind = detect_wayland_session_kind();
                    if let Ok(wl) = WaylandCaptureSource::open(kind, crop, fps, include_cursor) {
                        return Ok(Self::Wayland(wl));
                    }
                }
                Err(x_err)
            }
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::X11(s) => s.width(),
            Self::Wayland(s) => s.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::X11(s) => s.height(),
            Self::Wayland(s) => s.height(),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::X11(_) => "X11-MIT-SHM",
            Self::Wayland(WaylandCaptureSource::Screencopy { .. }) => "Wayland-wlr-screencopy",
            Self::Wayland(WaylandCaptureSource::PipeWire(_)) => "Wayland-PipeWire",
        }
    }

    pub fn capture_frame(&mut self) -> Result<VideoFrame> {
        match self {
            Self::X11(s) => s.capture_frame(),
            Self::Wayland(s) => s.capture_frame(),
        }
    }
}

/// Checks if `libpipewire-0.3.so.0` runtime shared library can be opened dynamically.
pub fn is_pipewire_available() -> bool {
    unsafe { libloading::Library::new("libpipewire-0.3.so.0") }.is_ok()
}
