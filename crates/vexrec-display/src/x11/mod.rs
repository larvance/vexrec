use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tracing::{debug, info};
use vexrec_core::buffer::{PixelFormat, PixelImage};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::geometry::{Rect, Size};
use vexrec_core::CompressionSpeed;
use vexrec_os::png_enc::encode_png_bytes;
use vexrec_os::shm::ShmBuffer;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::cursor::Handle as CursorHandle;
use x11rb::protocol::composite;
use x11rb::protocol::damage;
use x11rb::protocol::dri3;
use x11rb::protocol::randr;
use x11rb::protocol::shape;
use x11rb::protocol::shm;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::resource_manager::new_from_default;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

use crate::types::{
    ClientWindowInfo, CursorKind, DisplayBackend, DisplayEvent, DmaBufHandle, HotkeyAction,
    KeyAction, MonitorInfo, SessionKind, WindowRole,
};

/// Sanitizes `DISPLAY` when GNOME Wayland exports an internal unresponsive `GNOME_SETUP_DISPLAY`.
pub fn sanitize_display_env() {
    if let Ok(display) = std::env::var("DISPLAY") {
        let is_setup = std::env::var("GNOME_SETUP_DISPLAY")
            .map(|s| s == display)
            .unwrap_or(false);
        let is_wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|s| s.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
            || std::env::var_os("WAYLAND_DISPLAY").is_some();

        if (is_setup || (is_wayland && display == ":1"))
            && std::path::Path::new("/tmp/.X11-unix/X0").exists()
        {
            std::env::set_var("DISPLAY", ":0");
        }
    }
}

struct X11Atoms {
    net_client_list: u32,
    net_wm_pid: u32,
    net_wm_window_type: u32,
    net_wm_window_type_desktop: u32,
    net_wm_window_type_dock: u32,
    net_wm_window_type_utility: u32,
    net_wm_window_type_dialog: u32,
    net_wm_state: u32,
    net_wm_state_hidden: u32,
    net_wm_state_above: u32,
    net_wm_state_fullscreen: u32,
    net_wm_state_skip_taskbar: u32,
    net_wm_state_skip_pager: u32,
    net_frame_extents: u32,
    net_wm_name: u32,
    net_wm_icon: u32,
    net_active_window: u32,
    motif_wm_hints: u32,
    wm_protocols: u32,
    wm_delete_window: u32,
    clipboard: u32,
    targets: u32,
    timestamp: u32,
    incr: u32,
    image_png: u32,
    image_bmp: u32,
}

impl X11Atoms {
    fn intern_all(conn: &RustConnection<DefaultStream>) -> Result<Self> {
        let names: [&[u8]; 26] = [
            b"_NET_CLIENT_LIST",
            b"_NET_WM_PID",
            b"_NET_WM_WINDOW_TYPE",
            b"_NET_WM_WINDOW_TYPE_DESKTOP",
            b"_NET_WM_WINDOW_TYPE_DOCK",
            b"_NET_WM_WINDOW_TYPE_UTILITY",
            b"_NET_WM_WINDOW_TYPE_DIALOG",
            b"_NET_WM_STATE",
            b"_NET_WM_STATE_HIDDEN",
            b"_NET_WM_STATE_ABOVE",
            b"_NET_WM_STATE_FULLSCREEN",
            b"_NET_WM_STATE_SKIP_TASKBAR",
            b"_NET_WM_STATE_SKIP_PAGER",
            b"_NET_FRAME_EXTENTS",
            b"_NET_WM_NAME",
            b"_NET_WM_ICON",
            b"_NET_ACTIVE_WINDOW",
            b"_MOTIF_WM_HINTS",
            b"WM_PROTOCOLS",
            b"WM_DELETE_WINDOW",
            b"CLIPBOARD",
            b"TARGETS",
            b"TIMESTAMP",
            b"INCR",
            b"image/png",
            b"image/bmp",
        ];

        // Pipelined atom interning: 1 RTT total for all 26 atoms!
        let mut cookies = Vec::with_capacity(names.len());
        for name in names {
            cookies.push(
                conn.intern_atom(false, name)
                    .map_err(|e| VexrecError::Display(e.to_string()))?,
            );
        }

        let mut atoms = Vec::with_capacity(names.len());
        for c in cookies {
            atoms.push(
                c.reply()
                    .map_err(|e| VexrecError::Display(e.to_string()))?
                    .atom,
            );
        }

        Ok(Self {
            net_client_list: atoms[0],
            net_wm_pid: atoms[1],
            net_wm_window_type: atoms[2],
            net_wm_window_type_desktop: atoms[3],
            net_wm_window_type_dock: atoms[4],
            net_wm_window_type_utility: atoms[5],
            net_wm_window_type_dialog: atoms[6],
            net_wm_state: atoms[7],
            net_wm_state_hidden: atoms[8],
            net_wm_state_above: atoms[9],
            net_wm_state_fullscreen: atoms[10],
            net_wm_state_skip_taskbar: atoms[11],
            net_wm_state_skip_pager: atoms[12],
            net_frame_extents: atoms[13],
            net_wm_name: atoms[14],
            net_wm_icon: atoms[15],
            net_active_window: atoms[16],
            motif_wm_hints: atoms[17],
            wm_protocols: atoms[18],
            wm_delete_window: atoms[19],
            clipboard: atoms[20],
            targets: atoms[21],
            timestamp: atoms[22],
            incr: atoms[23],
            image_png: atoms[24],
            image_bmp: atoms[25],
        })
    }
}

struct RoleSurface {
    window: u32,
    gc: u32,
    depth: u8,
    visible: bool,
    width: u32,
    height: u32,
    shm: Option<(ShmBuffer, shm::Seg)>,
}

struct TrackedClientWindow {
    icon: Option<PixelImage>,
    damage: Option<damage::Damage>,
    redirected: bool,
}

struct IncrTransfer {
    window: u32,
    property: u32,
    target: u32,
    data: Arc<Vec<u8>>,
    offset: usize,
}

pub struct X11Display {
    conn: Arc<RustConnection<DefaultStream>>,
    root: u32,
    root_depth: u8,
    root_visual: u32,
    argb32_visual: Option<(u32, u32)>, // (visual_id, colormap_id)
    screen_w: u16,
    screen_h: u16,
    atoms: X11Atoms,
    shm_fd_supported: bool,
    has_composite: bool,
    has_damage: bool,
    damage_event_base: u8,
    has_dri3: bool,
    has_xfixes: bool,
    capture_shm: Option<(ShmBuffer, shm::Seg)>,
    surfaces: HashMap<WindowRole, RoleSurface>,
    tracked_windows: HashMap<u32, TrackedClientWindow>,
    damage_to_xid: HashMap<u32, u32>,
    cursors: HashMap<CursorKind, u32>,
    clipboard_win: u32,
    clipboard_image: Option<PixelImage>,
    clipboard_png_cache: Option<Arc<Vec<u8>>>,
    clipboard_timestamp: u32,
    incr_transfers: HashMap<(u32, u32), IncrTransfer>,
    print_keycode: Option<u8>,
    grabbed_hotkeys: bool,
    kbd_mapping: Option<(u8, u8, Vec<u32>)>, // (min_kc, syms_per_kc, keysyms)
}

impl X11Display {
    pub fn connect() -> Result<Self> {
        sanitize_display_env();
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| VexrecError::Display(format!("Failed to connect to X11 display: {e}")))?;

        {
            let screen = &conn.setup().roots[screen_num];
            let root = screen.root;
            // Verify root GetImage works (rejects rootless Wayland setup sockets that fail BadMatch).
            let probe = conn
                .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 1, 1, !0)
                .map_err(|e| VexrecError::Display(format!("X11 root probe failed: {e}")))?
                .reply()
                .map_err(|e| VexrecError::Display(format!("X11 root window unreadable (rootless Xwayland?): {e}")))?;
            if probe.data.len() < 4 {
                return Err(VexrecError::Display("Unexpected X11 root probe format".into()));
            }
        }

        let atoms = X11Atoms::intern_all(&conn)?;
        let (root, root_depth, root_visual, screen_w, screen_h, argb_vid) = {
            let screen = &conn.setup().roots[screen_num];
            let vid = find_argb32_visual(screen);
            (
                screen.root,
                screen.root_depth,
                screen.root_visual,
                screen.width_in_pixels,
                screen.height_in_pixels,
                vid,
            )
        };

        let argb32_visual = if let Some(vid) = argb_vid {
            let cmap = conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
            conn.create_colormap(xproto::ColormapAlloc::NONE, cmap, root, vid)
                .map_err(|e| VexrecError::Display(e.to_string()))?;
            Some((vid, cmap))
        } else {
            None
        };

        // Query extensions in parallel
        let shm_ck = shm::query_version(&conn).ok();
        let comp_ck = composite::query_version(&conn, 0, 4).ok();
        let dmg_ext = conn.extension_information(damage::X11_EXTENSION_NAME).ok().flatten();
        let dmg_ck = damage::query_version(&conn, 1, 1).ok();
        let dri3_ck = dri3::query_version(&conn, 1, 2).ok();
        let xfixes_ck = xfixes::query_version(&conn, 4, 0).ok();

        let _shm_rep = shm_ck.and_then(|c| c.reply().ok());
        let has_composite = comp_ck.and_then(|c| c.reply().ok()).is_some();
        let has_damage = dmg_ck.and_then(|c| c.reply().ok()).is_some() && dmg_ext.is_some();
        let damage_event_base = dmg_ext.map(|e| e.first_event).unwrap_or(0);
        let has_dri3 = dri3_ck.and_then(|c| c.reply().ok()).is_some();
        let has_xfixes = xfixes_ck.and_then(|c| c.reply().ok()).is_some();

        let shm_fd_supported = test_shm_attach_fd(&conn);

        // Load XCursor handles cleanly in pure Rust
        let mut cursors = HashMap::new();
        if let Ok(rdb) = new_from_default(&conn) {
            let screen = &conn.setup().roots[screen_num];
            if let Ok(ch_cookie) = CursorHandle::new(&conn, screen_num, &rdb) {
                if let Ok(handle) = ch_cookie.reply() {
                    let _ = screen;
                    if let Ok(c) = handle.load_cursor(&conn, "left_ptr") {
                        cursors.insert(CursorKind::Default, c);
                    }
                    if let Ok(c) = handle.load_cursor(&conn, "crosshair") {
                        cursors.insert(CursorKind::Crosshair, c);
                    }
                    if let Ok(c) = handle.load_cursor(&conn, "pointer").or_else(|_| handle.load_cursor(&conn, "hand2")) {
                        cursors.insert(CursorKind::Pointer, c);
                    }
                    if let Ok(c) = handle.load_cursor(&conn, "fleur").or_else(|_| handle.load_cursor(&conn, "move")) {
                        cursors.insert(CursorKind::Move, c);
                    }
                }
            }
        }

        // Hidden clipboard owner window
        let clipboard_win = conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            clipboard_win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| VexrecError::Display(e.to_string()))?;

        // Load initial keyboard mapping for keysym decoding
        let min_kc = conn.setup().min_keycode;
        let max_kc = conn.setup().max_keycode;
        let kbd_mapping = conn
            .get_keyboard_mapping(min_kc, max_kc.saturating_sub(min_kc) + 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|rep| (min_kc, rep.keysyms_per_keycode, rep.keysyms));

        let print_keycode = kbd_mapping.as_ref().and_then(|(min_k, per_kc, syms)| {
            let p = *per_kc as usize;
            if p == 0 {
                return None;
            }
            for (idx, chunk) in syms.chunks_exact(p).enumerate() {
                if chunk.contains(&0xff61) {
                    return Some(min_k + idx as u8);
                }
            }
            None
        });

        let mut disp = Self {
            conn: Arc::new(conn),
            root,
            root_depth,
            root_visual,
            argb32_visual,
            screen_w,
            screen_h,
            atoms,
            shm_fd_supported,
            has_composite,
            has_damage,
            damage_event_base,
            has_dri3,
            has_xfixes,
            capture_shm: None,
            surfaces: HashMap::new(),
            tracked_windows: HashMap::new(),
            damage_to_xid: HashMap::new(),
            cursors,
            clipboard_win,
            clipboard_image: None,
            clipboard_png_cache: None,
            clipboard_timestamp: 0,
            incr_transfers: HashMap::new(),
            print_keycode,
            grabbed_hotkeys: false,
            kbd_mapping,
        };

        disp.ensure_capture_shm((screen_w as u32) * (screen_h as u32) * 4)?;
        let _ = disp.conn.flush();
        info!(
            screen_w,
            screen_h,
            shm_fd_supported,
            has_composite,
            has_damage,
            has_dri3,
            "Initialized persistent bare-metal X11 display backend"
        );
        Ok(disp)
    }

    fn ensure_capture_shm(&mut self, required_bytes: u32) -> Result<()> {
        if !self.shm_fd_supported {
            return Ok(());
        }
        if let Some((ref buf, _)) = self.capture_shm {
            if buf.len() >= required_bytes as usize {
                return Ok(());
            }
        }
        if let Some((_old_buf, old_seg)) = self.capture_shm.take() {
            let _ = shm::detach(&*self.conn, old_seg);
        }
        let buf = ShmBuffer::new("vexrec-cap", required_bytes as usize)?;
        let fd = buf.try_clone_fd()?;
        let seg = self.conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
        shm::attach_fd(&*self.conn, seg, fd, false)
            .map_err(|e| VexrecError::Display(format!("SHM attach_fd failed: {e}")))?;
        self.conn.sync().map_err(|e| VexrecError::Display(e.to_string()))?;
        self.capture_shm = Some((buf, seg));
        Ok(())
    }

    fn capture_drawable_fast(&mut self, drawable: u32, x: i16, y: i16, w: u16, h: u16) -> Result<PixelImage> {
        let need_bytes = (w as u32) * (h as u32) * 4;
        if self.shm_fd_supported {
            self.ensure_capture_shm(need_bytes)?;
            if let Some((ref shm_buf, seg)) = self.capture_shm {
                if shm::get_image(
                    &*self.conn,
                    drawable,
                    x,
                    y,
                    w,
                    h,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    seg,
                    0,
                )
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some()
                {
                    let len = need_bytes as usize;
                    if let Some(img) = PixelImage::from_tight_bytes(
                        w as u32,
                        h as u32,
                        PixelFormat::Bgrx8,
                        &shm_buf.as_slice()[..len],
                    ) {
                        return Ok(img);
                    }
                }
            }
        }

        // Socket GetImage fallback
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, x, y, w, h, !0)
            .map_err(|e| VexrecError::Display(e.to_string()))?
            .reply()
            .map_err(|e| VexrecError::Display(e.to_string()))?;

        PixelImage::from_tight_bytes(w as u32, h as u32, PixelFormat::Bgrx8, &reply.data)
            .ok_or_else(|| VexrecError::Display("Unexpected GetImage byte length".into()))
    }

    fn overlay_cursor_onto_bgra(&self, img: &mut PixelImage, origin_x: i32, origin_y: i32) {
        if !self.has_xfixes {
            return;
        }
        let Ok(c) = xfixes::get_cursor_image(&*self.conn) else {
            return;
        };
        let Ok(rep) = c.reply() else {
            return;
        };
        let cx = rep.x as i32 - rep.xhot as i32 - origin_x;
        let cy = rep.y as i32 - rep.yhot as i32 - origin_y;
        let cw = rep.width as i32;
        let ch = rep.height as i32;
        let stride = img.stride as usize;
        let img_w = img.width as i32;
        let img_h = img.height as i32;
        let dst = img.as_mut_bytes();

        for row in 0..ch {
            let dy = cy + row;
            if dy < 0 || dy >= img_h {
                continue;
            }
            for col in 0..cw {
                let dx = cx + col;
                if dx < 0 || dx >= img_w {
                    continue;
                }
                let px = rep.cursor_image[(row * cw + col) as usize];
                let a = ((px >> 24) & 0xFF) as u8;
                if a == 0 {
                    continue;
                }
                let r = ((px >> 16) & 0xFF) as u8;
                let g = ((px >> 8) & 0xFF) as u8;
                let b = (px & 0xFF) as u8;
                let off = (dy as usize) * stride + (dx as usize) * 4;
                if off + 4 <= dst.len() {
                    vexrec_os::simd::blend_pixel_bgra(&mut dst[off..off + 4], b, g, r, a);
                }
            }
        }
    }

    fn ensure_role_surface(&mut self, role: WindowRole, geom: Rect) -> Result<()> {
        if let Some(existing) = self.surfaces.get(&role) {
            let win = existing.window;
            let _ = self.conn.configure_window(
                win,
                &xproto::ConfigureWindowAux::new()
                    .x(geom.x)
                    .y(geom.y)
                    .width(geom.width.max(1))
                    .height(geom.height.max(1))
                    .stack_mode(xproto::StackMode::ABOVE),
            );
            return Ok(());
        }

        let win = self.conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
        let use_argb = matches!(role, WindowRole::IndicatorPill | WindowRole::RecordingFrame)
            && self.argb32_visual.is_some();

        let (depth, visual, cmap_opt) = if use_argb {
            let (v, c) = self.argb32_visual.unwrap();
            (32u8, v, Some(c))
        } else {
            (self.root_depth, self.root_visual, None)
        };

        let event_mask = match role {
            WindowRole::RecordingFrame => xproto::EventMask::EXPOSURE,
            WindowRole::IndicatorPill => {
                xproto::EventMask::EXPOSURE
                    | xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::BUTTON_RELEASE
                    | xproto::EventMask::POINTER_MOTION
                    | xproto::EventMask::LEAVE_WINDOW
            }
            WindowRole::OverlayFullscreen => {
                xproto::EventMask::EXPOSURE
                    | xproto::EventMask::KEY_PRESS
                    | xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::BUTTON_RELEASE
                    | xproto::EventMask::POINTER_MOTION
                    | xproto::EventMask::LEAVE_WINDOW
                    | xproto::EventMask::STRUCTURE_NOTIFY
            }
            WindowRole::SettingsDialog => {
                xproto::EventMask::EXPOSURE
                    | xproto::EventMask::KEY_PRESS
                    | xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::BUTTON_RELEASE
                    | xproto::EventMask::POINTER_MOTION
                    | xproto::EventMask::STRUCTURE_NOTIFY
            }
        };

        let mut aux = xproto::CreateWindowAux::new()
            .event_mask(event_mask)
            .background_pixel(0)
            .border_pixel(0);
        if let Some(cm) = cmap_opt {
            aux = aux.colormap(cm);
        }
        if matches!(
            role,
            WindowRole::RecordingFrame | WindowRole::IndicatorPill | WindowRole::OverlayFullscreen
        ) {
            // override_redirect=1 prevents WM focus-stealing prevention, panel clamping, or lowering
            // the freeze capture overlay behind the currently focused window; keyboard is grabbed directly.
            aux = aux.override_redirect(1);
        }

        self.conn
            .create_window(
                depth,
                win,
                self.root,
                geom.x as i16,
                geom.y as i16,
                geom.width.max(1) as u16,
                geom.height.max(1) as u16,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                visual,
                &aux,
            )
            .map_err(|e| VexrecError::Display(format!("CreateWindow failed: {e}")))?;

        let gc = self.conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
        self.conn
            .create_gc(gc, win, &xproto::CreateGCAux::new())
            .map_err(|e| VexrecError::Display(e.to_string()))?;

        // Configure WM properties per role
        match role {
            WindowRole::OverlayFullscreen => {
                // Empty window title + UTILITY + ABOVE + FULLSCREEN prevents GNOME Focus Stealing Prevention
                let _ = self.conn.change_property8(
                    xproto::PropMode::REPLACE,
                    win,
                    xproto::AtomEnum::WM_NAME,
                    xproto::AtomEnum::STRING,
                    b"",
                );
                let _ = self.conn.change_property32(
                    xproto::PropMode::REPLACE,
                    win,
                    self.atoms.net_wm_window_type,
                    xproto::AtomEnum::ATOM,
                    &[self.atoms.net_wm_window_type_utility],
                );
                let _ = self.conn.change_property32(
                    xproto::PropMode::REPLACE,
                    win,
                    self.atoms.net_wm_state,
                    xproto::AtomEnum::ATOM,
                    &[
                        self.atoms.net_wm_state_above,
                        self.atoms.net_wm_state_fullscreen,
                        self.atoms.net_wm_state_skip_taskbar,
                        self.atoms.net_wm_state_skip_pager,
                    ],
                );
                // Borderless Motif hints: flags = 2, functions = 0, decorations = 0
                let _ = self.conn.change_property32(
                    xproto::PropMode::REPLACE,
                    win,
                    self.atoms.motif_wm_hints,
                    self.atoms.motif_wm_hints,
                    &[2, 0, 0, 0, 0],
                );
            }
            WindowRole::RecordingFrame => {
                // Empty input region guarantees 100% OS-level mouse click-through
                let _ = shape::rectangles(
                    &*self.conn,
                    shape::SO::SET,
                    shape::SK::INPUT,
                    xproto::ClipOrdering::UNSORTED,
                    win,
                    0,
                    0,
                    &[],
                );
            }
            WindowRole::IndicatorPill => {
                if let Some(&ptr_cur) = self.cursors.get(&CursorKind::Pointer) {
                    let _ = self.conn.change_window_attributes(
                        win,
                        &xproto::ChangeWindowAttributesAux::new().cursor(ptr_cur),
                    );
                }
            }
            WindowRole::SettingsDialog => {
                let _ = self.conn.change_property8(
                    xproto::PropMode::REPLACE,
                    win,
                    xproto::AtomEnum::WM_NAME,
                    xproto::AtomEnum::STRING,
                    b"Vexrec Preferences",
                );
                let _ = self.conn.change_property32(
                    xproto::PropMode::REPLACE,
                    win,
                    self.atoms.net_wm_window_type,
                    xproto::AtomEnum::ATOM,
                    &[self.atoms.net_wm_window_type_dialog],
                );
                let _ = self.conn.change_property32(
                    xproto::PropMode::REPLACE,
                    win,
                    self.atoms.wm_protocols,
                    xproto::AtomEnum::ATOM,
                    &[self.atoms.wm_delete_window],
                );
            }
        }

        let _ = self.conn.flush();
        self.surfaces.insert(
            role,
            RoleSurface {
                window: win,
                gc,
                depth,
                visible: false,
                width: geom.width.max(1),
                height: geom.height.max(1),
                shm: None,
            },
        );
        Ok(())
    }

    fn role_by_window(&self, win: u32) -> Option<WindowRole> {
        self.surfaces
            .iter()
            .find_map(|(r, s)| if s.window == win { Some(*r) } else { None })
    }

    fn is_own_window(&self, win: u32) -> bool {
        win == self.clipboard_win || self.surfaces.values().any(|s| s.window == win)
    }

    fn translate_keycode(&self, keycode: u8, state: u16) -> KeyAction {
        let ctrl = (state & u16::from(xproto::KeyButMask::CONTROL)) != 0;
        let shift = (state & u16::from(xproto::KeyButMask::SHIFT)) != 0;

        let keysym = if let Some((min_kc, per_kc, ref syms)) = self.kbd_mapping {
            if keycode >= min_kc && per_kc > 0 {
                let idx = (keycode - min_kc) as usize * per_kc as usize;
                let col = usize::from(shift);
                let sym = syms.get(idx + col).copied().unwrap_or(0);
                if sym == 0 {
                    syms.get(idx).copied().unwrap_or(0)
                } else {
                    sym
                }
            } else {
                0
            }
        } else {
            0
        };

        match keysym {
            0xff1b => KeyAction::Escape,
            0xff0d | 0xff8d => KeyAction::Return,
            0xff61 => KeyAction::Print,
            0xff08 => KeyAction::Backspace,
            0xff09 => KeyAction::Tab,
            0x0031 => KeyAction::Num(1),
            0x0032 => KeyAction::Num(2),
            0x0033 => KeyAction::Num(3),
            0x0034 => KeyAction::Num(4),
            0x0063 | 0x0043 if ctrl => KeyAction::CtrlC,
            0x0073 | 0x0053 if ctrl => KeyAction::CtrlS,
            k if (0x0020..=0x007e).contains(&k) => KeyAction::Char(k as u8 as char),
            _ => KeyAction::Other,
        }
    }

    fn handle_selection_request(&mut self, req: xproto::SelectionRequestEvent) {
        let prop = if req.property == x11rb::NONE {
            req.target
        } else {
            req.property
        };

        let mut notify_prop = prop;
        if req.target == self.atoms.targets {
            let targets = [
                self.atoms.timestamp,
                self.atoms.targets,
                self.atoms.image_png,
                self.atoms.image_bmp,
            ];
            let _ = self.conn.change_property32(
                xproto::PropMode::REPLACE,
                req.requestor,
                prop,
                xproto::AtomEnum::ATOM,
                &targets,
            );
        } else if req.target == self.atoms.timestamp {
            let _ = self.conn.change_property32(
                xproto::PropMode::REPLACE,
                req.requestor,
                prop,
                xproto::AtomEnum::INTEGER,
                &[self.clipboard_timestamp],
            );
        } else if req.target == self.atoms.image_png || req.target == self.atoms.image_bmp {
            if self.clipboard_png_cache.is_none() {
                if let Some(ref img) = self.clipboard_image {
                    if let Ok(encoded) = encode_png_bytes(img, CompressionSpeed::Fast) {
                        self.clipboard_png_cache = Some(Arc::new(encoded));
                    }
                }
            }

            if let Some(ref data) = self.clipboard_png_cache {
                const INCR_CHUNK: usize = 200 * 1024;
                if data.len() <= INCR_CHUNK {
                    let _ = self.conn.change_property8(
                        xproto::PropMode::REPLACE,
                        req.requestor,
                        prop,
                        req.target,
                        data.as_slice(),
                    );
                } else {
                    // ICCCM INCR protocol for > 200 KiB screenshots
                    let _ = self.conn.change_window_attributes(
                        req.requestor,
                        &xproto::ChangeWindowAttributesAux::new()
                            .event_mask(xproto::EventMask::PROPERTY_CHANGE),
                    );
                    let total_hint = data.len() as u32;
                    let _ = self.conn.change_property32(
                        xproto::PropMode::REPLACE,
                        req.requestor,
                        prop,
                        self.atoms.incr,
                        &[total_hint],
                    );
                    self.incr_transfers.insert(
                        (req.requestor, prop),
                        IncrTransfer {
                            window: req.requestor,
                            property: prop,
                            target: req.target,
                            data: data.clone(),
                            offset: 0,
                        },
                    );
                }
            } else {
                notify_prop = x11rb::NONE;
            }
        } else {
            notify_prop = x11rb::NONE;
        }

        let ev = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: notify_prop,
        };
        let _ = self.conn.send_event(false, req.requestor, xproto::EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    fn advance_incr_transfer(&mut self, win: u32, prop: u32) {
        const INCR_CHUNK: usize = 200 * 1024;
        let Some(mut tr) = self.incr_transfers.remove(&(win, prop)) else {
            return;
        };
        if tr.offset < tr.data.len() {
            let end = (tr.offset + INCR_CHUNK).min(tr.data.len());
            let _ = self.conn.change_property8(
                xproto::PropMode::REPLACE,
                tr.window,
                tr.property,
                tr.target,
                &tr.data[tr.offset..end],
            );
            tr.offset = end;
            self.incr_transfers.insert((win, prop), tr);
        } else {
            // Final zero-length write signals EOF in ICCCM INCR protocol
            let _ = self.conn.change_property8(
                xproto::PropMode::REPLACE,
                tr.window,
                tr.property,
                tr.target,
                &[],
            );
        }
        let _ = self.conn.flush();
    }
}

impl DisplayBackend for X11Display {
    fn session_kind(&self) -> SessionKind {
        SessionKind::X11
    }

    fn primary_size(&self) -> Size {
        Size::new(self.screen_w as u32, self.screen_h as u32)
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        let mut out = Vec::new();
        if let Ok(ck) = randr::get_monitors(&*self.conn, self.root, true) {
            if let Ok(rep) = ck.reply() {
                for m in rep.monitors {
                    let name = self
                        .conn
                        .get_atom_name(m.name)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|n| String::from_utf8_lossy(&n.name).into_owned())
                        .unwrap_or_else(|| "Monitor".into());
                    out.push(MonitorInfo {
                        name,
                        rect: Rect::new(m.x as i32, m.y as i32, m.width as u32, m.height as u32),
                        primary: m.primary,
                        scale_factor: 1.0,
                    });
                }
            }
        }
        if out.is_empty() {
            out.push(MonitorInfo {
                name: "Primary".into(),
                rect: Rect::new(0, 0, self.screen_w as u32, self.screen_h as u32),
                primary: true,
                scale_factor: 1.0,
            });
        }
        out
    }

    fn raw_fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }

    fn capture_screen(&mut self, crop: Option<Rect>, include_cursor: bool) -> Result<PixelImage> {
        let (x, y, w, h) = match crop {
            Some(r) => {
                let c = r.clamp_bounds(self.screen_w as u32, self.screen_h as u32);
                (c.x as i16, c.y as i16, c.width.max(1) as u16, c.height.max(1) as u16)
            }
            None => (0i16, 0i16, self.screen_w, self.screen_h),
        };
        let mut img = self.capture_drawable_fast(self.root, x, y, w, h)?;
        if include_cursor {
            self.overlay_cursor_onto_bgra(&mut img, x as i32, y as i32);
        }
        Ok(img)
    }

    fn enumerate_windows(&mut self, capture_previews: bool) -> Vec<ClientWindowInfo> {
        let Ok(list_rep) = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                xproto::AtomEnum::WINDOW,
                0,
                1024,
            )
            .map_err(|_| ())
            .and_then(|c| c.reply().map_err(|_| ()))
        else {
            return Vec::new();
        };

        let Some(iter) = list_rep.value32() else {
            return Vec::new();
        };
        let xids: Vec<u32> = iter.filter(|&w| !self.is_own_window(w)).collect();
        if xids.is_empty() {
            return Vec::new();
        }

        // Pipelined property + geometry + attribute cookies for ALL windows in 1 X11 flush!
        struct Pending<'a> {
            win: u32,
            pid: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            wtype: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            state: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            attrs: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetWindowAttributesReply>,
            geom: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetGeometryReply>,
            coords: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::TranslateCoordinatesReply>,
            extents: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            class: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            net_name: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            wm_name: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::GetPropertyReply>,
            tree: x11rb::cookie::Cookie<'a, RustConnection<DefaultStream>, xproto::QueryTreeReply>,
        }

        let conn: &RustConnection<DefaultStream> = &self.conn;
        let mut pending = Vec::with_capacity(xids.len());
        for &win in &xids {
            let Ok(pid) = conn.get_property(false, win, self.atoms.net_wm_pid, xproto::AtomEnum::CARDINAL, 0, 1) else { continue; };
            let Ok(wtype) = conn.get_property(false, win, self.atoms.net_wm_window_type, xproto::AtomEnum::ATOM, 0, 32) else { continue; };
            let Ok(state) = conn.get_property(false, win, self.atoms.net_wm_state, xproto::AtomEnum::ATOM, 0, 32) else { continue; };
            let Ok(attrs) = conn.get_window_attributes(win) else { continue; };
            let Ok(geom) = conn.get_geometry(win) else { continue; };
            let Ok(coords) = conn.translate_coordinates(win, self.root, 0, 0) else { continue; };
            let Ok(extents) = conn.get_property(false, win, self.atoms.net_frame_extents, xproto::AtomEnum::CARDINAL, 0, 4) else { continue; };
            let Ok(class) = conn.get_property(false, win, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, 0, 128) else { continue; };
            let Ok(net_name) = conn.get_property(false, win, self.atoms.net_wm_name, xproto::AtomEnum::ANY, 0, 256) else { continue; };
            let Ok(wm_name) = conn.get_property(false, win, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, 0, 256) else { continue; };
            let Ok(tree) = conn.query_tree(win) else { continue; };

            pending.push(Pending {
                win,
                pid,
                wtype,
                state,
                attrs,
                geom,
                coords,
                extents,
                class,
                net_name,
                wm_name,
                tree,
            });
        }

        let _ = self.conn.flush();
        let current_pid = std::process::id();
        let sw = self.screen_w as i32;
        let sh = self.screen_h as i32;

        // Collect metadata first without mutably borrowing `self` for SHM capture yet
        struct WindowMeta {
            win: u32,
            capture_drawable: u32,
            title: String,
            class_name: String,
            rect: Rect,
            parent_size: (u16, u16),
        }

        let mut metas = Vec::new();
        for item in pending {
            let win = item.win;
            if let Ok(pr) = item.pid.reply() {
                if pr.value32().and_then(|mut it| it.next()) == Some(current_pid) {
                    continue;
                }
            }
            if let Ok(wt) = item.wtype.reply() {
                if let Some(ts) = wt.value32() {
                    if ts.into_iter().any(|t| {
                        t == self.atoms.net_wm_window_type_desktop || t == self.atoms.net_wm_window_type_dock
                    }) {
                        continue;
                    }
                }
            }
            if let Ok(st) = item.state.reply() {
                if let Some(ss) = st.value32() {
                    if ss.into_iter().any(|s| s == self.atoms.net_wm_state_hidden) {
                        continue;
                    }
                }
            }
            let Ok(attrs) = item.attrs.reply() else { continue; };
            if attrs.map_state != xproto::MapState::VIEWABLE {
                continue;
            }
            let Ok(geom) = item.geom.reply() else { continue; };
            if geom.width < 50 || geom.height < 50 {
                continue;
            }

            let class_name = item
                .class
                .reply()
                .ok()
                .and_then(|r| {
                    r.value
                        .split(|&b| b == 0)
                        .filter(|p| !p.is_empty())
                        .last()
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                })
                .unwrap_or_default();
            if class_name.eq_ignore_ascii_case("vexrec") {
                continue;
            }

            let title = item
                .net_name
                .reply()
                .ok()
                .filter(|r| !r.value.is_empty())
                .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                .or_else(|| {
                    item.wm_name
                        .reply()
                        .ok()
                        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                })
                .unwrap_or_default();

            let (raw_x, raw_y) = item
                .coords
                .reply()
                .map(|c| (c.dst_x as i32, c.dst_y as i32))
                .unwrap_or((geom.x as i32, geom.y as i32));

            let (mut el, mut er, mut et, mut eb) = (0i32, 0i32, 0i32, 0i32);
            if let Ok(ext) = item.extents.reply() {
                if let Some(mut v) = ext.value32() {
                    el = v.next().unwrap_or(0) as i32;
                    er = v.next().unwrap_or(0) as i32;
                    et = v.next().unwrap_or(0) as i32;
                    eb = v.next().unwrap_or(0) as i32;
                }
            }

            let wx = raw_x - el;
            let wy = raw_y - et;
            let ww = geom.width as i32 + el + er;
            let wh = geom.height as i32 + et + eb;

            let fx = wx.clamp(0, (sw - 1).max(0));
            let fy = wy.clamp(0, (sh - 1).max(0));
            let fw = (ww - (fx - wx)).max(10).min((sw - fx).max(1)) as u32;
            let fh = (wh - (fy - wy)).max(10).min((sh - fy).max(1)) as u32;

            let mut target = win;
            let mut target_size = (geom.width, geom.height);
            if let Ok(tr) = item.tree.reply() {
                if tr.parent != self.root {
                    if let Ok(pg) = self.conn.get_geometry(tr.parent).and_then(|c| Ok(c.reply())) {
                        if let Ok(pg) = pg {
                            if pg.width >= geom.width && pg.height >= geom.height {
                                target = tr.parent;
                                target_size = (pg.width, pg.height);
                            }
                        }
                    }
                }
            }

            metas.push(WindowMeta {
                win,
                capture_drawable: target,
                title,
                class_name,
                rect: Rect::new(fx, fy, fw, fh),
                parent_size: target_size,
            });
        }

        // Prune tracked windows that no longer exist to avoid X server XDamage leaks
        let live_set: std::collections::HashSet<u32> = metas.iter().map(|m| m.win).collect();
        let stale: Vec<u32> = self
            .tracked_windows
            .keys()
            .copied()
            .filter(|w| !live_set.contains(w))
            .collect();
        for dead in stale {
            if let Some(tc) = self.tracked_windows.remove(&dead) {
                if let Some(dmg) = tc.damage {
                    self.damage_to_xid.remove(&dmg);
                    let _ = damage::destroy(&*self.conn, dmg);
                }
            }
        }

        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            // Attach XComposite + XDamage + PropertyNotify once per tracked window and cache its icon!
            if !self.tracked_windows.contains_key(&meta.win) {
                let icon = self.fetch_cached_icon(meta.win);
                let mut damage_opt = None;
                let mut redirected = false;
                if self.has_composite {
                    if composite::redirect_window(&*self.conn, meta.capture_drawable, composite::Redirect::AUTOMATIC).is_ok() {
                        redirected = true;
                    }
                }
                if self.has_damage {
                    if let Ok(dmg_id) = self.conn.generate_id() {
                        if damage::create(&*self.conn, dmg_id, meta.capture_drawable, damage::ReportLevel::NON_EMPTY).is_ok() {
                            self.damage_to_xid.insert(dmg_id, meta.win);
                            damage_opt = Some(dmg_id);
                        }
                    }
                }
                self.tracked_windows.insert(
                    meta.win,
                    TrackedClientWindow {
                        icon,
                        damage: damage_opt,
                        redirected,
                    },
                );
            }

            let preview = if capture_previews {
                self.capture_window_drawable(meta.win, meta.capture_drawable, meta.parent_size.0, meta.parent_size.1)
                    .ok()
            } else {
                None
            };

            let icon = self.tracked_windows.get(&meta.win).and_then(|t| t.icon.clone());
            out.push(ClientWindowInfo {
                id: meta.win as u64,
                title: meta.title,
                app_id: meta.class_name,
                rect: meta.rect,
                icon,
                preview,
            });
        }

        let _ = self.conn.flush();
        out
    }

    fn capture_window(&mut self, window_id: u64) -> Result<PixelImage> {
        let win = window_id as u32;
        let geom = self
            .conn
            .get_geometry(win)
            .map_err(|e| VexrecError::Display(e.to_string()))?
            .reply()
            .map_err(|e| VexrecError::Display(e.to_string()))?;

        let mut target = win;
        let mut tw = geom.width;
        let mut th = geom.height;
        if let Ok(tr) = self.conn.query_tree(win).and_then(|c| Ok(c.reply())) {
            if let Ok(tr) = tr {
                if tr.parent != self.root {
                    if let Ok(pg) = self.conn.get_geometry(tr.parent).and_then(|c| Ok(c.reply())) {
                        if let Ok(pg) = pg {
                            if pg.width >= geom.width && pg.height >= geom.height {
                                target = tr.parent;
                                tw = pg.width;
                                th = pg.height;
                            }
                        }
                    }
                }
            }
        }

        self.capture_window_drawable(win, target, tw, th)
            .or_else(|_| self.capture_drawable_fast(win, 0, 0, geom.width.max(1), geom.height.max(1)))
    }

    fn show_surface(&mut self, role: WindowRole, geom: Rect) {
        if self.ensure_role_surface(role, geom).is_err() {
            return;
        }
        let win = match self.surfaces.get(&role) {
            Some(s) => s.window,
            None => return,
        };

        let _ = self.conn.configure_window(
            win,
            &xproto::ConfigureWindowAux::new()
                .x(geom.x)
                .y(geom.y)
                .width(geom.width.max(1))
                .height(geom.height.max(1))
                .stack_mode(xproto::StackMode::ABOVE),
        );
        let _ = self.conn.map_window(win);
        let _ = self.conn.configure_window(
            win,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        );
        if role == WindowRole::RecordingFrame {
            let _ = shape::rectangles(
                &*self.conn,
                shape::SO::SET,
                shape::SK::INPUT,
                xproto::ClipOrdering::UNSORTED,
                win,
                0,
                0,
                &[],
            );
        }

        if role == WindowRole::SettingsDialog {
            let ev = xproto::ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: win,
                type_: self.atoms.net_active_window,
                data: xproto::ClientMessageData::from([2, x11rb::CURRENT_TIME, 0, 0, 0]),
            };
            let _ = self.conn.send_event(
                false,
                self.root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            );
        }

        if let Some(s) = self.surfaces.get_mut(&role) {
            s.visible = true;
            s.width = geom.width.max(1);
            s.height = geom.height.max(1);
        }
        let _ = self.conn.flush();
    }

    fn hide_surface(&mut self, role: WindowRole) {
        if role == WindowRole::OverlayFullscreen {
            self.ungrab_overlay_keyboard();
        }
        if let Some(s) = self.surfaces.get_mut(&role) {
            if s.visible {
                let _ = self.conn.unmap_window(s.window);
                let _ = self.conn.flush();
                s.visible = false;
            }
        }
    }

    fn is_surface_visible(&self, role: WindowRole) -> bool {
        self.surfaces.get(&role).map(|s| s.visible).unwrap_or(false)
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
        let Some(surf) = self.surfaces.get_mut(&role) else {
            return Ok(());
        };
        if width == 0 || height == 0 {
            return Ok(());
        }
        let needed = (width as usize) * (height as usize) * 4;
        let dmg = dirty
            .map(|r| r.clamp_bounds(width, height))
            .unwrap_or_else(|| Rect::new(0, 0, width, height));
        if dmg.is_empty() {
            return Ok(());
        }

        if self.shm_fd_supported {
            let need_realloc = surf
                .shm
                .as_ref()
                .map(|(b, _)| b.len() < needed)
                .unwrap_or(true);

            if need_realloc {
                if let Some((_, old_seg)) = surf.shm.take() {
                    let _ = shm::detach(&*self.conn, old_seg);
                }
                let buf = ShmBuffer::new("vexrec-surf", needed.max(4096))?;
                let fd = buf.try_clone_fd()?;
                let seg = self.conn.generate_id().map_err(|e| VexrecError::Display(e.to_string()))?;
                shm::attach_fd(&*self.conn, seg, fd, true)
                    .map_err(|e| VexrecError::Display(e.to_string()))?;
                let _ = self.conn.sync();
                surf.shm = Some((buf, seg));
            }

            if let Some((ref mut shm_buf, seg)) = surf.shm {
                let dst = shm_buf.as_mut_slice();
                let dst_stride = (width as usize) * 4;
                let src_stride = stride as usize;
                let rx = dmg.x.max(0) as usize;
                let ry = dmg.y.max(0) as usize;
                let rw = dmg.width as usize;
                let rh = dmg.height as usize;

                for row in ry..(ry + rh).min(height as usize) {
                    let s_off = row * src_stride + rx * 4;
                    let d_off = row * dst_stride + rx * 4;
                    let row_b = rw * 4;
                    if s_off + row_b <= bgra.len() && d_off + row_b <= dst.len() {
                        dst[d_off..d_off + row_b].copy_from_slice(&bgra[s_off..s_off + row_b]);
                    }
                }

                shm::put_image(
                    &*self.conn,
                    surf.window,
                    surf.gc,
                    width as u16,
                    height as u16,
                    rx as u16,
                    ry as u16,
                    dmg.width as u16,
                    dmg.height as u16,
                    dmg.x as i16,
                    dmg.y as i16,
                    surf.depth,
                    ImageFormat::Z_PIXMAP.into(),
                    false,
                    seg,
                    0,
                )
                .map_err(|e| VexrecError::Display(e.to_string()))?;
                let _ = self.conn.flush();
                return Ok(());
            }
        }

        // Fallback unshared PutImage
        let packed = if stride == width * 4 {
            std::borrow::Cow::Borrowed(&bgra[..needed.min(bgra.len())])
        } else {
            let row_b = (width as usize) * 4;
            let mut p = vec![0u8; row_b * height as usize];
            for row in 0..height as usize {
                let s = row * stride as usize;
                let d = row * row_b;
                if s + row_b <= bgra.len() {
                    p[d..d + row_b].copy_from_slice(&bgra[s..s + row_b]);
                }
            }
            std::borrow::Cow::Owned(p)
        };
        let _ = self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            surf.window,
            surf.gc,
            width as u16,
            height as u16,
            0,
            0,
            0,
            surf.depth,
            &packed,
        );
        let _ = self.conn.flush();
        Ok(())
    }

    fn set_cursor(&mut self, role: WindowRole, cursor: CursorKind) {
        let Some(surf) = self.surfaces.get(&role) else {
            return;
        };
        let cur_id = self
            .cursors
            .get(&cursor)
            .copied()
            .or_else(|| self.cursors.get(&CursorKind::Default).copied())
            .unwrap_or(x11rb::NONE);
        let _ = self.conn.change_window_attributes(
            surf.window,
            &xproto::ChangeWindowAttributesAux::new().cursor(cur_id),
        );
        let _ = self.conn.flush();
    }

    fn grab_overlay_keyboard(&mut self) -> bool {
        let Some(surf) = self.surfaces.get(&WindowRole::OverlayFullscreen) else {
            return false;
        };
        let win = surf.window;

        let _ = self.conn.configure_window(
            win,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        );
        let _ = self.conn.flush();

        for attempt in 0..20 {
            if let Ok(ck) = self.conn.grab_keyboard(
                false,
                win,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            ) {
                if let Ok(rep) = ck.reply() {
                    if rep.status == xproto::GrabStatus::SUCCESS {
                        debug!(attempt, "Acquired immediate X11 overlay keyboard grab");
                        return true;
                    }
                }
            }
            if attempt < 19 {
                std::thread::sleep(std::time::Duration::from_millis(6));
            }
        }
        false
    }

    fn ungrab_overlay_keyboard(&mut self) {
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    fn register_global_hotkeys(&mut self) {
        if self.grabbed_hotkeys {
            return;
        }
        if let Some(kc) = self.print_keycode {
            let lock_masks = [
                0u16,
                u16::from(xproto::ModMask::LOCK),
                u16::from(xproto::ModMask::M2),
                u16::from(xproto::ModMask::LOCK | xproto::ModMask::M2),
            ];
            let mut ok_count = 0usize;
            for lock in lock_masks {
                // PrintScreen (0) + Shift+PrintScreen (1)
                for base_mod in [0u16, u16::from(xproto::ModMask::SHIFT)] {
                    if self
                        .conn
                        .grab_key(
                            false,
                            self.root,
                            (base_mod | lock).into(),
                            kc,
                            xproto::GrabMode::ASYNC,
                            xproto::GrabMode::ASYNC,
                        )
                        .is_ok()
                    {
                        ok_count += 1;
                    }
                }
            }
            let _ = self.conn.flush();
            self.grabbed_hotkeys = ok_count > 0;
            debug!(ok_count, "Registered root XGrabKey listeners (Print / Shift+Print)");
        }
    }

    fn set_clipboard_image(&mut self, img: PixelImage) -> Result<()> {
        self.clipboard_image = Some(img);
        self.clipboard_png_cache = None;
        self.incr_transfers.clear();
        let _ = self
            .conn
            .set_selection_owner(self.clipboard_win, self.atoms.clipboard, x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<DisplayEvent> {
        let mut events = Vec::new();
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                Event::Expose(e) => {
                    if let Some(role) = self.role_by_window(e.window) {
                        if let Some(s) = self.surfaces.get(&role) {
                            events.push(DisplayEvent::Expose {
                                role,
                                width: s.width,
                                height: s.height,
                            });
                        }
                    }
                }
                Event::ConfigureNotify(e) => {
                    if let Some(role) = self.role_by_window(e.window) {
                        if let Some(s) = self.surfaces.get_mut(&role) {
                            s.width = e.width.max(1) as u32;
                            s.height = e.height.max(1) as u32;
                        }
                        events.push(DisplayEvent::SurfaceConfigured {
                            role,
                            width: e.width.max(1) as u32,
                            height: e.height.max(1) as u32,
                            scale_factor: 1.0,
                        });
                    }
                }
                Event::MotionNotify(e) => {
                    if let Some(role) = self.role_by_window(e.event) {
                        events.push(DisplayEvent::PointerMotion {
                            role,
                            x: e.event_x as f64,
                            y: e.event_y as f64,
                        });
                    }
                }
                Event::ButtonPress(e) => {
                    if let Some(role) = self.role_by_window(e.event) {
                        events.push(DisplayEvent::PointerButton {
                            role,
                            button: e.detail as u32,
                            pressed: true,
                            x: e.event_x as f64,
                            y: e.event_y as f64,
                        });
                    }
                }
                Event::ButtonRelease(e) => {
                    if let Some(role) = self.role_by_window(e.event) {
                        events.push(DisplayEvent::PointerButton {
                            role,
                            button: e.detail as u32,
                            pressed: false,
                            x: e.event_x as f64,
                            y: e.event_y as f64,
                        });
                    }
                }
                Event::LeaveNotify(e) => {
                    if let Some(role) = self.role_by_window(e.event) {
                        events.push(DisplayEvent::PointerLeave { role });
                    }
                }
                Event::KeyPress(e) => {
                    if e.event == self.root {
                        let shift = (u16::from(e.state) & u16::from(xproto::ModMask::SHIFT)) != 0;
                        if Some(e.detail) == self.print_keycode {
                            events.push(DisplayEvent::GlobalHotkey(if shift {
                                HotkeyAction::StopRecording
                            } else {
                                HotkeyAction::Freeze
                            }));
                        }
                    } else if let Some(role) = self.role_by_window(e.event) {
                        let key = self.translate_keycode(e.detail, u16::from(e.state));
                        events.push(DisplayEvent::KeyPress { role, key });
                    }
                }
                Event::ClientMessage(e) => {
                    if e.type_ == self.atoms.wm_protocols && e.data.as_data32()[0] == self.atoms.wm_delete_window {
                        if let Some(role) = self.role_by_window(e.window) {
                            events.push(DisplayEvent::CloseRequested { role });
                        }
                    }
                }
                Event::SelectionRequest(req) => {
                    self.clipboard_timestamp = req.time;
                    self.handle_selection_request(req);
                }
                Event::SelectionClear(_) => {
                    self.clipboard_image = None;
                    self.clipboard_png_cache = None;
                    self.incr_transfers.clear();
                }
                Event::PropertyNotify(e) => {
                    if e.state == xproto::Property::DELETE {
                        self.advance_incr_transfer(e.window, e.atom);
                    }
                }
                Event::DamageNotify(d) => {
                    let _ = damage::subtract(&*self.conn, d.damage, x11rb::NONE, x11rb::NONE);
                    if let Some(&xid) = self.damage_to_xid.get(&d.damage) {
                        events.push(DisplayEvent::WindowDamaged { window_id: xid as u64 });
                    }
                }
                _ => {
                    let _ = self.damage_event_base;
                }
            }
        }
        events
    }

    fn sync_and_wait_frame(&mut self) {
        let _ = self.conn.sync();
        // Yield briefly (~1 vblank / 25ms) so Xorg/Mutter compositors finish unmapping previous surfaces
        std::thread::sleep(std::time::Duration::from_millis(28));
        let _ = self.conn.sync();
    }

    fn try_export_dmabuf(&mut self, window_id: Option<u64>) -> Option<DmaBufHandle> {
        if !self.has_dri3 || !self.has_composite {
            return None;
        }
        let target = window_id.map(|id| id as u32).unwrap_or(self.root);
        let pixmap = self.conn.generate_id().ok()?;
        composite::name_window_pixmap(&*self.conn, target, pixmap).ok()?;
        let rep = dri3::buffers_from_pixmap(&*self.conn, pixmap)
            .ok()?
            .reply()
            .ok()?;
        let _ = self.conn.free_pixmap(pixmap);
        let fd = rep.buffers.into_iter().next()?;
        Some(DmaBufHandle {
            fd,
            fourcc: 0x34325258, // DRM_FORMAT_XRGB8888
            modifier: rep.modifier,
            width: rep.width as u32,
            height: rep.height as u32,
            stride: *rep.strides.first().unwrap_or(&(rep.width as u32 * 4)),
            offset: *rep.offsets.first().unwrap_or(&0),
        })
    }
}

impl X11Display {
    fn capture_window_drawable(
        &mut self,
        win: u32,
        drawable: u32,
        w: u16,
        h: u16,
    ) -> Result<PixelImage> {
        // Redirected XComposite NameWindowPixmap extracts 100% unobscured window content even under overlap!
        if self.has_composite {
            let tracked = self.tracked_windows.get(&win).map(|t| t.redirected).unwrap_or(false);
            if !tracked {
                let _ = composite::redirect_window(&*self.conn, drawable, composite::Redirect::AUTOMATIC);
            }
            if let Ok(pixmap) = self.conn.generate_id() {
                if composite::name_window_pixmap(&*self.conn, drawable, pixmap).is_ok() {
                    let res = self.capture_drawable_fast(pixmap, 0, 0, w.max(1), h.max(1));
                    let _ = self.conn.free_pixmap(pixmap);
                    if res.is_ok() {
                        return res;
                    }
                }
            }
        }
        self.capture_drawable_fast(drawable, 0, 0, w.max(1), h.max(1))
    }

    fn fetch_cached_icon(&self, win: u32) -> Option<PixelImage> {
        let rep = self
            .conn
            .get_property(false, win, self.atoms.net_wm_icon, xproto::AtomEnum::CARDINAL, 0, 131_072)
            .ok()?
            .reply()
            .ok()?;
        let all_vals: Vec<u32> = rep.value32()?.collect();
        let mut idx = 0usize;
        let mut best: Option<(u32, u32, usize)> = None;
        let mut best_diff = i32::MAX;

        while idx + 2 <= all_vals.len() {
            let iw = all_vals[idx];
            let ih = all_vals[idx + 1];
            let count = (iw as usize).saturating_mul(ih as usize);
            if iw == 0 || ih == 0 || idx + 2 + count > all_vals.len() {
                break;
            }
            let diff = (iw as i32 - 42).abs();
            if diff < best_diff {
                best_diff = diff;
                best = Some((iw, ih, idx + 2));
            }
            idx += 2 + count;
        }

        let (iw, ih, start) = best?;
        let mut img = PixelImage::new(iw, ih, PixelFormat::Bgra8);
        let dst = img.as_mut_bytes();
        for (i, &p) in all_vals[start..start + (iw * ih) as usize].iter().enumerate() {
            let o = i * 4;
            dst[o] = (p & 0xFF) as u8;
            dst[o + 1] = ((p >> 8) & 0xFF) as u8;
            dst[o + 2] = ((p >> 16) & 0xFF) as u8;
            dst[o + 3] = ((p >> 24) & 0xFF) as u8;
        }
        Some(img)
    }
}

fn test_shm_attach_fd(conn: &RustConnection<DefaultStream>) -> bool {
    let Ok(test_buf) = ShmBuffer::new("vexrec-shm-probe", 4096) else {
        return false;
    };
    let Ok(fd) = test_buf.try_clone_fd() else {
        return false;
    };
    let Ok(seg) = conn.generate_id() else {
        return false;
    };
    if shm::attach_fd(conn, seg, fd, false).is_err() {
        return false;
    }
    let ok = conn.sync().is_ok();
    let _ = shm::detach(conn, seg);
    ok
}

fn find_argb32_visual(screen: &xproto::Screen) -> Option<u32> {
    for depth in &screen.allowed_depths {
        if depth.depth == 32 {
            for vis in &depth.visuals {
                if vis.class == xproto::VisualClass::TRUE_COLOR
                    && vis.red_mask == 0x00ff_0000
                    && vis.green_mask == 0x0000_ff00
                    && vis.blue_mask == 0x0000_00ff
                {
                    return Some(vis.visual_id);
                }
            }
        }
    }
    None
}
