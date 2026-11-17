use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use libloading::{Library, Symbol};
use tracing::warn;
use vexrec_core::error::{Result, VexrecError};

const PA_SAMPLE_FLOAT32LE: i32 = 5;
const PA_STREAM_RECORD: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct PaSampleSpec {
    format: i32,
    rate: u32,
    channels: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type FnPaSimpleNew = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const PaSampleSpec,
    map: *const c_void,
    attr: *const PaBufferAttr,
    error: *mut c_int,
) -> *mut c_void;

type FnPaSimpleRead =
    unsafe extern "C" fn(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;

type FnPaSimpleFree = unsafe extern "C" fn(s: *mut c_void);

/// Runtime `dlopen("libpulse-simple.so.0")` audio reader covering both PulseAudio and PipeWire (`pipewire-pulse`).
pub struct PulseSimpleReader {
    _lib: Library,
    _lib_pulse: Option<Library>,
    handle: *mut c_void,
    pa_read: FnPaSimpleRead,
    pa_free: FnPaSimpleFree,
    channels: usize,
}

unsafe impl Send for PulseSimpleReader {}

impl PulseSimpleReader {
    pub fn open(device: Option<&str>, stream_name: &str, rate: u32, channels: u8) -> Result<Self> {
        let lib = unsafe { Library::new("libpulse-simple.so.0") }
            .map_err(|e| VexrecError::Audio(format!("libpulse-simple.so.0 unavailable: {e}")))?;

        let pa_new: FnPaSimpleNew = unsafe {
            let s: Symbol<FnPaSimpleNew> = lib
                .get(b"pa_simple_new\0")
                .map_err(|e| VexrecError::Audio(e.to_string()))?;
            *s
        };
        let pa_read: FnPaSimpleRead = unsafe {
            let s: Symbol<FnPaSimpleRead> = lib
                .get(b"pa_simple_read\0")
                .map_err(|e| VexrecError::Audio(e.to_string()))?;
            *s
        };
        let pa_free: FnPaSimpleFree = unsafe {
            let s: Symbol<FnPaSimpleFree> = lib
                .get(b"pa_simple_free\0")
                .map_err(|e| VexrecError::Audio(e.to_string()))?;
            *s
        };

        let lib_pulse = unsafe { Library::new("libpulse.so.0") }.ok();
        let app = CString::new("Vexrec").unwrap();
        let st_name = CString::new(stream_name).unwrap_or_else(|_| CString::new("Capture").unwrap());
        let dev_c = device.and_then(|d| CString::new(d).ok());

        let ss = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate,
            channels,
        };
        // 10 ms low-latency fragment chunks (480 frames @ 48kHz)
        let frag_bytes = ((rate / 100) * (channels as u32) * 4).max(1024);
        let attr = PaBufferAttr {
            maxlength: frag_bytes * 8,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: frag_bytes,
        };

        let mut err: c_int = 0;
        let handle = unsafe {
            pa_new(
                std::ptr::null(),
                app.as_ptr(),
                PA_STREAM_RECORD,
                dev_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                st_name.as_ptr(),
                &ss,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };

        if handle.is_null() {
            let reason = if let Some(ref lp) = lib_pulse {
                if let Ok(sym) = unsafe { lp.get::<unsafe extern "C" fn(c_int) -> *const c_char>(b"pa_strerror\0") } {
                    let p = unsafe { sym(err) };
                    if !p.is_null() {
                        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                    } else {
                        format!("code {err}")
                    }
                } else {
                    format!("code {err}")
                }
            } else {
                format!("code {err}")
            };
            return Err(VexrecError::Audio(format!(
                "pa_simple_new({:?}) failed: {reason}",
                device
            )));
        }

        Ok(Self {
            _lib: lib,
            _lib_pulse: lib_pulse,
            handle,
            pa_read,
            pa_free,
            channels: channels as usize,
        })
    }

    /// Reads exactly `out.len()` floats (must be a multiple of channels) into `out`.
    pub fn read_f32(&mut self, out: &mut [f32]) -> bool {
        let bytes = std::mem::size_of_val(out);
        let mut err: c_int = 0;
        let rc = unsafe { (self.pa_read)(self.handle, out.as_mut_ptr() as *mut c_void, bytes, &mut err) };
        if rc < 0 {
            warn!(err, "pa_simple_read error");
            return false;
        }
        true
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for PulseSimpleReader {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.pa_free)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// Checks if `libpulse-simple.so.0` is available for runtime audio capture.
pub fn is_pulse_available() -> bool {
    unsafe { Library::new("libpulse-simple.so.0") }.is_ok()
}

pub fn run_pulse_capture_loop(
    mut reader: PulseSimpleReader,
    mut tx: rtrb::Producer<f32>,
    stop: &AtomicBool,
) {
    let chunk_samples = 480 * reader.channels();
    let mut buf = vec![0.0f32; chunk_samples];
    while !stop.load(Ordering::Relaxed) {
        if !reader.read_f32(&mut buf) {
            break;
        }
        // Push non-blocking into lock-free SPSC ring buffer, dropping oldest-excess cleanly on overrun
        let pushed = tx.push_partial_slice(&buf);
        let _ = pushed;
    }
}
