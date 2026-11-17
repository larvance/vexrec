use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use libloading::Library;
use tracing::warn;
use vexrec_core::error::{Result, VexrecError};

const SND_PCM_STREAM_CAPTURE: c_int = 1;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
const SND_PCM_FORMAT_S16_LE: c_int = 2;
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;

type FnSndPcmOpen =
    unsafe extern "C" fn(pcm: *mut *mut c_void, name: *const c_char, stream: c_int, mode: c_int) -> c_int;
type FnSndPcmClose = unsafe extern "C" fn(pcm: *mut c_void) -> c_int;
type FnSndPcmSetParams = unsafe extern "C" fn(
    pcm: *mut c_void,
    format: c_int,
    access: c_int,
    channels: c_uint,
    rate: c_uint,
    soft_resample: c_int,
    latency: c_uint,
) -> c_int;
type FnSndPcmReadi =
    unsafe extern "C" fn(pcm: *mut c_void, buffer: *mut c_void, size: c_ulong) -> c_long;
type FnSndPcmRecover =
    unsafe extern "C" fn(pcm: *mut c_void, err: c_int, silent: c_int) -> c_int;

/// Pure-ABI dynamic `libasound.so.2` fallback capture when Pulse/PipeWire-Pulse is absent.
pub struct AlsaCaptureReader {
    _lib: Library,
    pcm: *mut c_void,
    snd_readi: FnSndPcmReadi,
    snd_recover: FnSndPcmRecover,
    snd_close: FnSndPcmClose,
    is_float: bool,
    channels: usize,
}

unsafe impl Send for AlsaCaptureReader {}

impl AlsaCaptureReader {
    pub fn open_default(rate: u32, channels: u32) -> Result<Self> {
        let lib = unsafe { Library::new("libasound.so.2") }
            .map_err(|e| VexrecError::Audio(format!("libasound.so.2 unavailable: {e}")))?;

        let snd_open: FnSndPcmOpen = unsafe { *lib.get(b"snd_pcm_open\0").map_err(|e| VexrecError::Audio(e.to_string()))? };
        let snd_close: FnSndPcmClose = unsafe { *lib.get(b"snd_pcm_close\0").map_err(|e| VexrecError::Audio(e.to_string()))? };
        let snd_set: FnSndPcmSetParams = unsafe { *lib.get(b"snd_pcm_set_params\0").map_err(|e| VexrecError::Audio(e.to_string()))? };
        let snd_readi: FnSndPcmReadi = unsafe { *lib.get(b"snd_pcm_readi\0").map_err(|e| VexrecError::Audio(e.to_string()))? };
        let snd_recover: FnSndPcmRecover = unsafe { *lib.get(b"snd_pcm_recover\0").map_err(|e| VexrecError::Audio(e.to_string()))? };

        let dev = CString::new("default").unwrap();
        let mut pcm: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { snd_open(&mut pcm, dev.as_ptr(), SND_PCM_STREAM_CAPTURE, 0) };
        if rc < 0 || pcm.is_null() {
            return Err(VexrecError::Audio(format!("ALSA snd_pcm_open failed: {rc}")));
        }

        let mut is_float = true;
        let rc_f = unsafe {
            snd_set(
                pcm,
                SND_PCM_FORMAT_FLOAT_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                channels,
                rate,
                1,
                20_000, // 20 ms latency
            )
        };
        if rc_f < 0 {
            is_float = false;
            let rc_s = unsafe {
                snd_set(
                    pcm,
                    SND_PCM_FORMAT_S16_LE,
                    SND_PCM_ACCESS_RW_INTERLEAVED,
                    channels,
                    rate,
                    1,
                    20_000,
                )
            };
            if rc_s < 0 {
                unsafe { snd_close(pcm) };
                return Err(VexrecError::Audio(format!("ALSA snd_pcm_set_params failed: {rc_s}")));
            }
        }

        Ok(Self {
            _lib: lib,
            pcm,
            snd_readi,
            snd_recover,
            snd_close,
            is_float,
            channels: channels as usize,
        })
    }

    pub fn read_frames_f32(&mut self, frames: usize, out: &mut Vec<f32>) -> bool {
        out.resize(frames * self.channels, 0.0);
        if self.is_float {
            let n = unsafe { (self.snd_readi)(self.pcm, out.as_mut_ptr() as *mut c_void, frames as c_ulong) };
            if n < 0 {
                let r = unsafe { (self.snd_recover)(self.pcm, n as c_int, 1) };
                if r < 0 {
                    warn!(err = r, "ALSA unrecoverable capture error");
                    return false;
                }
                out.fill(0.0);
                return true;
            }
            out.truncate((n as usize) * self.channels);
            true
        } else {
            let mut s16 = vec![0i16; frames * self.channels];
            let n = unsafe { (self.snd_readi)(self.pcm, s16.as_mut_ptr() as *mut c_void, frames as c_ulong) };
            if n < 0 {
                let r = unsafe { (self.snd_recover)(self.pcm, n as c_int, 1) };
                if r < 0 {
                    warn!(err = r, "ALSA S16 unrecoverable capture error");
                    return false;
                }
                out.fill(0.0);
                return true;
            }
            let samples = (n as usize) * self.channels;
            out.clear();
            for &s in &s16[..samples] {
                out.push((s as f32) / 32768.0);
            }
            true
        }
    }
}

impl Drop for AlsaCaptureReader {
    fn drop(&mut self) {
        if !self.pcm.is_null() {
            unsafe { (self.snd_close)(self.pcm) };
            self.pcm = std::ptr::null_mut();
        }
    }
}

pub fn run_alsa_capture_loop(mut reader: AlsaCaptureReader, mut tx: rtrb::Producer<f32>, stop: &AtomicBool) {
    let mut tmp = Vec::with_capacity(480 * reader.channels);
    while !stop.load(Ordering::Relaxed) {
        if !reader.read_frames_f32(480, &mut tmp) {
            break;
        }
        let _ = tx.push_partial_slice(&tmp);
    }
}
