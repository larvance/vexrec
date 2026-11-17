use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalList};
use smithay_client_toolkit::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{
    PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use tracing::info;
use vexrec_core::buffer::{PixelFormat, PixelImage};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::geometry::{Rect, Size};
use vexrec_core::CompressionSpeed;
use vexrec_os::dbus::{kwin_capture_workspace_pipe, portal_screenshot};
use vexrec_os::png_enc::{decode_png_to_bgrx, encode_png_bytes};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape as CursorShape, WpCursorShapeDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_source_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::types::{
    ClientWindowInfo, CursorKind, DisplayBackend, DisplayEvent, KeyAction, MonitorInfo,
    SessionKind, WindowRole,
};

pub fn detect_wayland_session_kind() -> SessionKind {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return SessionKind::WaylandHyprland;
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return SessionKind::WaylandSway;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_uppercase();
    if desktop.contains("KDE") || std::env::var_os("KDE_FULL_SESSION").is_some() {
        return SessionKind::WaylandKde;
    }
    if desktop.contains("GNOME") || desktop.contains("UBUNTU") {
        return SessionKind::WaylandGnome;
    }
    SessionKind::WaylandWlroots
}

enum SurfaceShell {
    Layer(LayerSurface),
    Xdg(Window),
}

struct ManagedSurface {
    role: WindowRole,
    wl_surface: wl_surface::WlSurface,
    shell: SurfaceShell,
    width: u32,
    height: u32,
    visible: bool,
    configured: bool,
}

#[derive(Default)]
struct ScreencopyCaptureState {
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
    ready: bool,
    failed: bool,
    buffer_params_done: bool,
}

struct WlState {
    session_kind: SessionKind,
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    shm: Shm,
    seat_state: SeatState,
    layer_shell: Option<LayerShell>,
    xdg_shell: XdgShell,
    cursor_shape_mgr: Option<CursorShapeManager>,
    screencopy_mgr: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    data_control_mgr: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    data_control_device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    data_source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_shape: Option<WpCursorShapeDeviceV1>,
    last_pointer_enter_serial: u32,
    last_pointer_press_serial: u32,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    active_seat: Option<wl_seat::WlSeat>,
    ctrl_down: bool,
    shift_down: bool,
    surfaces: HashMap<WindowRole, ManagedSurface>,
    pool: Option<SlotPool>,
    events: Vec<DisplayEvent>,
    clipboard_image: Option<PixelImage>,
    clipboard_png_cache: Option<Arc<Vec<u8>>>,
    screencopy: ScreencopyCaptureState,
}

impl WlState {
    fn role_for_surface(&self, s: &wl_surface::WlSurface) -> Option<WindowRole> {
        self.surfaces
            .values()
            .find(|ms| ms.wl_surface == *s)
            .map(|ms| ms.role)
    }

    fn map_evdev_key(&self, keycode: u32) -> KeyAction {
        match keycode {
            1 => KeyAction::Escape,
            28 | 96 => KeyAction::Return,
            99 => KeyAction::Print,
            14 => KeyAction::Backspace,
            15 => KeyAction::Tab,
            2 => KeyAction::Num(1),
            3 => KeyAction::Num(2),
            4 => KeyAction::Num(3),
            5 => KeyAction::Num(4),
            46 if self.ctrl_down => KeyAction::CtrlC,
            31 if self.ctrl_down => KeyAction::CtrlS,
            46 => KeyAction::Char(if self.shift_down { 'C' } else { 'c' }),
            31 => KeyAction::Char(if self.shift_down { 'S' } else { 's' }),
            _ => KeyAction::Other,
        }
    }
}

impl CompositorHandler for WlState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(role) = self.role_for_surface(surface) {
            if let Some(ms) = self.surfaces.get(&role) {
                self.events.push(DisplayEvent::Expose {
                    role,
                    width: ms.width,
                    height: ms.height,
                });
            }
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for WlState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for WlState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for WlState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.active_seat.is_none() {
            self.active_seat = Some(seat.clone());
            if let Some(ref mgr) = self.data_control_mgr {
                self.data_control_device = Some(mgr.get_data_device(&seat, qh, ()));
            }
        }
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            if let Ok(ptr) = self.seat_state.get_pointer(qh, &seat) {
                if let Some(ref mgr) = self.cursor_shape_mgr {
                    self.pointer_shape = Some(mgr.get_shape_device(&ptr, qh));
                }
                self.pointer = Some(ptr);
            }
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            let kbd = seat.get_keyboard(qh, ());
            self.keyboard = Some(kbd);
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
            self.pointer_shape = None;
        }
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for WlState {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for ev in events {
            let Some(role) = self.role_for_surface(&ev.surface) else {
                continue;
            };
            match ev.kind {
                PointerEventKind::Enter { serial } => {
                    self.last_pointer_enter_serial = serial;
                }
                PointerEventKind::Leave { .. } => {
                    self.events.push(DisplayEvent::PointerLeave { role });
                }
                PointerEventKind::Motion { .. } => {
                    self.events.push(DisplayEvent::PointerMotion {
                        role,
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                }
                PointerEventKind::Press { button, serial, .. } => {
                    self.last_pointer_press_serial = serial;
                    let b = match button {
                        BTN_LEFT => 1,
                        BTN_MIDDLE => 2,
                        BTN_RIGHT => 3,
                        o => o,
                    };
                    self.events.push(DisplayEvent::PointerButton {
                        role,
                        button: b,
                        pressed: true,
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    let b = match button {
                        BTN_LEFT => 1,
                        BTN_MIDDLE => 2,
                        BTN_RIGHT => 3,
                        o => o,
                    };
                    self.events.push(DisplayEvent::PointerButton {
                        role,
                        button: b,
                        pressed: false,
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

impl LayerShellHandler for WlState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        let role_opt = self.surfaces.iter().find_map(|(r, ms)| match ms.shell {
            SurfaceShell::Layer(ref ls) if ls == layer => Some(*r),
            _ => None,
        });
        if let Some(role) = role_opt {
            self.events.push(DisplayEvent::CloseRequested { role });
        }
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let role_opt = self.surfaces.iter().find_map(|(r, ms)| match ms.shell {
            SurfaceShell::Layer(ref ls) if ls == layer => Some(*r),
            _ => None,
        });
        if let Some(role) = role_opt {
            let (w, h) = (configure.new_size.0.max(1), configure.new_size.1.max(1));
            if let Some(ms) = self.surfaces.get_mut(&role) {
                if configure.new_size.0 > 0 {
                    ms.width = w;
                }
                if configure.new_size.1 > 0 {
                    ms.height = h;
                }
                ms.configured = true;
                self.events.push(DisplayEvent::SurfaceConfigured {
                    role,
                    width: ms.width,
                    height: ms.height,
                    scale_factor: 1.0,
                });
            }
        }
    }
}

impl WindowHandler for WlState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        let role_opt = self.surfaces.iter().find_map(|(r, ms)| match ms.shell {
            SurfaceShell::Xdg(ref w) if w == window => Some(*r),
            _ => None,
        });
        if let Some(role) = role_opt {
            self.events.push(DisplayEvent::CloseRequested { role });
        }
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, configure: WindowConfigure, _: u32) {
        let role_opt = self.surfaces.iter().find_map(|(r, ms)| match ms.shell {
            SurfaceShell::Xdg(ref w) if w == window => Some(*r),
            _ => None,
        });
        if let Some(role) = role_opt {
            if let Some(ms) = self.surfaces.get_mut(&role) {
                if let Some(w) = configure.new_size.0 {
                    ms.width = w.get();
                }
                if let Some(h) = configure.new_size.1 {
                    ms.height = h.get();
                }
                ms.configured = true;
                self.events.push(DisplayEvent::SurfaceConfigured {
                    role,
                    width: ms.width,
                    height: ms.height,
                    scale_factor: 1.0,
                });
            }
        }
    }
}

impl ProvidesRegistryState for WlState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(WlState);
delegate_output!(WlState);
delegate_shm!(WlState);
delegate_seat!(WlState);
delegate_pointer!(WlState);
delegate_layer!(WlState);
delegate_xdg_shell!(WlState);
delegate_xdg_window!(WlState);
delegate_registry!(WlState);

// Raw protocol Dispatches for wl_keyboard, wlr-screencopy, wlr-data-control, and wp_cursor_shape_device_v1
impl Dispatch<wl_keyboard::WlKeyboard, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                state.shift_down = (mods_depressed & 1) != 0;
                state.ctrl_down = (mods_depressed & 4) != 0;
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                let mapped = state.map_evdev_key(key);
                let target_role = if state
                    .surfaces
                    .get(&WindowRole::SettingsDialog)
                    .map(|s| s.visible)
                    .unwrap_or(false)
                {
                    WindowRole::SettingsDialog
                } else {
                    WindowRole::OverlayFullscreen
                };
                state.events.push(DisplayEvent::KeyPress {
                    role: target_role,
                    key: mapped,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for WlState {
    fn event(_: &mut Self, _: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _: zwlr_screencopy_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(fmt),
                width,
                height,
                stride,
            } => {
                if state.screencopy.format.is_none()
                    || matches!(fmt, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888)
                {
                    state.screencopy.format = Some(fmt);
                    state.screencopy.width = width;
                    state.screencopy.height = height;
                    state.screencopy.stride = stride;
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                state.screencopy.buffer_params_done = true;
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags: WEnum::Value(f) } => {
                state.screencopy.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.screencopy.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.screencopy.failed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for WlState {
    fn event(_: &mut Self, _: &zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _: zwlr_data_control_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for WlState {
    fn event(_: &mut Self, _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, _: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                if state.clipboard_png_cache.is_none() {
                    if let Some(ref img) = state.clipboard_image {
                        if let Ok(encoded) = encode_png_bytes(img, CompressionSpeed::Fast) {
                            state.clipboard_png_cache = Some(Arc::new(encoded));
                        }
                    }
                }
                if let Some(bytes) = state.clipboard_png_cache.clone() {
                    std::thread::spawn(move || {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&bytes);
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.clipboard_image = None;
                state.clipboard_png_cache = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for WlState {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: <WpCursorShapeDeviceV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

pub struct WaylandDisplay {
    conn: Connection,
    event_queue: EventQueue<WlState>,
    qh: QueueHandle<WlState>,
    state: WlState,
}

impl WaylandDisplay {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .map_err(|e| VexrecError::Display(format!("Wayland connect failed: {e}")))?;

        let (globals, mut event_queue): (GlobalList, EventQueue<WlState>) =
            registry_queue_init(&conn)
                .map_err(|e| VexrecError::Display(format!("Wayland registry init failed: {e}")))?;
        let qh = event_queue.handle();

        let registry_state = RegistryState::new(&globals);
        let compositor_state = CompositorState::bind(&globals, &qh)
            .map_err(|e| VexrecError::Display(format!("wl_compositor unavailable: {e}")))?;
        let output_state = OutputState::new(&globals, &qh);
        let shm = Shm::bind(&globals, &qh)
            .map_err(|e| VexrecError::Display(format!("wl_shm unavailable: {e}")))?;
        let seat_state = SeatState::new(&globals, &qh);
        let layer_shell = LayerShell::bind(&globals, &qh).ok();
        let xdg_shell = XdgShell::bind(&globals, &qh)
            .map_err(|e| VexrecError::Display(format!("xdg_wm_base unavailable: {e}")))?;
        let cursor_shape_mgr = CursorShapeManager::bind(&globals, &qh).ok();
        let screencopy_mgr = globals
            .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
            .ok();
        let data_control_mgr = globals
            .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
            .ok();

        let session_kind = detect_wayland_session_kind();
        let pool = SlotPool::new(1920 * 1080 * 4 * 2, &shm).ok();

        let mut state = WlState {
            session_kind,
            registry_state,
            compositor_state,
            output_state,
            shm,
            seat_state,
            layer_shell,
            xdg_shell,
            cursor_shape_mgr,
            screencopy_mgr,
            data_control_mgr,
            data_control_device: None,
            data_source: None,
            pointer: None,
            pointer_shape: None,
            last_pointer_enter_serial: 0,
            last_pointer_press_serial: 0,
            keyboard: None,
            active_seat: None,
            ctrl_down: false,
            shift_down: false,
            surfaces: HashMap::new(),
            pool,
            events: Vec::new(),
            clipboard_image: None,
            clipboard_png_cache: None,
            screencopy: ScreencopyCaptureState::default(),
        };

        let _ = event_queue.roundtrip(&mut state);
        info!(
            kind = session_kind.name(),
            layer_shell = state.layer_shell.is_some(),
            screencopy = state.screencopy_mgr.is_some(),
            data_control = state.data_control_mgr.is_some(),
            "Initialized bare-metal Wayland display backend"
        );

        Ok(Self {
            conn,
            event_queue,
            qh,
            state,
        })
    }

    fn capture_via_screencopy(&mut self, crop: Option<Rect>, include_cursor: bool) -> Result<PixelImage> {
        let mgr = self
            .state
            .screencopy_mgr
            .clone()
            .ok_or_else(|| VexrecError::Display("zwlr_screencopy_manager_v1 unavailable".into()))?;
        let output = self
            .state
            .output_state
            .outputs()
            .next()
            .ok_or_else(|| VexrecError::Display("No Wayland output found".into()))?;

        self.state.screencopy = ScreencopyCaptureState::default();
        let cursor_flag = i32::from(include_cursor);
        let frame = match crop {
            Some(r) => mgr.capture_output_region(
                cursor_flag,
                &output,
                r.x,
                r.y,
                r.width.max(1) as i32,
                r.height.max(1) as i32,
                &self.qh,
                (),
            ),
            None => mgr.capture_output(cursor_flag, &output, &self.qh, ()),
        };

        // Roundtrip until compositor sends buffer parameters
        for _ in 0..8 {
            let _ = self.event_queue.roundtrip(&mut self.state);
            if self.state.screencopy.buffer_params_done
                || self.state.screencopy.format.is_some()
                || self.state.screencopy.failed
            {
                break;
            }
        }

        if self.state.screencopy.failed || self.state.screencopy.width == 0 {
            frame.destroy();
            return Err(VexrecError::Display("wlr-screencopy buffer negotiation failed".into()));
        }

        let fmt = self.state.screencopy.format.unwrap_or(wl_shm::Format::Argb8888);
        let w = self.state.screencopy.width;
        let h = self.state.screencopy.height;
        let stride = self.state.screencopy.stride;

        let mut cap_pool = SlotPool::new((stride as usize) * (h as usize), &self.state.shm)
            .map_err(|e| VexrecError::Display(format!("SlotPool alloc failed: {e}")))?;
        let (buf, _) = cap_pool
            .create_buffer(w as i32, h as i32, stride as i32, fmt)
            .map_err(|e| VexrecError::Display(format!("wl_buffer alloc failed: {e}")))?;

        frame.copy(buf.wl_buffer());
        while !self.state.screencopy.ready && !self.state.screencopy.failed {
            self.event_queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| VexrecError::Display(e.to_string()))?;
        }
        frame.destroy();

        if self.state.screencopy.failed {
            return Err(VexrecError::Display("wlr-screencopy copy failed".into()));
        }

        let raw_bytes = cap_pool
            .canvas(&buf)
            .ok_or_else(|| VexrecError::Display("Cannot read screencopy SHM canvas".into()))?;
        let row_bytes = (w as usize) * 4;
        let mut img = PixelImage::new(w, h, PixelFormat::Bgrx8);
        let dst = img.as_mut_bytes();
        let invert = self.state.screencopy.y_invert;

        for y in 0..h as usize {
            let src_y = if invert { h as usize - 1 - y } else { y };
            let s_off = src_y * stride as usize;
            let d_off = y * row_bytes;
            if s_off + row_bytes <= raw_bytes.len() && d_off + row_bytes <= dst.len() {
                if fmt == wl_shm::Format::Xbgr8888 || fmt == wl_shm::Format::Abgr8888 {
                    // Swap R <-> B cleanly when compositor gives XBGR
                    for (sp, dp) in raw_bytes[s_off..s_off + row_bytes]
                        .chunks_exact(4)
                        .zip(dst[d_off..d_off + row_bytes].chunks_exact_mut(4))
                    {
                        dp[0] = sp[2];
                        dp[1] = sp[1];
                        dp[2] = sp[0];
                        dp[3] = 255;
                    }
                } else {
                    dst[d_off..d_off + row_bytes].copy_from_slice(&raw_bytes[s_off..s_off + row_bytes]);
                }
            }
        }
        Ok(img)
    }

    fn ensure_role_surface(&mut self, role: WindowRole, geom: Rect) -> Result<()> {
        if self.state.surfaces.contains_key(&role) {
            if let Some(ms) = self.state.surfaces.get(&role) {
                if role == WindowRole::RecordingFrame {
                    if let SurfaceShell::Layer(ref ls) = ms.shell {
                        ls.set_size(geom.width.max(1), geom.height.max(1));
                        ls.set_margin(geom.y, 0, 0, geom.x);
                        ls.commit();
                    }
                }
            }
            return Ok(());
        }

        let wl_surface = self.state.compositor_state.create_surface(&self.qh);
        let shell = match (&self.state.layer_shell, role) {
            (Some(ls_mgr), WindowRole::OverlayFullscreen) => {
                let ls = ls_mgr.create_layer_surface(
                    &self.qh,
                    wl_surface.clone(),
                    Layer::Overlay,
                    Some("vexrec-overlay"),
                    None,
                );
                ls.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                ls.set_exclusive_zone(-1);
                ls.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
                ls.commit();
                SurfaceShell::Layer(ls)
            }
            (Some(ls_mgr), WindowRole::IndicatorPill) => {
                let ls = ls_mgr.create_layer_surface(
                    &self.qh,
                    wl_surface.clone(),
                    Layer::Overlay,
                    Some("vexrec-indicator"),
                    None,
                );
                ls.set_anchor(Anchor::TOP | Anchor::RIGHT);
                ls.set_size(geom.width.max(100), geom.height.max(28));
                ls.set_margin(38, 20, 0, 0);
                ls.set_keyboard_interactivity(KeyboardInteractivity::None);
                ls.commit();
                SurfaceShell::Layer(ls)
            }
            (Some(ls_mgr), WindowRole::RecordingFrame) => {
                if let Ok(empty_reg) = smithay_client_toolkit::compositor::Region::new(&self.state.compositor_state) {
                    wl_surface.set_input_region(Some(empty_reg.wl_region()));
                }
                let ls = ls_mgr.create_layer_surface(
                    &self.qh,
                    wl_surface.clone(),
                    Layer::Overlay,
                    Some("vexrec-frame"),
                    None,
                );
                ls.set_anchor(Anchor::TOP | Anchor::LEFT);
                ls.set_exclusive_zone(-1);
                ls.set_size(geom.width.max(1), geom.height.max(1));
                ls.set_margin(geom.y.max(0), 0, 0, geom.x.max(0));
                ls.set_keyboard_interactivity(KeyboardInteractivity::None);
                ls.commit();
                SurfaceShell::Layer(ls)
            }
            (_, _) => {
                let decorations = if role == WindowRole::SettingsDialog {
                    WindowDecorations::None
                } else {
                    WindowDecorations::RequestServer
                };
                let win = self.state.xdg_shell.create_window(
                    wl_surface.clone(),
                    decorations,
                    &self.qh,
                );
                match role {
                    WindowRole::OverlayFullscreen => {
                        win.set_title("");
                        win.set_fullscreen(None);
                    }
                    WindowRole::IndicatorPill => {
                        win.set_title("Vexrec Indicator");
                    }
                    WindowRole::RecordingFrame => {
                        if let Ok(empty_reg) = smithay_client_toolkit::compositor::Region::new(&self.state.compositor_state) {
                            wl_surface.set_input_region(Some(empty_reg.wl_region()));
                        }
                    }
                    WindowRole::SettingsDialog => {
                        let fw = geom.width.max(1);
                        let fh = geom.height.max(1);
                        win.set_title("Vexrec Preferences");
                        win.set_app_id("vexrec");
                        win.set_min_size(Some((fw, fh)));
                        win.set_max_size(Some((fw, fh)));
                    }
                }
                win.commit();
                SurfaceShell::Xdg(win)
            }
        };

        self.state.surfaces.insert(
            role,
            ManagedSurface {
                role,
                wl_surface,
                shell,
                width: geom.width.max(1),
                height: geom.height.max(1),
                visible: true,
                configured: false,
            },
        );
        let _ = self.event_queue.roundtrip(&mut self.state);
        Ok(())
    }
}

impl DisplayBackend for WaylandDisplay {
    fn session_kind(&self) -> SessionKind {
        self.state.session_kind
    }

    fn primary_size(&self) -> Size {
        for out in self.state.output_state.outputs() {
            if let Some(info) = self.state.output_state.info(&out) {
                if let Some((w, h)) = info.logical_size.or(info.modes.first().map(|m| m.dimensions)) {
                    if w > 0 && h > 0 {
                        return Size::new(w as u32, h as u32);
                    }
                }
            }
        }
        Size::new(1920, 1080)
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        let mut res = Vec::new();
        for (idx, out) in self.state.output_state.outputs().enumerate() {
            if let Some(info) = self.state.output_state.info(&out) {
                let (w, h) = info.logical_size.unwrap_or((1920, 1080));
                let (x, y) = info.logical_position.unwrap_or((0, 0));
                res.push(MonitorInfo {
                    name: info.name.unwrap_or_else(|| format!("Monitor-{idx}")),
                    rect: Rect::new(x, y, w.max(1) as u32, h.max(1) as u32),
                    primary: idx == 0,
                    scale_factor: info.scale_factor as f64,
                });
            }
        }
        if res.is_empty() {
            res.push(MonitorInfo {
                name: "Primary".into(),
                rect: Rect::new(0, 0, 1920, 1080),
                primary: true,
                scale_factor: 1.0,
            });
        }
        res
    }

    fn raw_fd(&self) -> RawFd {
        self.conn.as_fd().as_raw_fd()
    }

    fn capture_screen(&mut self, crop: Option<Rect>, include_cursor: bool) -> Result<PixelImage> {
        if self.state.screencopy_mgr.is_some() {
            if let Ok(img) = self.capture_via_screencopy(crop, include_cursor) {
                return Ok(img);
            }
        }

        if self.state.session_kind == SessionKind::WaylandKde {
            if let Ok((w, h, stride, bytes)) = kwin_capture_workspace_pipe(include_cursor) {
                let mut full = PixelImage::new(w, h, PixelFormat::Bgrx8);
                let dst = full.as_mut_bytes();
                let rb = (w as usize) * 4;
                for row in 0..h as usize {
                    let s = row * stride as usize;
                    let d = row * rb;
                    if s + rb <= bytes.len() && d + rb <= dst.len() {
                        dst[d..d + rb].copy_from_slice(&bytes[s..s + rb]);
                    }
                }
                if let Some(r) = crop {
                    return full
                        .crop_rect(r.x.max(0) as u32, r.y.max(0) as u32, r.width, r.height)
                        .ok_or_else(|| VexrecError::Display("Crop outside KWin image bounds".into()));
                }
                return Ok(full);
            }
        }

        // Universal GNOME / Portal non-interactive fallback
        let uri = portal_screenshot(false)?;
        let path_str = uri
            .strip_prefix("file://")
            .ok_or_else(|| VexrecError::Portal("Invalid Portal file URI".into()))?;
        let img = decode_png_to_bgrx(std::path::Path::new(path_str))?;
        let _ = std::fs::remove_file(path_str);

        if let Some(r) = crop {
            return img
                .crop_rect(r.x.max(0) as u32, r.y.max(0) as u32, r.width, r.height)
                .ok_or_else(|| VexrecError::Display("Crop outside Portal image bounds".into()));
        }
        Ok(img)
    }

    fn enumerate_windows(&mut self, capture_previews: bool) -> Vec<ClientWindowInfo> {
        let mut list = match self.state.session_kind {
            SessionKind::WaylandHyprland => enumerate_hyprland_clients(),
            SessionKind::WaylandSway => enumerate_sway_clients(),
            _ => Vec::new(),
        };

        if capture_previews && !list.is_empty() && self.state.screencopy_mgr.is_some() {
            if let Ok(desktop) = self.capture_via_screencopy(None, false) {
                for win in &mut list {
                    win.preview = desktop.crop_rect(
                        win.rect.x.max(0) as u32,
                        win.rect.y.max(0) as u32,
                        win.rect.width,
                        win.rect.height,
                    );
                }
            }
        }
        list
    }

    fn capture_window(&mut self, window_id: u64) -> Result<PixelImage> {
        let windows = self.enumerate_windows(false);
        let win = windows
            .into_iter()
            .find(|w| w.id == window_id)
            .ok_or_else(|| VexrecError::Display("Wayland target window not found".into()))?;
        self.capture_screen(Some(win.rect), false)
    }

    fn show_surface(&mut self, role: WindowRole, geom: Rect) {
        let _ = self.ensure_role_surface(role, geom);
    }

    fn hide_surface(&mut self, role: WindowRole) {
        self.state.surfaces.remove(&role);
        let _ = self.conn.flush();
    }

    fn is_surface_visible(&self, role: WindowRole) -> bool {
        self.state.surfaces.get(&role).map(|s| s.visible).unwrap_or(false)
    }

    fn present_surface_bgra(
        &mut self,
        role: WindowRole,
        width: u32,
        height: u32,
        stride: u32,
        bgra: &[u8],
        dirty: Option<Rect>,
    ) -> Result<()> {
        let Some(ms) = self.state.surfaces.get(&role) else {
            return Ok(());
        };
        let wl_surface = ms.wl_surface.clone();
        if width == 0 || height == 0 {
            return Ok(());
        }

        let need = (width as usize) * 4 * (height as usize);
        if self
            .state
            .pool
            .as_ref()
            .map(|p| p.len() < need * 2)
            .unwrap_or(true)
        {
            self.state.pool = Some(
                SlotPool::new((need * 2).max(4096 * 4096), &self.state.shm)
                    .map_err(|e| VexrecError::Display(e.to_string()))?,
            );
        }

        let pool = self.state.pool.as_mut().unwrap();
        let dst_stride = width * 4;
        let fmt = if matches!(
            role,
            WindowRole::IndicatorPill | WindowRole::RecordingFrame | WindowRole::SettingsDialog
        ) {
            wl_shm::Format::Argb8888
        } else {
            wl_shm::Format::Xrgb8888
        };

        let (buf, canvas) = pool
            .create_buffer(width as i32, height as i32, dst_stride as i32, fmt)
            .map_err(|e| VexrecError::Display(format!("Wayland SHM create_buffer failed: {e}")))?;

        let row_b = dst_stride as usize;
        for row in 0..height as usize {
            let s = row * stride as usize;
            let d = row * row_b;
            if s + row_b <= bgra.len() && d + row_b <= canvas.len() {
                canvas[d..d + row_b].copy_from_slice(&bgra[s..s + row_b]);
            }
        }

        wl_surface.attach(Some(buf.wl_buffer()), 0, 0);
        if let Some(d) = dirty {
            wl_surface.damage_buffer(d.x, d.y, d.width as i32, d.height as i32);
        } else {
            wl_surface.damage_buffer(0, 0, width as i32, height as i32);
        }
        wl_surface.commit();
        let _ = self.conn.flush();
        Ok(())
    }

    fn set_cursor(&mut self, _role: WindowRole, cursor: CursorKind) {
        if let Some(ref shape_dev) = self.state.pointer_shape {
            let s = match cursor {
                CursorKind::Default => CursorShape::Default,
                CursorKind::Crosshair => CursorShape::Crosshair,
                CursorKind::Pointer => CursorShape::Pointer,
                CursorKind::Move => CursorShape::Move,
            };
            shape_dev.set_shape(self.state.last_pointer_enter_serial, s);
            let _ = self.conn.flush();
        }
    }

    fn minimize_surface(&mut self, role: WindowRole) {
        if let Some(ms) = self.state.surfaces.get(&role) {
            if let SurfaceShell::Xdg(ref win) = ms.shell {
                win.set_minimized();
                let _ = self.conn.flush();
            }
        }
    }

    fn begin_surface_move(&mut self, role: WindowRole) {
        if let (Some(ms), Some(ref seat)) = (self.state.surfaces.get(&role), &self.state.active_seat) {
            if let SurfaceShell::Xdg(ref win) = ms.shell {
                win.move_(seat, self.state.last_pointer_press_serial);
                let _ = self.conn.flush();
            }
        }
    }

    fn grab_overlay_keyboard(&mut self) -> bool {
        // LayerShell KeyboardInteractivity::Exclusive gives guaranteed focus immediately on map.
        self.state.surfaces.contains_key(&WindowRole::OverlayFullscreen)
    }

    fn ungrab_overlay_keyboard(&mut self) {}

    fn register_global_hotkeys(&mut self) {}

    fn set_clipboard_image(&mut self, img: PixelImage) -> Result<()> {
        if let (Some(ref mgr), Some(ref dev)) =
            (&self.state.data_control_mgr, &self.state.data_control_device)
        {
            self.state.clipboard_image = Some(img);
            self.state.clipboard_png_cache = None;
            let src = mgr.create_data_source(&self.qh, ());
            src.offer("image/png".to_string());
            dev.set_selection(Some(&src));
            self.state.data_source = Some(src);
            let _ = self.conn.flush();
            return Ok(());
        }

        // GNOME Mutter lacks wlr-data-control -> spawn detached wl-copy only if available
        let png = encode_png_bytes(&img, CompressionSpeed::Fast)?;
        let _ = std::process::Command::new("wl-copy")
            .args(["--type", "image/png"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&png);
                }
                Ok(())
            });
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<DisplayEvent> {
        let _ = self.event_queue.dispatch_pending(&mut self.state);
        if let Some(guard) = self.conn.prepare_read() {
            let _ = guard.read();
            let _ = self.event_queue.dispatch_pending(&mut self.state);
        }
        std::mem::take(&mut self.state.events)
    }

    fn sync_and_wait_frame(&mut self) {
        let _ = self.event_queue.roundtrip(&mut self.state);
        std::thread::sleep(std::time::Duration::from_millis(24));
    }
}

fn enumerate_hyprland_clients() -> Vec<ClientWindowInfo> {
    let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") else {
        return Vec::new();
    };
    let runtime = vexrec_core::xdg::runtime_dir();
    let sock_path = runtime.join("hypr").join(&sig).join(".socket.sock");
    let Ok(mut stream) = UnixStream::connect(&sock_path) else {
        return Vec::new();
    };
    if stream.write_all(b"j/clients").is_err() {
        return Vec::new();
    }
    let mut json_raw = String::new();
    if stream.read_to_string(&mut json_raw).is_err() {
        return Vec::new();
    }
    let Ok(vals) = serde_json::from_str::<Vec<serde_json::Value>>(&json_raw) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in vals {
        let mapped = item.get("mapped").and_then(|v| v.as_bool()).unwrap_or(false);
        let hidden = item.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
        if !mapped || hidden {
            continue;
        }
        let class = item.get("class").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if class.eq_ignore_ascii_case("vexrec") {
            continue;
        }
        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let at = item.get("at").and_then(|v| v.as_array());
        let size = item.get("size").and_then(|v| v.as_array());
        let (Some(at), Some(size)) = (at, size) else {
            continue;
        };
        let x = at.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let y = at.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let w = size.first().and_then(|v| v.as_i64()).unwrap_or(0).max(1) as u32;
        let h = size.get(1).and_then(|v| v.as_i64()).unwrap_or(0).max(1) as u32;
        if w < 50 || h < 50 {
            continue;
        }
        let addr_str = item.get("address").and_then(|v| v.as_str()).unwrap_or("0x0");
        let id = u64::from_str_radix(addr_str.trim_start_matches("0x"), 16).unwrap_or(out.len() as u64 + 1);
        out.push(ClientWindowInfo {
            id,
            title,
            app_id: class,
            rect: Rect::new(x, y, w, h),
            icon: None,
            preview: None,
        });
    }
    out
}

fn enumerate_sway_clients() -> Vec<ClientWindowInfo> {
    let Ok(sock_path) = std::env::var("SWAYSOCK") else {
        return Vec::new();
    };
    let Ok(mut stream) = UnixStream::connect(sock_path) else {
        return Vec::new();
    };
    // i3-ipc GET_TREE (type = 4, len = 0)
    let mut pkt = [0u8; 14];
    pkt[0..6].copy_from_slice(b"i3-ipc");
    pkt[10..14].copy_from_slice(&4u32.to_le_bytes());
    if stream.write_all(&pkt).is_err() {
        return Vec::new();
    }
    let mut hdr = [0u8; 14];
    if stream.read_exact(&mut hdr).is_err() {
        return Vec::new();
    }
    let len = u32::from_le_bytes([hdr[6], hdr[7], hdr[8], hdr[9]]) as usize;
    if len > 16 * 1024 * 1024 {
        return Vec::new();
    }
    let mut body = vec![0u8; len];
    if stream.read_exact(&mut body).is_err() {
        return Vec::new();
    }
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    collect_sway_nodes(&root, &mut out);
    out
}

fn collect_sway_nodes(node: &serde_json::Value, out: &mut Vec<ClientWindowInfo>) {
    let visible = node.get("visible").and_then(|v| v.as_bool()).unwrap_or(false);
    let node_type = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if (node_type == "con" || node_type == "floating_con")
        && visible
        && node.get("pid").and_then(|v| v.as_i64()).is_some()
    {
        if let Some(r) = node.get("rect") {
            let x = r.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let y = r.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let w = r.get("width").and_then(|v| v.as_i64()).unwrap_or(0).max(1) as u32;
            let h = r.get("height").and_then(|v| v.as_i64()).unwrap_or(0).max(1) as u32;
            let app_id = node
                .get("app_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if w >= 50 && h >= 50 && !app_id.eq_ignore_ascii_case("vexrec") {
                out.push(ClientWindowInfo {
                    id: node.get("id").and_then(|v| v.as_u64()).unwrap_or(out.len() as u64 + 1),
                    title: node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    app_id,
                    rect: Rect::new(x, y, w, h),
                    icon: None,
                    preview: None,
                });
            }
        }
    }

    for key in ["nodes", "floating_nodes"] {
        if let Some(arr) = node.get(key).and_then(|v| v.as_array()) {
            for child in arr {
                collect_sway_nodes(child, out);
            }
        }
    }
}
