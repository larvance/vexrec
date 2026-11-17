//! Hardware probes and encoder interfaces for VA-API (DRM via dlopen), Vulkan Video (ash),
//! and NVIDIA NVENC (dlopen).

use std::ffi::{c_int, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use libloading::{Library, Symbol};
use tracing::{debug, info};
use vexrec_core::buffer::{FrameBacking, VideoFrame};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::VideoQuality;
use vexrec_os::drm::{display_render_node, enumerate_drm_devices};
use vexrec_os::simd::bgra_to_nv12_bt709;

use super::{EncodedVideoPacket, VideoEncoder};
use crate::mux::bitstream::{
    build_h264_slice_header_vaapi, build_h264_sps_pps_vaapi, extract_h264_sps_pps,
};

// Canonical libva 2.x constants
const VA_STATUS_SUCCESS: c_int = 0;
const VA_INVALID_ID: u32 = 0xffff_ffff;
const VA_PROFILE_H264_MAIN: c_int = 6;
const VA_PROFILE_H264_HIGH: c_int = 7;
const VA_ENTRYPOINT_ENCSLICE: c_int = 6;
const VA_ENTRYPOINT_ENCSLICE_LP: c_int = 8;
const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 1;
const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;

const VA_RC_CBR: u32 = 0x0000_0002;
const VA_RC_CQP: u32 = 0x0000_0010;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
const VA_ENC_PACKED_HEADER_SLICE: u32 = 0x0000_0004;

const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: c_int = 22;
const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: c_int = 23;
const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: c_int = 24;
const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: c_int = 25;
const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: c_int = 26;
const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: c_int = 27;
const VA_ENC_CODED_BUFFER_TYPE: c_int = 21;

const VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL: c_int = 1;
const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;

const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
const VA_GENERIC_VALUE_INTEGER: c_int = 1;

type VADisplay = *mut c_void;
type VAConfigID = u32;
type VAContextID = u32;
type VASurfaceID = u32;
type VABufferID = u32;
type VAImageID = u32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VAConfigAttrib {
    type_: c_int,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VASurfaceAttrib {
    type_: c_int,
    flags: u32,
    value: VAGenericValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VAGenericValue {
    type_: c_int,
    value: VAGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
union VAGenericValueUnion {
    i: c_int,
    f: f32,
    p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VAImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VAImage {
    image_id: VAImageID,
    format: VAImageFormat,
    buf: VABufferID,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: c_int,
    entry_bytes: c_int,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VACodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut VACodedBufferSegment,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VAPictureH264 {
    picture_id: VASurfaceID,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}

impl Default for VAPictureH264 {
    fn default() -> Self {
        Self {
            picture_id: VA_INVALID_ID,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        }
    }
}

#[repr(C)]
struct VAEncSequenceParameterBufferH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VAEncPictureParameterBufferH264 {
    curr_pic: VAPictureH264,
    reference_frames: [VAPictureH264; 16],
    coded_buf: VABufferID,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VAEncSliceParameterBufferH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: VABufferID,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list_0: [VAPictureH264; 32],
    ref_pic_list_1: [VAPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VAEncPackedHeaderParameterBuffer {
    type_: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VAEncMiscParameterRateControlWrapper {
    type_: c_int,
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    rc_flags: u32,
    icq_quality_factor: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    va_reserved: [u32; 4],
}

#[allow(non_snake_case)]
struct LibVaApi {
    _lib_va: Library,
    _lib_drm: Library,
    vaGetDisplayDRM: unsafe extern "C" fn(fd: c_int) -> VADisplay,
    vaInitialize: unsafe extern "C" fn(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> c_int,
    vaTerminate: unsafe extern "C" fn(dpy: VADisplay) -> c_int,
    vaMaxNumEntrypoints: unsafe extern "C" fn(dpy: VADisplay) -> c_int,
    vaQueryConfigEntrypoints: unsafe extern "C" fn(dpy: VADisplay, profile: c_int, ep_list: *mut c_int, num_ep: *mut c_int) -> c_int,
    vaGetConfigAttributes: unsafe extern "C" fn(dpy: VADisplay, profile: c_int, entrypoint: c_int, attrib_list: *mut VAConfigAttrib, num: c_int) -> c_int,
    vaCreateConfig: unsafe extern "C" fn(dpy: VADisplay, profile: c_int, entrypoint: c_int, attrib_list: *mut VAConfigAttrib, num: c_int, config_id: *mut VAConfigID) -> c_int,
    vaDestroyConfig: unsafe extern "C" fn(dpy: VADisplay, config_id: VAConfigID) -> c_int,
    vaCreateSurfaces: unsafe extern "C" fn(dpy: VADisplay, format: u32, w: u32, h: u32, surfaces: *mut VASurfaceID, num: u32, attrib_list: *mut VASurfaceAttrib, num_attribs: u32) -> c_int,
    vaDestroySurfaces: unsafe extern "C" fn(dpy: VADisplay, surfaces: *mut VASurfaceID, num: c_int) -> c_int,
    vaCreateContext: unsafe extern "C" fn(dpy: VADisplay, config_id: VAConfigID, w: c_int, h: c_int, flag: c_int, render_targets: *mut VASurfaceID, num: c_int, ctx: *mut VAContextID) -> c_int,
    vaDestroyContext: unsafe extern "C" fn(dpy: VADisplay, ctx: VAContextID) -> c_int,
    vaCreateBuffer: unsafe extern "C" fn(dpy: VADisplay, ctx: VAContextID, type_: c_int, size: u32, num_elements: u32, data: *mut c_void, buf_id: *mut VABufferID) -> c_int,
    vaDestroyBuffer: unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID) -> c_int,
    vaMapBuffer: unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID, pbuf: *mut *mut c_void) -> c_int,
    vaUnmapBuffer: unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID) -> c_int,
    vaBeginPicture: unsafe extern "C" fn(dpy: VADisplay, ctx: VAContextID, render_target: VASurfaceID) -> c_int,
    vaRenderPicture: unsafe extern "C" fn(dpy: VADisplay, ctx: VAContextID, buffers: *mut VABufferID, num: c_int) -> c_int,
    vaEndPicture: unsafe extern "C" fn(dpy: VADisplay, ctx: VAContextID) -> c_int,
    vaSyncSurface: unsafe extern "C" fn(dpy: VADisplay, render_target: VASurfaceID) -> c_int,
    vaCreateImage: unsafe extern "C" fn(dpy: VADisplay, format: *mut VAImageFormat, w: c_int, h: c_int, image: *mut VAImage) -> c_int,
    vaPutImage: unsafe extern "C" fn(dpy: VADisplay, surface: VASurfaceID, image: VAImageID, sx: c_int, sy: c_int, sw: u32, sh: u32, dx: c_int, dy: c_int, dw: u32, dh: u32) -> c_int,
    vaDestroyImage: unsafe extern "C" fn(dpy: VADisplay, image: VAImageID) -> c_int,
}

static VA_API: OnceLock<Option<LibVaApi>> = OnceLock::new();

// SAFETY: LibVaApi holds handle wrappers & resolved immutable C function pointers safe across threads cleanly.
unsafe impl Sync for LibVaApi {}
unsafe impl Send for LibVaApi {}

impl LibVaApi {
    fn get() -> Option<&'static Self> {
        VA_API.get_or_init(|| unsafe { Self::load() }).as_ref()
    }

    unsafe fn load() -> Option<Self> {
        let lib_va = Library::new("libva.so.2").ok()?;
        let lib_drm = Library::new("libva-drm.so.2").ok()?;

        macro_rules! sym {
            ($lib:expr, $name:literal) => {{
                let s: Symbol<'_, _> = $lib.get($name).ok()?;
                *s
            }};
        }

        Some(Self {
            vaGetDisplayDRM: sym!(lib_drm, b"vaGetDisplayDRM\0"),
            vaInitialize: sym!(lib_va, b"vaInitialize\0"),
            vaTerminate: sym!(lib_va, b"vaTerminate\0"),
            vaMaxNumEntrypoints: sym!(lib_va, b"vaMaxNumEntrypoints\0"),
            vaQueryConfigEntrypoints: sym!(lib_va, b"vaQueryConfigEntrypoints\0"),
            vaGetConfigAttributes: sym!(lib_va, b"vaGetConfigAttributes\0"),
            vaCreateConfig: sym!(lib_va, b"vaCreateConfig\0"),
            vaDestroyConfig: sym!(lib_va, b"vaDestroyConfig\0"),
            vaCreateSurfaces: sym!(lib_va, b"vaCreateSurfaces\0"),
            vaDestroySurfaces: sym!(lib_va, b"vaDestroySurfaces\0"),
            vaCreateContext: sym!(lib_va, b"vaCreateContext\0"),
            vaDestroyContext: sym!(lib_va, b"vaDestroyContext\0"),
            vaCreateBuffer: sym!(lib_va, b"vaCreateBuffer\0"),
            vaDestroyBuffer: sym!(lib_va, b"vaDestroyBuffer\0"),
            vaMapBuffer: sym!(lib_va, b"vaMapBuffer\0"),
            vaUnmapBuffer: sym!(lib_va, b"vaUnmapBuffer\0"),
            vaBeginPicture: sym!(lib_va, b"vaBeginPicture\0"),
            vaRenderPicture: sym!(lib_va, b"vaRenderPicture\0"),
            vaEndPicture: sym!(lib_va, b"vaEndPicture\0"),
            vaSyncSurface: sym!(lib_va, b"vaSyncSurface\0"),
            vaCreateImage: sym!(lib_va, b"vaCreateImage\0"),
            vaPutImage: sym!(lib_va, b"vaPutImage\0"),
            vaDestroyImage: sym!(lib_va, b"vaDestroyImage\0"),
            _lib_va: lib_va,
            _lib_drm: lib_drm,
        })
    }
}

fn open_drm_rw(node: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(node)
}

fn find_non_nvidia_render_node() -> Option<PathBuf> {
    if let Some(disp) = display_render_node() {
        if disp.vendor_id != vexrec_os::DrmDeviceInfo::VENDOR_NVIDIA {
            return disp.render_path;
        }
    }
    enumerate_drm_devices()
        .into_iter()
        .find(|d| d.vendor_id != vexrec_os::DrmDeviceInfo::VENDOR_NVIDIA && d.render_path.is_some())
        .and_then(|d| d.render_path)
}

/// Probes VA-API DRM hardware H.264 encode availability without touching D3cold discrete NVIDIA nodes.
pub fn probe_vaapi_h264() -> Option<String> {
    let node = find_non_nvidia_render_node()?;
    let file = open_drm_rw(&node).ok()?;
    unsafe {
        let api = LibVaApi::get()?;
        let dpy = (api.vaGetDisplayDRM)(file.as_raw_fd());
        if dpy.is_null() {
            return None;
        }
        let mut major = 0;
        let mut minor = 0;
        if (api.vaInitialize)(dpy, &mut major, &mut minor) != VA_STATUS_SUCCESS {
            return None;
        }
        let max_ep = (api.vaMaxNumEntrypoints)(dpy).max(1) as usize;
        let mut eps = vec![0i32; max_ep];
        let mut found = None;
        for profile in [VA_PROFILE_H264_HIGH, VA_PROFILE_H264_MAIN] {
            let mut num = 0i32;
            if (api.vaQueryConfigEntrypoints)(dpy, profile, eps.as_mut_ptr(), &mut num) == VA_STATUS_SUCCESS {
                let slice = &eps[..(num as usize).min(max_ep)];
                if slice.contains(&VA_ENTRYPOINT_ENCSLICE) || slice.contains(&VA_ENTRYPOINT_ENCSLICE_LP) {
                    found = Some(format!("VA-API {major}.{minor} on {}", node.display()));
                    break;
                }
            }
        }
        let _ = (api.vaTerminate)(dpy);
        found
    }
}

/// Direct OS-level Linux VA-API DRM H.264 encoder loaded dynamically via `libva.so.2` (`0` build-time headers).
pub struct VaApiH264Encoder {
    api: &'static LibVaApi,
    _drm_file: File,
    dpy: VADisplay,
    config_id: VAConfigID,
    ctx_id: VAContextID,
    surfaces: [VASurfaceID; 3],
    coded_buf: VABufferID,
    staging_img: VAImage,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    frame_count: u64,
    idr_count: u16,
    idr_period: u64,
    packed_headers_supported: u32,
    profile_idc: u8,
    sps_bytes: Vec<u8>,
    pps_bytes: Vec<u8>,
    y_scratch: Vec<u8>,
    uv_scratch: Vec<u8>,
}

// SAFETY: VaApiH264Encoder owns its isolated VADisplay/VAContext cleanly and executes serially on the encoder worker thread.
unsafe impl Send for VaApiH264Encoder {}

impl VaApiH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, quality: VideoQuality) -> Result<Self> {
        let node = find_non_nvidia_render_node()
            .ok_or_else(|| VexrecError::Encoding("No AMD/Intel DRM render node found for VA-API".into()))?;
        let drm_file = open_drm_rw(&node)?;
        let w = (width & !1).max(32);
        let h = (height & !1).max(32);
        let fps = fps.max(1);
        let bitrate = quality.target_bitrate_bps();

        unsafe {
            let api = LibVaApi::get()
                .ok_or_else(|| VexrecError::Encoding("libva.so.2 / libva-drm.so.2 unavailable".into()))?;
            let dpy = (api.vaGetDisplayDRM)(drm_file.as_raw_fd());
            if dpy.is_null() {
                return Err(VexrecError::Encoding("vaGetDisplayDRM returned null".into()));
            }
            let mut major = 0;
            let mut minor = 0;
            if (api.vaInitialize)(dpy, &mut major, &mut minor) != VA_STATUS_SUCCESS {
                return Err(VexrecError::Encoding("vaInitialize failed".into()));
            }

            let max_ep = (api.vaMaxNumEntrypoints)(dpy).max(1) as usize;
            let mut eps = vec![0i32; max_ep];
            let mut selected: Option<(c_int, c_int, u8)> = None;
            for (profile, idc) in [(VA_PROFILE_H264_HIGH, 100u8), (VA_PROFILE_H264_MAIN, 77u8)] {
                let mut num = 0i32;
                if (api.vaQueryConfigEntrypoints)(dpy, profile, eps.as_mut_ptr(), &mut num) == VA_STATUS_SUCCESS {
                    let sl = &eps[..(num as usize).min(max_ep)];
                    if sl.contains(&VA_ENTRYPOINT_ENCSLICE_LP) {
                        selected = Some((profile, VA_ENTRYPOINT_ENCSLICE_LP, idc));
                        break;
                    }
                    if sl.contains(&VA_ENTRYPOINT_ENCSLICE) {
                        selected = Some((profile, VA_ENTRYPOINT_ENCSLICE, idc));
                        break;
                    }
                }
            }
            let Some((profile, entrypoint, profile_idc)) = selected else {
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("No VA-API H.264 encode entrypoint exposed by driver".into()));
            };

            let mut q_attribs = [
                VAConfigAttrib { type_: VA_CONFIG_ATTRIB_RATE_CONTROL, value: 0 },
                VAConfigAttrib { type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS, value: 0 },
            ];
            let _ = (api.vaGetConfigAttributes)(dpy, profile, entrypoint, q_attribs.as_mut_ptr(), 2);
            let rc_supported = if q_attribs[0].value == 0x8000_0000 { 0 } else { q_attribs[0].value };
            let rc_mode = if (rc_supported & VA_RC_CBR) != 0 {
                VA_RC_CBR
            } else {
                VA_RC_CQP
            };
            let packed_headers_supported = if q_attribs[1].value == 0x8000_0000 { 0 } else { q_attribs[1].value };
            let desired_packed = packed_headers_supported
                & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE);

            let mut cfg_attribs = [
                VAConfigAttrib { type_: VA_CONFIG_ATTRIB_RT_FORMAT, value: VA_RT_FORMAT_YUV420 },
                VAConfigAttrib { type_: VA_CONFIG_ATTRIB_RATE_CONTROL, value: rc_mode },
                VAConfigAttrib { type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS, value: desired_packed },
            ];
            let n_cfg_attribs = if desired_packed != 0 { 3 } else { 2 };
            let mut config_id = 0u32;
            if (api.vaCreateConfig)(dpy, profile, entrypoint, cfg_attribs.as_mut_ptr(), n_cfg_attribs, &mut config_id) != VA_STATUS_SUCCESS {
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("vaCreateConfig failed".into()));
            }

            let mut surfaces = [0u32; 3];
            let mut surf_attrib = VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
                flags: 2, // VA_SURFACE_ATTRIB_SETTABLE = 0x00000002
                value: VAGenericValue {
                    type_: VA_GENERIC_VALUE_INTEGER,
                    value: VAGenericValueUnion { i: VA_FOURCC_NV12 as c_int },
                },
            };
            if (api.vaCreateSurfaces)(dpy, VA_RT_FORMAT_YUV420, w, h, surfaces.as_mut_ptr(), 3, &mut surf_attrib, 1) != VA_STATUS_SUCCESS {
                let _ = (api.vaDestroyConfig)(dpy, config_id);
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("vaCreateSurfaces NV12 failed".into()));
            }

            let mut ctx_id = 0u32;
            if (api.vaCreateContext)(dpy, config_id, w as c_int, h as c_int, 1, surfaces.as_mut_ptr(), 3, &mut ctx_id) != VA_STATUS_SUCCESS {
                let _ = (api.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), 3);
                let _ = (api.vaDestroyConfig)(dpy, config_id);
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("vaCreateContext failed".into()));
            }

            let coded_size = (w * h * 2).max(1024 * 1024);
            let mut coded_buf = 0u32;
            if (api.vaCreateBuffer)(dpy, ctx_id, VA_ENC_CODED_BUFFER_TYPE, coded_size, 1, std::ptr::null_mut(), &mut coded_buf) != VA_STATUS_SUCCESS {
                let _ = (api.vaDestroyContext)(dpy, ctx_id);
                let _ = (api.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), 3);
                let _ = (api.vaDestroyConfig)(dpy, config_id);
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("vaCreateBuffer coded_buf failed".into()));
            }

            let mut nv12_fmt = VAImageFormat {
                fourcc: VA_FOURCC_NV12,
                byte_order: 1, // VA_LSB_FIRST
                bits_per_pixel: 12,
                ..VAImageFormat::default()
            };
            let mut staging_img = VAImage::default();
            if (api.vaCreateImage)(dpy, &mut nv12_fmt, w as c_int, h as c_int, &mut staging_img) != VA_STATUS_SUCCESS {
                let _ = (api.vaDestroyBuffer)(dpy, coded_buf);
                let _ = (api.vaDestroyContext)(dpy, ctx_id);
                let _ = (api.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), 3);
                let _ = (api.vaDestroyConfig)(dpy, config_id);
                let _ = (api.vaTerminate)(dpy);
                return Err(VexrecError::Encoding("vaCreateImage NV12 failed".into()));
            }

            let (sps_annexb, pps_annexb) = build_h264_sps_pps_vaapi(w, h, fps, profile_idc, 42);
            let (sps_opt, pps_opt) = extract_h264_sps_pps(&[sps_annexb.clone(), pps_annexb.clone()].concat());

            info!(w, h, fps, node = %node.display(), "Initialized zero-GStreamer VA-API DRM hardware H.264 encoder");

            Ok(Self {
                api,
                _drm_file: drm_file,
                dpy,
                config_id,
                ctx_id,
                surfaces,
                coded_buf,
                staging_img,
                width: w,
                height: h,
                fps,
                bitrate,
                frame_count: 0,
                idr_count: 0,
                idr_period: (fps * 2) as u64,
                packed_headers_supported,
                profile_idc,
                sps_bytes: sps_opt.unwrap_or_default(),
                pps_bytes: pps_opt.unwrap_or_default(),
                y_scratch: vec![0u8; (w * h) as usize],
                uv_scratch: vec![0u8; (w * h / 2) as usize],
            })
        }
    }

    fn upload_nv12_to_surface(&mut self, surf: VASurfaceID, frame: &VideoFrame) -> Result<()> {
        let FrameBacking::Host { stride, ref data } = frame.backing else {
            return Err(VexrecError::Encoding("VA-API host upload expects host buffer".into()));
        };
        bgra_to_nv12_bt709(
            data.as_slice(),
            self.width,
            self.height,
            stride,
            &mut self.y_scratch,
            self.width,
            &mut self.uv_scratch,
            self.width,
        );

        unsafe {
            let mut base_ptr: *mut c_void = std::ptr::null_mut();
            if (self.api.vaMapBuffer)(self.dpy, self.staging_img.buf, &mut base_ptr) != VA_STATUS_SUCCESS {
                return Err(VexrecError::Encoding("vaMapBuffer failed on NV12 staging VAImage".into()));
            }

            let dst_base = base_ptr.cast::<u8>();
            let max_bytes = self.staging_img.data_size as usize;
            let y_off = self.staging_img.offsets[0] as usize;
            let y_pitch = (self.staging_img.pitches[0] as usize).max(self.width as usize);
            let uv_off = self.staging_img.offsets[1] as usize;
            let uv_pitch = (self.staging_img.pitches[1] as usize).max(self.width as usize);
            let w = self.width as usize;
            let h = self.height as usize;

            for row in 0..h {
                let d_off = y_off + row * y_pitch;
                if d_off + w <= max_bytes {
                    std::ptr::copy_nonoverlapping(
                        self.y_scratch.as_ptr().add(row * w),
                        dst_base.add(d_off),
                        w,
                    );
                }
            }
            for row in 0..h / 2 {
                let d_off = uv_off + row * uv_pitch;
                if d_off + w <= max_bytes {
                    std::ptr::copy_nonoverlapping(
                        self.uv_scratch.as_ptr().add(row * w),
                        dst_base.add(d_off),
                        w,
                    );
                }
            }

            let _ = (self.api.vaUnmapBuffer)(self.dpy, self.staging_img.buf);
            let rc_put = (self.api.vaPutImage)(
                self.dpy,
                surf,
                self.staging_img.image_id,
                0,
                0,
                self.width,
                self.height,
                0,
                0,
                self.width,
                self.height,
            );
            if rc_put != VA_STATUS_SUCCESS {
                return Err(VexrecError::Encoding(format!("vaPutImage NV12 failed: {rc_put}")));
            }
        }
        Ok(())
    }
}

impl Drop for VaApiH264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.api.vaDestroyImage)(self.dpy, self.staging_img.image_id);
            let _ = (self.api.vaDestroyBuffer)(self.dpy, self.coded_buf);
            let _ = (self.api.vaDestroyContext)(self.dpy, self.ctx_id);
            let _ = (self.api.vaDestroySurfaces)(self.dpy, self.surfaces.as_mut_ptr(), 3);
            let _ = (self.api.vaDestroyConfig)(self.dpy, self.config_id);
            let _ = (self.api.vaTerminate)(self.dpy);
        }
    }
}

impl VideoEncoder for VaApiH264Encoder {
    fn backend_name(&self) -> &'static str {
        "VA-API DRM (Hardware H.264)"
    }

    fn sps_pps(&self) -> Option<(&[u8], &[u8])> {
        if !self.sps_bytes.is_empty() && !self.pps_bytes.is_empty() {
            Some((&self.sps_bytes, &self.pps_bytes))
        } else {
            None
        }
    }

    fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedVideoPacket>> {
        let cur_idx = (self.frame_count % 2) as usize;
        let ref_idx = ((self.frame_count + 1) % 2) as usize;
        let in_surf = self.surfaces[0];
        let recon_surf = self.surfaces[1 + cur_idx];
        let ref_recon_surf = self.surfaces[1 + ref_idx];
        let is_idr = force_keyframe || (self.frame_count % self.idr_period == 0);

        self.upload_nv12_to_surface(in_surf, frame)?;

        let mb_w = ((self.width + 15) / 16) as u16;
        let mb_h = ((self.height + 15) / 16) as u16;
        let gop_pos = if force_keyframe { 0 } else { self.frame_count % self.idr_period };
        let frame_num = (gop_pos % 256) as u16;
        let poc = ((gop_pos % 128) * 2) as i32;

        let mut bufs_to_submit: Vec<VABufferID> = Vec::with_capacity(8);

        unsafe {
            if is_idr {
                let mut seq = std::mem::zeroed::<VAEncSequenceParameterBufferH264>();
                seq.seq_parameter_set_id = 0;
                seq.level_idc = 42;
                seq.intra_period = self.idr_period as u32;
                seq.intra_idr_period = self.idr_period as u32;
                seq.ip_period = 1;
                seq.bits_per_second = self.bitrate;
                seq.max_num_ref_frames = 1;
                seq.picture_width_in_mbs = mb_w;
                seq.picture_height_in_mbs = mb_h;
                seq.seq_fields = 1 | (1 << 2) | (1 << 5) | (4 << 6) | (4 << 12);
                let crop_r = (mb_w as u32 * 16 - self.width) / 2;
                let crop_b = (mb_h as u32 * 16 - self.height) / 2;
                if crop_r > 0 || crop_b > 0 {
                    seq.frame_cropping_flag = 1;
                    seq.frame_crop_right_offset = crop_r;
                    seq.frame_crop_bottom_offset = crop_b;
                }
                seq.vui_parameters_present_flag = 1;
                seq.vui_fields = (1 << 1) | (1 << 2) | (11 << 3) | (11 << 8) | (1 << 13) | (1 << 15);
                seq.num_units_in_tick = 1;
                seq.time_scale = self.fps * 2;

                let mut seq_id = 0u32;
                (self.api.vaCreateBuffer)(
                    self.dpy,
                    self.ctx_id,
                    VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE,
                    std::mem::size_of::<VAEncSequenceParameterBufferH264>() as u32,
                    1,
                    (&mut seq as *mut VAEncSequenceParameterBufferH264).cast(),
                    &mut seq_id,
                );
                bufs_to_submit.push(seq_id);

                let mut rc = std::mem::zeroed::<VAEncMiscParameterRateControlWrapper>();
                rc.type_ = VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL;
                rc.bits_per_second = self.bitrate;
                rc.target_percentage = 90;
                rc.window_size = 1000;
                rc.initial_qp = 22;
                rc.min_qp = 12;
                rc.max_qp = 40;
                let mut rc_id = 0u32;
                (self.api.vaCreateBuffer)(
                    self.dpy,
                    self.ctx_id,
                    VA_ENC_MISC_PARAMETER_BUFFER_TYPE,
                    std::mem::size_of::<VAEncMiscParameterRateControlWrapper>() as u32,
                    1,
                    (&mut rc as *mut VAEncMiscParameterRateControlWrapper).cast(),
                    &mut rc_id,
                );
                bufs_to_submit.push(rc_id);

                if (self.packed_headers_supported & VA_ENC_PACKED_HEADER_SEQUENCE) != 0 {
                    let (sps_annexb, pps_annexb) = build_h264_sps_pps_vaapi(
                        self.width,
                        self.height,
                        self.fps,
                        self.profile_idc,
                        42,
                    );
                    let mut seq_hdr_data = [sps_annexb, pps_annexb].concat();
                    let mut ph_s = std::mem::zeroed::<VAEncPackedHeaderParameterBuffer>();
                    ph_s.type_ = VA_ENC_PACKED_HEADER_SEQUENCE;
                    ph_s.bit_length = (seq_hdr_data.len() * 8) as u32;
                    ph_s.has_emulation_bytes = 1;
                    let (mut ph_id, mut pd_id) = (0u32, 0u32);
                    (self.api.vaCreateBuffer)(
                        self.dpy,
                        self.ctx_id,
                        VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE,
                        std::mem::size_of::<VAEncPackedHeaderParameterBuffer>() as u32,
                        1,
                        (&mut ph_s as *mut VAEncPackedHeaderParameterBuffer).cast(),
                        &mut ph_id,
                    );
                    (self.api.vaCreateBuffer)(
                        self.dpy,
                        self.ctx_id,
                        VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
                        seq_hdr_data.len() as u32,
                        1,
                        seq_hdr_data.as_mut_ptr().cast(),
                        &mut pd_id,
                    );
                    bufs_to_submit.push(ph_id);
                    bufs_to_submit.push(pd_id);
                }
            }

            let cur_idr_id = self.idr_count;
            let mut pic = std::mem::zeroed::<VAEncPictureParameterBufferH264>();
            pic.curr_pic = VAPictureH264 {
                picture_id: recon_surf,
                frame_idx: frame_num as u32,
                flags: 0,
                top_field_order_cnt: poc,
                bottom_field_order_cnt: poc,
                va_reserved: [0; 4],
            };
            for r in &mut pic.reference_frames {
                *r = VAPictureH264::default();
            }
            if !is_idr {
                pic.reference_frames[0] = VAPictureH264 {
                    picture_id: ref_recon_surf,
                    frame_idx: frame_num.wrapping_sub(1) as u32,
                    flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                    top_field_order_cnt: poc - 2,
                    bottom_field_order_cnt: poc - 2,
                    va_reserved: [0; 4],
                };
            }
            pic.coded_buf = self.coded_buf;
            pic.frame_num = frame_num;
            pic.pic_init_qp = 26;
            let cabac_bool = self.profile_idc >= 77;
            let cabac = u32::from(cabac_bool);
            // Bit 0: idr_pic_flag | Bits 1..=2: reference_pic_flag (1) | Bit 3: entropy_coding_mode_flag | Bit 9: deblocking_filter_control_present_flag
            pic.pic_fields = u32::from(is_idr) | (1 << 1) | (cabac << 3) | (1 << 9);

            let mut pic_id = 0u32;
            (self.api.vaCreateBuffer)(
                self.dpy,
                self.ctx_id,
                VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VAEncPictureParameterBufferH264>() as u32,
                1,
                (&mut pic as *mut VAEncPictureParameterBufferH264).cast(),
                &mut pic_id,
            );
            bufs_to_submit.push(pic_id);

            let mut slice = std::mem::zeroed::<VAEncSliceParameterBufferH264>();
            slice.num_macroblocks = (mb_w as u32) * (mb_h as u32);
            slice.macroblock_info = VA_INVALID_ID;
            slice.slice_type = if is_idr { 2 } else { 0 }; // 2 = I-slice, 0 = P-slice
            slice.idr_pic_id = if is_idr { cur_idr_id } else { 0 };
            slice.pic_order_cnt_lsb = poc as u16;
            for r in &mut slice.ref_pic_list_0 {
                *r = VAPictureH264::default();
            }
            for r in &mut slice.ref_pic_list_1 {
                *r = VAPictureH264::default();
            }
            if !is_idr {
                slice.ref_pic_list_0[0] = pic.reference_frames[0];
            }
            let mut slice_id = 0u32;
            (self.api.vaCreateBuffer)(
                self.dpy,
                self.ctx_id,
                VA_ENC_SLICE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VAEncSliceParameterBufferH264>() as u32,
                1,
                (&mut slice as *mut VAEncSliceParameterBufferH264).cast(),
                &mut slice_id,
            );
            bufs_to_submit.push(slice_id);

            if (self.packed_headers_supported & VA_ENC_PACKED_HEADER_SLICE) != 0 {
                let (mut sl_bytes, sl_bits) = build_h264_slice_header_vaapi(
                    is_idr,
                    frame_num,
                    poc,
                    cur_idr_id,
                    cabac_bool,
                );
                let mut ph_sl = std::mem::zeroed::<VAEncPackedHeaderParameterBuffer>();
                ph_sl.type_ = VA_ENC_PACKED_HEADER_SLICE;
                ph_sl.bit_length = sl_bits;
                ph_sl.has_emulation_bytes = 1;
                let (mut sh_param_id, mut sh_data_id) = (0u32, 0u32);
                (self.api.vaCreateBuffer)(
                    self.dpy,
                    self.ctx_id,
                    VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE,
                    std::mem::size_of::<VAEncPackedHeaderParameterBuffer>() as u32,
                    1,
                    (&mut ph_sl as *mut VAEncPackedHeaderParameterBuffer).cast(),
                    &mut sh_param_id,
                );
                (self.api.vaCreateBuffer)(
                    self.dpy,
                    self.ctx_id,
                    VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
                    sl_bytes.len() as u32,
                    1,
                    sl_bytes.as_mut_ptr().cast(),
                    &mut sh_data_id,
                );
                bufs_to_submit.push(sh_param_id);
                bufs_to_submit.push(sh_data_id);
            }

            let rc_b = (self.api.vaBeginPicture)(self.dpy, self.ctx_id, in_surf);
            let rc_r = if rc_b == VA_STATUS_SUCCESS {
                (self.api.vaRenderPicture)(
                    self.dpy,
                    self.ctx_id,
                    bufs_to_submit.as_mut_ptr(),
                    bufs_to_submit.len() as c_int,
                )
            } else {
                rc_b
            };
            let rc_e = if rc_b == VA_STATUS_SUCCESS && rc_r == VA_STATUS_SUCCESS {
                (self.api.vaEndPicture)(self.dpy, self.ctx_id)
            } else {
                rc_r
            };
            let rc_s = if rc_e == VA_STATUS_SUCCESS {
                (self.api.vaSyncSurface)(self.dpy, in_surf)
            } else {
                rc_e
            };

            for bid in bufs_to_submit {
                let _ = (self.api.vaDestroyBuffer)(self.dpy, bid);
            }

            if rc_b != VA_STATUS_SUCCESS || rc_r != VA_STATUS_SUCCESS || rc_e != VA_STATUS_SUCCESS || rc_s != VA_STATUS_SUCCESS {
                debug!(rc_b, rc_r, rc_e, rc_s, "VA-API encode step returned non-success");
                return Err(VexrecError::Encoding(format!(
                    "VA-API driver picture submission failed (b={rc_b}, r={rc_r}, e={rc_e}, s={rc_s})"
                )));
            }

            let mut seg_ptr: *mut c_void = std::ptr::null_mut();
            if (self.api.vaMapBuffer)(self.dpy, self.coded_buf, &mut seg_ptr) != VA_STATUS_SUCCESS {
                return Err(VexrecError::Encoding("vaMapBuffer coded_buf failed".into()));
            }

            let mut out_bytes = Vec::new();
            let mut cur = seg_ptr.cast::<VACodedBufferSegment>();
            while !cur.is_null() {
                let s = &*cur;
                if !s.buf.is_null() && s.size > 0 {
                    out_bytes.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                cur = s.next;
            }
            let _ = (self.api.vaUnmapBuffer)(self.dpy, self.coded_buf);

            self.frame_count += 1;
            if is_idr {
                self.idr_count = self.idr_count.wrapping_add(1);
            }
            if out_bytes.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![EncodedVideoPacket {
                pts_ns: frame.pts_ns,
                is_keyframe: is_idr,
                data: out_bytes,
            }])
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedVideoPacket>> {
        Ok(Vec::new())
    }
}
