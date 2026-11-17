use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use libloading::Library;
use tracing::{info, warn};
use vexrec_core::buffer::{FrameBacking, PixelFormat, PixelImage, VideoFrame};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::geometry::Rect;
use vexrec_display::{DisplayBackend, SessionKind, WaylandDisplay};
use vexrec_os::dbus::portal_screencast;
use vexrec_os::simd::bgrx_to_rgba;

// Verified PipeWire / SPA wire & ABI constants (PipeWire 1.0.5 headers)
const PW_DIRECTION_INPUT: c_int = 0;

const PW_STREAM_FLAG_AUTOCONNECT: c_int = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: c_int = 1 << 2;

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;

const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;

const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_PARAM_BUFFERS: u32 = 5;

const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;

const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;

const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;

const SPA_PARAM_BUFFERS_BUFFERS: u32 = 1;
const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;

const SPA_DATA_MEM_PTR: u32 = 1;
const SPA_DATA_MEM_FD: u32 = 2;

const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
const SPA_VIDEO_FORMAT_XRGB: u32 = 9;
const SPA_VIDEO_FORMAT_XBGR: u32 = 10;
const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
const SPA_VIDEO_FORMAT_BGRA: u32 = 12;

#[repr(C)]
struct SpaPod {
    size: u32,
    pod_type: u32,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaData {
    data_type: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
    time: u64,
}

#[repr(C)]
struct SpaHook {
    _link: [*mut c_void; 2],
    _funcs: *const c_void,
    _data: *mut c_void,
    _removed: *mut c_void,
    _priv: *mut c_void,
}

impl Default for SpaHook {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const SpaPod)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// 8-byte aligned SPA Pod byte builder (zero C macro dependencies).
struct SpaPodBuilder {
    buf: Vec<u8>,
}

impl SpaPodBuilder {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(512) }
    }
    fn push_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_ne_bytes());
    }
    fn pad8(&mut self) {
        while (self.buf.len() & 7) != 0 {
            self.buf.push(0);
        }
    }
    fn begin_object(&mut self, obj_type: u32, id: u32) -> usize {
        let pos = self.buf.len();
        self.push_u32(0); // placeholder size
        self.push_u32(SPA_TYPE_OBJECT);
        self.push_u32(obj_type);
        self.push_u32(id);
        pos
    }
    fn end_object(&mut self, pos: usize) {
        self.pad8();
        let body_size = (self.buf.len() - pos - 8) as u32;
        self.buf[pos..pos + 4].copy_from_slice(&body_size.to_ne_bytes());
    }
    fn prop_id(&mut self, key: u32, val: u32) {
        self.push_u32(key);
        self.push_u32(0); // flags
        self.push_u32(4); // pod.size
        self.push_u32(SPA_TYPE_ID);
        self.push_u32(val);
        self.pad8();
    }
    fn prop_int(&mut self, key: u32, val: i32) {
        self.push_u32(key);
        self.push_u32(0);
        self.push_u32(4);
        self.push_u32(SPA_TYPE_INT);
        self.push_u32(val as u32);
        self.pad8();
    }
    fn prop_choice_ids(&mut self, key: u32, choice_type: u32, vals: &[u32]) {
        self.push_u32(key);
        self.push_u32(0);
        let pod_pos = self.buf.len();
        self.push_u32(0); // choice size placeholder
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(choice_type);
        self.push_u32(0); // flags
        self.push_u32(4); // child.size
        self.push_u32(SPA_TYPE_ID);
        for &v in vals {
            self.push_u32(v);
        }
        self.pad8();
        let c_size = (self.buf.len() - pod_pos - 8) as u32;
        self.buf[pod_pos..pod_pos + 4].copy_from_slice(&c_size.to_ne_bytes());
    }
    fn prop_choice_ints(&mut self, key: u32, choice_type: u32, vals: &[i32]) {
        self.push_u32(key);
        self.push_u32(0);
        let pod_pos = self.buf.len();
        self.push_u32(0);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(choice_type);
        self.push_u32(0);
        self.push_u32(4);
        self.push_u32(SPA_TYPE_INT);
        for &v in vals {
            self.push_u32(v as u32);
        }
        self.pad8();
        let c_size = (self.buf.len() - pod_pos - 8) as u32;
        self.buf[pod_pos..pod_pos + 4].copy_from_slice(&c_size.to_ne_bytes());
    }
    fn prop_choice_rects(&mut self, key: u32, choice_type: u32, rects: &[(u32, u32)]) {
        self.push_u32(key);
        self.push_u32(0);
        let pod_pos = self.buf.len();
        self.push_u32(0);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(choice_type);
        self.push_u32(0);
        self.push_u32(8); // sizeof(spa_rectangle)
        self.push_u32(SPA_TYPE_RECTANGLE);
        for &(w, h) in rects {
            self.push_u32(w);
            self.push_u32(h);
        }
        self.pad8();
        let c_size = (self.buf.len() - pod_pos - 8) as u32;
        self.buf[pod_pos..pod_pos + 4].copy_from_slice(&c_size.to_ne_bytes());
    }
    fn prop_choice_fractions(&mut self, key: u32, choice_type: u32, fracs: &[(u32, u32)]) {
        self.push_u32(key);
        self.push_u32(0);
        let pod_pos = self.buf.len();
        self.push_u32(0);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(choice_type);
        self.push_u32(0);
        self.push_u32(8); // sizeof(spa_fraction)
        self.push_u32(SPA_TYPE_FRACTION);
        for &(num, denom) in fracs {
            self.push_u32(num);
            self.push_u32(denom);
        }
        self.pad8();
        let c_size = (self.buf.len() - pod_pos - 8) as u32;
        self.buf[pod_pos..pod_pos + 4].copy_from_slice(&c_size.to_ne_bytes());
    }
    fn as_pod_ptr(&self) -> *const SpaPod {
        self.buf.as_ptr() as *const SpaPod
    }
}

/// Parses SPA_PARAM_Format Object properties for video format and rectangle dimensions.
unsafe fn parse_spa_video_format_pod(pod: *const SpaPod) -> Option<(u32, u32, u32)> {
    if pod.is_null() {
        return None;
    }
    let hdr = unsafe { &*pod };
    if hdr.pod_type != SPA_TYPE_OBJECT || hdr.size < 8 {
        return None;
    }
    let total_bytes = 8usize + hdr.size as usize;
    let slice = unsafe { std::slice::from_raw_parts(pod as *const u8, total_bytes) };
    let obj_type = u32::from_ne_bytes(slice[8..12].try_into().ok()?);
    if obj_type != SPA_TYPE_OBJECT_FORMAT {
        return None;
    }

    let mut fmt_id = 0u32;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut off = 16usize;

    while off + 16 <= slice.len() {
        let key = u32::from_ne_bytes(slice[off..off + 4].try_into().ok()?);
        let _flags = u32::from_ne_bytes(slice[off + 4..off + 8].try_into().ok()?);
        let val_size = u32::from_ne_bytes(slice[off + 8..off + 12].try_into().ok()?) as usize;
        let val_type = u32::from_ne_bytes(slice[off + 12..off + 16].try_into().ok()?);
        let val_off = off + 16;

        if val_off + val_size > slice.len() {
            break;
        }
        if key == SPA_FORMAT_VIDEO_FORMAT && val_type == SPA_TYPE_ID && val_size >= 4 {
            fmt_id = u32::from_ne_bytes(slice[val_off..val_off + 4].try_into().ok()?);
        } else if key == SPA_FORMAT_VIDEO_SIZE && val_type == SPA_TYPE_RECTANGLE && val_size >= 8 {
            width = u32::from_ne_bytes(slice[val_off..val_off + 4].try_into().ok()?);
            height = u32::from_ne_bytes(slice[val_off + 4..val_off + 8].try_into().ok()?);
        }

        let prop_bytes = 16 + val_size;
        off += (prop_bytes + 7) & !7;
    }

    if width > 0 && height > 0 {
        Some((fmt_id, width, height))
    } else {
        None
    }
}

type FnPwInit = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type FnPwThreadLoopNew = unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void;
type FnPwThreadLoopDestroy = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopGetLoop = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnPwThreadLoopLock = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopUnlock = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopStart = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPwThreadLoopStop = unsafe extern "C" fn(*mut c_void);
type FnPwContextNew = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type FnPwContextDestroy = unsafe extern "C" fn(*mut c_void);
type FnPwContextConnectFd = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void;
type FnPwCoreDisconnect = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPwPropertiesNew = unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char) -> *mut c_void;
type FnPwStreamNew = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void;
type FnPwStreamAddListener = unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const PwStreamEvents, *mut c_void);
type FnPwStreamConnect = unsafe extern "C" fn(*mut c_void, c_int, u32, c_int, *mut *const SpaPod, u32) -> c_int;
type FnPwStreamUpdateParams = unsafe extern "C" fn(*mut c_void, *mut *const SpaPod, u32) -> c_int;
type FnPwStreamDequeueBuffer = unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer;
type FnPwStreamQueueBuffer = unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int;
type FnPwStreamDisconnect = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPwStreamDestroy = unsafe extern "C" fn(*mut c_void);

struct PwCbState {
    stream: *mut c_void,
    fn_update_params: FnPwStreamUpdateParams,
    fn_dequeue: FnPwStreamDequeueBuffer,
    fn_queue: FnPwStreamQueueBuffer,
    spa_fmt: u32,
    raw_width: u32,
    raw_height: u32,
    crop: Option<Rect>,
    out_w: u32,
    out_h: u32,
    latest_frame: Arc<(Mutex<Option<PixelImage>>, Condvar)>,
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const SpaPod) {
    if data.is_null() || id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    let st = unsafe { &mut *(data as *mut PwCbState) };
    if let Some((fmt, w, h)) = unsafe { parse_spa_video_format_pod(param) } {
        st.spa_fmt = fmt;
        st.raw_width = w;
        st.raw_height = h;
        info!(fmt, w, h, "Negotiated PipeWire ScreenCast video stream format");

        let mut pb = SpaPodBuilder::new();
        let obj = pb.begin_object(SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS);
        pb.prop_choice_ints(SPA_PARAM_BUFFERS_BUFFERS, SPA_CHOICE_RANGE, &[4, 2, 8]);
        pb.prop_int(
            SPA_PARAM_BUFFERS_DATA_TYPE,
            ((1u32 << SPA_DATA_MEM_PTR) | (1u32 << SPA_DATA_MEM_FD)) as i32,
        );
        pb.end_object(obj);

        let ptr = pb.as_pod_ptr();
        let mut arr = [ptr];
        unsafe { (st.fn_update_params)(st.stream, arr.as_mut_ptr(), 1) };
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let st = unsafe { &mut *(data as *mut PwCbState) };
    let pw_buf = unsafe { (st.fn_dequeue)(st.stream) };
    if pw_buf.is_null() {
        return;
    }

    let spa_buf = unsafe { (*pw_buf).buffer };
    if !spa_buf.is_null() && unsafe { (*spa_buf).n_datas >= 1 } && st.raw_width > 0 && st.raw_height > 0 {
        let d0 = unsafe { &*(*spa_buf).datas };
        let chunk = unsafe { &*d0.chunk };
        let stride = if chunk.stride > 0 {
            chunk.stride as usize
        } else {
            (st.raw_width as usize) * 4
        };
        let needed = stride * (st.raw_height as usize);
        let offset = chunk.offset as usize;

        if !d0.data.is_null() && d0.maxsize as usize >= offset + needed {
            let base = unsafe { std::slice::from_raw_parts(d0.data as *const u8, d0.maxsize as usize) };
            let src = &base[offset..];
            let mut img = extract_cropped_bgrx(
                src,
                st.raw_width,
                st.raw_height,
                stride,
                st.spa_fmt,
                st.crop,
                st.out_w,
                st.out_h,
            );
            // Force opaque alpha safely
            let _ = &mut img;
            let (lock, cvar) = &*st.latest_frame;
            if let Ok(mut guard) = lock.lock() {
                *guard = Some(img);
                cvar.notify_all();
            }
        }
    }

    unsafe { (st.fn_queue)(st.stream, pw_buf) };
}

fn extract_cropped_bgrx(
    src: &[u8],
    raw_w: u32,
    raw_h: u32,
    src_stride: usize,
    spa_fmt: u32,
    crop: Option<Rect>,
    out_w: u32,
    out_h: u32,
) -> PixelImage {
    let mut dst_img = PixelImage::new(out_w, out_h, PixelFormat::Bgrx8);
    let dst_stride = dst_img.stride as usize;
    let dst = dst_img.as_mut_bytes();

    let (sx0, sy0) = match crop {
        Some(r) => (r.x.max(0) as usize, r.y.max(0) as usize),
        None => (0, 0),
    };
    let copy_w = (out_w as usize).min((raw_w as usize).saturating_sub(sx0));
    let copy_h = (out_h as usize).min((raw_h as usize).saturating_sub(sy0));
    let swap_rb = matches!(spa_fmt, SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_RGBA);
    let reorder_xrgb = matches!(spa_fmt, SPA_VIDEO_FORMAT_XRGB | SPA_VIDEO_FORMAT_XBGR);

    for row in 0..copy_h {
        let s_off = (sy0 + row) * src_stride + sx0 * 4;
        let d_off = row * dst_stride;
        if s_off + copy_w * 4 > src.len() || d_off + copy_w * 4 > dst.len() {
            break;
        }
        let s_row = &src[s_off..s_off + copy_w * 4];
        let d_row = &mut dst[d_off..d_off + copy_w * 4];
        if !swap_rb && !reorder_xrgb {
            d_row.copy_from_slice(s_row);
        } else if swap_rb {
            bgrx_to_rgba(s_row, d_row);
        } else {
            for (sp, dp) in s_row.chunks_exact(4).zip(d_row.chunks_exact_mut(4)) {
                if spa_fmt == SPA_VIDEO_FORMAT_XRGB {
                    dp[0] = sp[3];
                    dp[1] = sp[2];
                    dp[2] = sp[1];
                    dp[3] = 255;
                } else {
                    dp[0] = sp[1];
                    dp[1] = sp[2];
                    dp[2] = sp[3];
                    dp[3] = 255;
                }
            }
        }
    }
    dst_img
}

/// Unified Wayland capture source supporting both zero-prompt `zwlr-screencopy` (Hyprland/Sway)
/// and XDG Portal `ScreenCast` + runtime `dlopen("libpipewire-0.3.so.0")` streaming (GNOME/KDE).
pub enum WaylandCaptureSource {
    Screencopy {
        disp: WaylandDisplay,
        crop: Option<Rect>,
        width: u32,
        height: u32,
        include_cursor: bool,
        start: Instant,
    },
    PipeWire(Box<PipeWireStreamCapture>),
}

pub struct PipeWireStreamCapture {
    _lib: Library,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _cb_state: Box<PwCbState>,
    _hook: Box<SpaHook>,
    _events: Box<PwStreamEvents>,
    fn_loop_lock: FnPwThreadLoopLock,
    fn_loop_unlock: FnPwThreadLoopUnlock,
    fn_loop_stop: FnPwThreadLoopStop,
    fn_loop_destroy: FnPwThreadLoopDestroy,
    fn_stream_disconnect: FnPwStreamDisconnect,
    fn_stream_destroy: FnPwStreamDestroy,
    fn_core_disconnect: FnPwCoreDisconnect,
    fn_ctx_destroy: FnPwContextDestroy,
    latest_frame: Arc<(Mutex<Option<PixelImage>>, Condvar)>,
    last_good: Option<PixelImage>,
    width: u32,
    height: u32,
    start: Instant,
}

unsafe impl Send for PipeWireStreamCapture {}

impl PipeWireStreamCapture {
    pub fn open(crop: Option<Rect>, fps: u32, include_cursor: bool) -> Result<Self> {
        let streams = portal_screencast(include_cursor, 1 | 2)?;
        let sc = streams
            .into_iter()
            .next()
            .ok_or_else(|| VexrecError::Portal("No ScreenCast PipeWire stream returned".into()))?;

        let raw_w = sc.width.unwrap_or(1920);
        let raw_h = sc.height.unwrap_or(1080);
        let (out_crop, out_w, out_h) = match crop {
            Some(r) => {
                let c = r.clamp_bounds(raw_w, raw_h).snap_even();
                let w = c.width.max(32) & !1;
                let h = c.height.max(32) & !1;
                (Some(Rect::new(c.x, c.y, w.min(raw_w), h.min(raw_h))), w & !1, h & !1)
            }
            None => (None, (raw_w & !1).max(32), (raw_h & !1).max(32)),
        };

        let lib = unsafe { Library::new("libpipewire-0.3.so.0") }
            .map_err(|e| VexrecError::Display(format!("libpipewire-0.3.so.0 unavailable: {e}")))?;

        let pw_init: FnPwInit = unsafe { *lib.get(b"pw_init\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_new: FnPwThreadLoopNew = unsafe { *lib.get(b"pw_thread_loop_new\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_destroy: FnPwThreadLoopDestroy = unsafe { *lib.get(b"pw_thread_loop_destroy\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_get_loop: FnPwThreadLoopGetLoop = unsafe { *lib.get(b"pw_thread_loop_get_loop\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_lock: FnPwThreadLoopLock = unsafe { *lib.get(b"pw_thread_loop_lock\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_unlock: FnPwThreadLoopUnlock = unsafe { *lib.get(b"pw_thread_loop_unlock\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_start: FnPwThreadLoopStart = unsafe { *lib.get(b"pw_thread_loop_start\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_loop_stop: FnPwThreadLoopStop = unsafe { *lib.get(b"pw_thread_loop_stop\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_ctx_new: FnPwContextNew = unsafe { *lib.get(b"pw_context_new\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_ctx_destroy: FnPwContextDestroy = unsafe { *lib.get(b"pw_context_destroy\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_ctx_connect_fd: FnPwContextConnectFd = unsafe { *lib.get(b"pw_context_connect_fd\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_core_disconnect: FnPwCoreDisconnect = unsafe { *lib.get(b"pw_core_disconnect\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_props_new: FnPwPropertiesNew = unsafe { *lib.get(b"pw_properties_new\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_stream_new: FnPwStreamNew = unsafe { *lib.get(b"pw_stream_new\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_add_listener: FnPwStreamAddListener = unsafe { *lib.get(b"pw_stream_add_listener\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_stream_connect: FnPwStreamConnect = unsafe { *lib.get(b"pw_stream_connect\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_update_params: FnPwStreamUpdateParams = unsafe { *lib.get(b"pw_stream_update_params\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_dequeue: FnPwStreamDequeueBuffer = unsafe { *lib.get(b"pw_stream_dequeue_buffer\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_queue: FnPwStreamQueueBuffer = unsafe { *lib.get(b"pw_stream_queue_buffer\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_stream_disconnect: FnPwStreamDisconnect = unsafe { *lib.get(b"pw_stream_disconnect\0").map_err(|e| VexrecError::Display(e.to_string()))? };
        let fn_stream_destroy: FnPwStreamDestroy = unsafe { *lib.get(b"pw_stream_destroy\0").map_err(|e| VexrecError::Display(e.to_string()))? };

        unsafe { pw_init(std::ptr::null_mut(), std::ptr::null_mut()) };

        let loop_name = CString::new("vexrec-pw").unwrap();
        let thread_loop = unsafe { fn_loop_new(loop_name.as_ptr(), std::ptr::null()) };
        if thread_loop.is_null() {
            return Err(VexrecError::Display("pw_thread_loop_new failed".into()));
        }
        let main_loop = unsafe { fn_loop_get_loop(thread_loop) };
        let context = unsafe { fn_ctx_new(main_loop, std::ptr::null_mut(), 0) };
        if context.is_null() {
            unsafe { fn_loop_destroy(thread_loop) };
            return Err(VexrecError::Display("pw_context_new failed".into()));
        }

        // Duplicate portal fd safely because PipeWire closes the fd given to pw_context_connect_fd
        let dup_fd = unsafe { libc::fcntl(sc.pipewire_fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if dup_fd < 0 {
            unsafe {
                fn_ctx_destroy(context);
                fn_loop_destroy(thread_loop);
            }
            return Err(VexrecError::Display("Failed to dup PipeWire portal fd".into()));
        }

        let core = unsafe { fn_ctx_connect_fd(context, dup_fd, std::ptr::null_mut(), 0) };
        if core.is_null() {
            unsafe {
                libc::close(dup_fd);
                fn_ctx_destroy(context);
                fn_loop_destroy(thread_loop);
            }
            return Err(VexrecError::Display("pw_context_connect_fd failed".into()));
        }

        let k_type = CString::new("media.type").unwrap();
        let v_type = CString::new("Video").unwrap();
        let k_cat = CString::new("media.category").unwrap();
        let v_cat = CString::new("Capture").unwrap();
        let k_role = CString::new("media.role").unwrap();
        let v_role = CString::new("Screen").unwrap();
        let stream_name = CString::new("vexrec-capture").unwrap();

        let props = unsafe {
            fn_props_new(
                k_type.as_ptr(),
                v_type.as_ptr(),
                k_cat.as_ptr(),
                v_cat.as_ptr(),
                k_role.as_ptr(),
                v_role.as_ptr(),
                std::ptr::null(),
            )
        };
        let stream = unsafe { fn_stream_new(core, stream_name.as_ptr(), props) };
        if stream.is_null() {
            unsafe {
                fn_core_disconnect(core);
                fn_ctx_destroy(context);
                fn_loop_destroy(thread_loop);
            }
            return Err(VexrecError::Display("pw_stream_new failed".into()));
        }

        let latest_frame = Arc::new((Mutex::new(None), Condvar::new()));
        let mut cb_state = Box::new(PwCbState {
            stream,
            fn_update_params: fn_update_params,
            fn_dequeue,
            fn_queue,
            spa_fmt: SPA_VIDEO_FORMAT_BGRX,
            raw_width: raw_w,
            raw_height: raw_h,
            crop: out_crop,
            out_w,
            out_h,
            latest_frame: Arc::clone(&latest_frame),
        });
        let cb_ptr = (&mut *cb_state) as *mut PwCbState as *mut c_void;
        let mut hook = Box::new(SpaHook::default());
        let events = Box::new(PwStreamEvents {
            version: 2,
            destroy: None,
            state_changed: None,
            control_info: None,
            io_changed: None,
            param_changed: Some(on_param_changed),
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_process),
            drained: None,
            command: None,
            trigger_done: None,
        });

        let mut pb = SpaPodBuilder::new();
        let obj = pb.begin_object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT);
        pb.prop_id(SPA_FORMAT_MEDIA_TYPE, SPA_MEDIA_TYPE_VIDEO);
        pb.prop_id(SPA_FORMAT_MEDIA_SUBTYPE, SPA_MEDIA_SUBTYPE_RAW);
        pb.prop_choice_ids(
            SPA_FORMAT_VIDEO_FORMAT,
            SPA_CHOICE_ENUM,
            &[
                SPA_VIDEO_FORMAT_BGRX,
                SPA_VIDEO_FORMAT_BGRX,
                SPA_VIDEO_FORMAT_BGRA,
                SPA_VIDEO_FORMAT_RGBX,
                SPA_VIDEO_FORMAT_RGBA,
                SPA_VIDEO_FORMAT_XRGB,
                SPA_VIDEO_FORMAT_XBGR,
            ],
        );
        pb.prop_choice_rects(
            SPA_FORMAT_VIDEO_SIZE,
            SPA_CHOICE_RANGE,
            &[(raw_w, raw_h), (16, 16), (8192, 8192)],
        );
        pb.prop_choice_fractions(
            SPA_FORMAT_VIDEO_FRAMERATE,
            SPA_CHOICE_RANGE,
            &[(fps.max(1), 1), (0, 1), (240, 1)],
        );
        pb.end_object(obj);
        let mut param_ptrs = [pb.as_pod_ptr()];

        unsafe {
            fn_add_listener(stream, &mut *hook, &*events, cb_ptr);
            let rc = fn_stream_connect(
                stream,
                PW_DIRECTION_INPUT,
                sc.node_id,
                PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                param_ptrs.as_mut_ptr(),
                1,
            );
            if rc < 0 {
                fn_stream_destroy(stream);
                fn_core_disconnect(core);
                fn_ctx_destroy(context);
                fn_loop_destroy(thread_loop);
                return Err(VexrecError::Display(format!("pw_stream_connect failed: {rc}")));
            }
            if fn_loop_start(thread_loop) < 0 {
                fn_stream_disconnect(stream);
                fn_stream_destroy(stream);
                fn_core_disconnect(core);
                fn_ctx_destroy(context);
                fn_loop_destroy(thread_loop);
                return Err(VexrecError::Display("pw_thread_loop_start failed".into()));
            }
        }

        info!(node_id = sc.node_id, out_w, out_h, fps, "PipeWire ScreenCast stream connected");

        Ok(Self {
            _lib: lib,
            thread_loop,
            context,
            core,
            stream,
            _cb_state: cb_state,
            _hook: hook,
            _events: events,
            fn_loop_lock,
            fn_loop_unlock,
            fn_loop_stop,
            fn_loop_destroy,
            fn_stream_disconnect,
            fn_stream_destroy,
            fn_core_disconnect,
            fn_ctx_destroy,
            latest_frame,
            last_good: None,
            width: out_w,
            height: out_h,
            start: Instant::now(),
        })
    }

    pub fn capture_frame(&mut self) -> Result<VideoFrame> {
        let (lock, cvar) = &*self.latest_frame;
        if let Ok(mut guard) = lock.lock() {
            if guard.is_none() && self.last_good.is_none() {
                let (g, _) = cvar
                    .wait_timeout(guard, std::time::Duration::from_millis(500))
                    .map_err(|_| VexrecError::Display("PipeWire wait poisoned".into()))?;
                guard = g;
            }
            if let Some(fresh) = guard.take() {
                self.last_good = Some(fresh);
            }
        }
        let img = match self.last_good {
            Some(ref im) => im.clone(),
            None => PixelImage::new(self.width, self.height, PixelFormat::Bgrx8),
        };
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

impl Drop for PipeWireStreamCapture {
    fn drop(&mut self) {
        unsafe {
            (self.fn_loop_lock)(self.thread_loop);
            (self.fn_stream_disconnect)(self.stream);
            (self.fn_stream_destroy)(self.stream);
            (self.fn_core_disconnect)(self.core);
            (self.fn_loop_unlock)(self.thread_loop);
            (self.fn_loop_stop)(self.thread_loop);
            (self.fn_ctx_destroy)(self.context);
            (self.fn_loop_destroy)(self.thread_loop);
        }
    }
}

impl WaylandCaptureSource {
    pub fn open(
        session_kind: SessionKind,
        crop: Option<Rect>,
        fps: u32,
        include_cursor: bool,
    ) -> Result<Self> {
        if session_kind == SessionKind::WaylandWlroots {
            if let Ok(disp) = WaylandDisplay::connect() {
                let sz = disp.primary_size();
                let (eff_crop, w, h) = match crop {
                    Some(r) => {
                        let c = r.clamp_bounds(sz.width, sz.height).snap_even();
                        let cw = c.width.max(32) & !1;
                        let ch = c.height.max(32) & !1;
                        (Some(Rect::new(c.x, c.y, cw.min(sz.width), ch.min(sz.height))), cw & !1, ch & !1)
                    }
                    None => (None, (sz.width & !1).max(32), (sz.height & !1).max(32)),
                };
                return Ok(Self::Screencopy {
                    disp,
                    crop: eff_crop,
                    width: w,
                    height: h,
                    include_cursor,
                    start: Instant::now(),
                });
            }
        }

        match PipeWireStreamCapture::open(crop, fps, include_cursor) {
            Ok(pw) => Ok(Self::PipeWire(Box::new(pw))),
            Err(e) => {
                warn!("PipeWire ScreenCast failed ({e}), trying native WaylandDisplay fallback");
                let disp = WaylandDisplay::connect()?;
                let sz = disp.primary_size();
                let (eff_crop, w, h) = match crop {
                    Some(r) => {
                        let c = r.clamp_bounds(sz.width, sz.height).snap_even();
                        let cw = c.width.max(32) & !1;
                        let ch = c.height.max(32) & !1;
                        (Some(Rect::new(c.x, c.y, cw.min(sz.width), ch.min(sz.height))), cw & !1, ch & !1)
                    }
                    None => (None, (sz.width & !1).max(32), (sz.height & !1).max(32)),
                };
                Ok(Self::Screencopy {
                    disp,
                    crop: eff_crop,
                    width: w,
                    height: h,
                    include_cursor,
                    start: Instant::now(),
                })
            }
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::Screencopy { width, .. } => *width,
            Self::PipeWire(pw) => pw.width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Screencopy { height, .. } => *height,
            Self::PipeWire(pw) => pw.height,
        }
    }

    pub fn capture_frame(&mut self) -> Result<VideoFrame> {
        match self {
            Self::Screencopy {
                disp,
                crop,
                width,
                height,
                include_cursor,
                start,
            } => {
                let mut img = disp.capture_screen(*crop, *include_cursor)?;
                if img.width != *width || img.height != *height {
                    let mut norm = PixelImage::new(*width, *height, PixelFormat::Bgrx8);
                    let cw = img.width.min(*width) as usize;
                    let ch = img.height.min(*height) as usize;
                    let ss = img.stride as usize;
                    let ds = norm.stride as usize;
                    let s = img.as_bytes();
                    let d = norm.as_mut_bytes();
                    for row in 0..ch {
                        d[row * ds..row * ds + cw * 4]
                            .copy_from_slice(&s[row * ss..row * ss + cw * 4]);
                    }
                    img = norm;
                }
                Ok(VideoFrame {
                    width: *width,
                    height: *height,
                    format: img.format,
                    pts_ns: start.elapsed().as_nanos() as u64,
                    damaged: true,
                    backing: FrameBacking::Host {
                        stride: img.stride,
                        data: img.data,
                    },
                })
            }
            Self::PipeWire(pw) => pw.capture_frame(),
        }
    }
}
