pub mod types;
pub mod wayland;
pub mod x11;

pub use types::{
    ClientWindowInfo, CursorKind, DisplayBackend, DisplayEvent, DmaBufHandle, HotkeyAction,
    KeyAction, MonitorInfo, SessionKind, WindowRole,
};
pub use x11::{sanitize_display_env, X11Display};
pub use wayland::{detect_wayland_session_kind, WaylandDisplay};

use vexrec_core::Result;

/// Auto-detects the active Linux display server and opens the fastest native backend.
pub fn connect_auto() -> Result<Box<dyn DisplayBackend>> {
    sanitize_display_env();

    let is_wayland_env = std::env::var("XDG_SESSION_TYPE")
        .map(|s| s.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some();

    if is_wayland_env {
        if let Ok(wl) = WaylandDisplay::connect() {
            return Ok(Box::new(wl));
        }
    }

    match X11Display::connect() {
        Ok(x11) => Ok(Box::new(x11)),
        Err(x_err) => {
            if let Ok(wl) = WaylandDisplay::connect() {
                return Ok(Box::new(wl));
            }
            Err(x_err)
        }
    }
}
