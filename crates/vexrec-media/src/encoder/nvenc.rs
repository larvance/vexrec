//! NVIDIA NVENC runtime capability detector dynamically probing `libnvidia-encode.so.1`
//! without waking a D3cold dGPU unless explicitly invoked for diagnostics/encoding.

use libloading::Library;

/// Probes whether `libnvidia-encode.so.1` and `libcuda.so.1` are present and loadable cleanly.
pub fn probe_nvenc_runtime() -> bool {
    unsafe {
        Library::new("libnvidia-encode.so.1").is_ok() && Library::new("libcuda.so.1").is_ok()
    }
}
