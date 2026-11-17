use std::os::fd::OwnedFd;
use vexrec_core::buffer::PixelImage;
use vexrec_core::geometry::{Rect, Size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    X11,
    WaylandHyprland,
    WaylandSway,
    WaylandKde,
    WaylandGnome,
    WaylandWlroots,
}

impl SessionKind {
    pub const fn is_wayland(self) -> bool {
        !matches!(self, Self::X11)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::X11 => "X11",
            Self::WaylandHyprland => "Wayland (Hyprland)",
            Self::WaylandSway => "Wayland (Sway)",
            Self::WaylandKde => "Wayland (KDE Plasma)",
            Self::WaylandGnome => "Wayland (GNOME Mutter)",
            Self::WaylandWlroots => "Wayland (wlroots)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowRole {
    OverlayFullscreen,
    IndicatorPill,
    RecordingFrame,
    SettingsDialog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorKind {
    Default,
    Crosshair,
    Pointer,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    Freeze,
    StopRecording,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Escape,
    Return,
    Print,
    CtrlC,
    CtrlS,
    Num(u8),
    Backspace,
    Tab,
    Char(char),
    Other,
}

#[derive(Debug, Clone)]
pub enum DisplayEvent {
    Expose { role: WindowRole, width: u32, height: u32 },
    PointerMotion { role: WindowRole, x: f64, y: f64 },
    PointerButton { role: WindowRole, button: u32, pressed: bool, x: f64, y: f64 },
    PointerLeave { role: WindowRole },
    KeyPress { role: WindowRole, key: KeyAction },
    WindowDamaged { window_id: u64 },
    GlobalHotkey(HotkeyAction),
    SurfaceConfigured { role: WindowRole, width: u32, height: u32, scale_factor: f64 },
    CloseRequested { role: WindowRole },
}

#[derive(Debug)]
pub struct DmaBufHandle {
    pub fd: OwnedFd,
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
    pub scale_factor: f64,
}

#[derive(Debug, Clone)]
pub struct ClientWindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub rect: Rect,
    pub icon: Option<PixelImage>,
    pub preview: Option<PixelImage>,
}

/// Abstracts OS display operations uniformly across bare-metal X11 and Wayland compositors.
pub trait DisplayBackend {
    fn session_kind(&self) -> SessionKind;
    fn primary_size(&self) -> Size;
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn raw_fd(&self) -> std::os::fd::RawFd;

    /// Captures the whole desktop (or cropped screen rectangle) immediately using fastest OS path.
    fn capture_screen(&mut self, crop: Option<Rect>, include_cursor: bool) -> vexrec_core::Result<PixelImage>;

    /// Enumerates active top-level windows with cached icons and live/unobscured previews.
    fn enumerate_windows(&mut self, capture_previews: bool) -> Vec<ClientWindowInfo>;

    /// Refreshes live buffer for a single window (called incrementally on XDamage / compositor updates).
    fn capture_window(&mut self, window_id: u64) -> vexrec_core::Result<PixelImage>;

    /// Maps/shows or updates position/size of a managed UI role surface.
    fn show_surface(&mut self, role: WindowRole, geom: Rect);
    fn hide_surface(&mut self, role: WindowRole);
    fn is_surface_visible(&self, role: WindowRole) -> bool;

    /// Presents BGRA8 pixel buffer onto a surface with optional dirty damage optimization.
    fn present_surface_bgra(
        &mut self,
        role: WindowRole,
        width: u32,
        height: u32,
        stride: u32,
        bgra: &[u8],
        dirty: Option<Rect>,
    ) -> vexrec_core::Result<()>;

    fn set_cursor(&mut self, role: WindowRole, cursor: CursorKind);
    fn grab_overlay_keyboard(&mut self) -> bool;
    fn ungrab_overlay_keyboard(&mut self);

    /// Registers global hotkeys natively (`XGrabKey` on X11; Portal/compositor on Wayland).
    fn register_global_hotkeys(&mut self);

    /// Owns clipboard lazily inside the running daemon (`0 ms` perceived completion).
    fn set_clipboard_image(&mut self, img: PixelImage) -> vexrec_core::Result<()>;

    /// Drains all pending OS events and translates them to `DisplayEvent`s cleanly without blocking.
    fn poll_events(&mut self) -> Vec<DisplayEvent>;

    /// Flushes requests and synchronizes compositor repaint frames (essential for zero self-capture).
    fn sync_and_wait_frame(&mut self);

    /// Optional DRI3 DMA-BUF handle extraction for zero-copy GPU capture/compositing on X11.
    fn try_export_dmabuf(&mut self, window_id: Option<u64>) -> Option<DmaBufHandle> {
        let _ = window_id;
        None
    }
}
