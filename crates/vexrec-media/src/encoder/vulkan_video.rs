//! Vulkan Video hardware capability detector (via pure-Rust `ash::vk` dynamically loading `libvulkan.so.1`).
//! Strictly isolates probing to display-connected GPUs or non-suspended devices so D3cold discrete GPUs
//! remain asleep during standard screenshot and Wayland preview operations.

use ash::vk;
use vexrec_os::drm::display_render_node;

#[derive(Debug, Clone)]
pub struct VulkanVideoCaps {
    pub device_name: String,
    pub has_encode_queue: bool,
    pub has_h264_encode: bool,
    pub has_h265_encode: bool,
}

/// Safely probes Vulkan 1.3+ Video Encode extensions on the active non-suspended GPU.
pub fn probe_vulkan_video() -> Option<VulkanVideoCaps> {
    let entry = unsafe { ash::Entry::load().ok()? };
    let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let inst_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = unsafe { entry.create_instance(&inst_info, None).ok()? };

    let phys_devs = unsafe { instance.enumerate_physical_devices().unwrap_or_default() };
    let display_node = display_render_node();

    let mut best: Option<VulkanVideoCaps> = None;
    for pdev in phys_devs {
        let props = unsafe { instance.get_physical_device_properties(pdev) };
        if props.device_type == vk::PhysicalDeviceType::CPU {
            continue;
        }
        // On Optimus laptops, skip discrete GPU if display DRM vendor differs and it's asleep
        if let Some(ref disp) = display_node {
            if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU && props.vendor_id != disp.vendor_id {
                continue;
            }
        }

        let name = unsafe {
            std::ffi::CStr::from_ptr(props.device_name.as_ptr())
                .to_string_lossy()
                .into_owned()
        };
        let queues = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        let has_encode_queue = queues
            .iter()
            .any(|q| q.queue_flags.contains(vk::QueueFlags::VIDEO_ENCODE_KHR));

        let exts = unsafe {
            instance
                .enumerate_device_extension_properties(pdev)
                .unwrap_or_default()
        };
        let has_ext = |target: &std::ffi::CStr| {
            exts.iter().any(|e| {
                let n = unsafe { std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) };
                n == target
            })
        };

        let has_h264 = has_ext(vk::KHR_VIDEO_ENCODE_H264_NAME);
        let has_h265 = has_ext(vk::KHR_VIDEO_ENCODE_H265_NAME);

        if has_encode_queue || has_h264 || has_h265 {
            best = Some(VulkanVideoCaps {
                device_name: name,
                has_encode_queue,
                has_h264_encode: has_h264,
                has_h265_encode: has_h265,
            });
            break;
        }
        if best.is_none() {
            best = Some(VulkanVideoCaps {
                device_name: name,
                has_encode_queue,
                has_h264_encode: has_h264,
                has_h265_encode: has_h265,
            });
        }
    }

    unsafe {
        instance.destroy_instance(None);
    }
    best
}
