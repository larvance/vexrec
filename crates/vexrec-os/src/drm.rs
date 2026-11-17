use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DrmDeviceInfo {
    pub card_path: PathBuf,
    pub render_path: Option<PathBuf>,
    pub vendor_id: u32,
    pub driver: String,
    pub is_boot_vga: bool,
    pub has_connected_output: bool,
    pub runtime_suspended: bool,
}

impl DrmDeviceInfo {
    pub const VENDOR_AMD: u32 = 0x1002;
    pub const VENDOR_INTEL: u32 = 0x8086;
    pub const VENDOR_NVIDIA: u32 = 0x10de;
}

/// Probes `/sys/class/drm` via sysfs ONLY (never calls open() on render nodes!) so sleeping
/// discrete GPUs in RTD3 D3cold are NEVER woken up during screenshot discovery or idle checks.
pub fn enumerate_drm_devices() -> Vec<DrmDeviceInfo> {
    let mut devices = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return devices;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev_dir = entry.path().join("device");
        let vendor_str = fs::read_to_string(dev_dir.join("vendor")).unwrap_or_default();
        let vendor_id = u32::from_str_radix(
            vendor_str.trim().trim_start_matches("0x"),
            16,
        )
        .unwrap_or(0);

        let is_boot_vga = fs::read_to_string(dev_dir.join("boot_vga"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);

        let driver = fs::read_link(dev_dir.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_default();

        let runtime_suspended = fs::read_to_string(dev_dir.join("power/runtime_status"))
            .map(|s| s.trim() == "suspended")
            .unwrap_or(false);

        let has_connected_output = fs::read_dir("/sys/class/drm")
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|conn| {
                conn.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}-"))
            })
            .any(|conn| {
                fs::read_to_string(conn.path().join("status"))
                    .map(|s| s.trim() == "connected")
                    .unwrap_or(false)
            });

        // Resolve matching renderD* node under device/drm/
        let render_path = fs::read_dir(dev_dir.join("drm"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|n| n.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("renderD"))
            .map(|n| PathBuf::from("/dev/dri").join(n));

        devices.push(DrmDeviceInfo {
            card_path: PathBuf::from("/dev/dri").join(name),
            render_path,
            vendor_id,
            driver,
            is_boot_vga,
            has_connected_output,
            runtime_suspended,
        });
    }

    devices.sort_by_key(|d| (!d.has_connected_output, !d.is_boot_vga));
    devices
}

/// Returns the primary display-connected render node (e.g. AMD iGPU `/dev/dri/renderD128` on laptops),
/// ensuring discrete NVIDIA GPUs remain undisturbed in D3cold sleep during desktop/UI operations.
pub fn display_render_node() -> Option<DrmDeviceInfo> {
    let devs = enumerate_drm_devices();
    devs.iter()
        .find(|d| d.has_connected_output && d.render_path.is_some())
        .or_else(|| devs.iter().find(|d| d.is_boot_vga && d.render_path.is_some()))
        .or_else(|| devs.iter().find(|d| !d.runtime_suspended && d.render_path.is_some()))
        .cloned()
}
