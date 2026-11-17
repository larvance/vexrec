pub mod dbus;
pub mod drm;
pub mod fonts;
pub mod ipc;
pub mod png_enc;
pub mod shm;
pub mod simd;
pub mod tray;

pub use dbus::{
    ensure_portal_screenshot_permission, kwin_capture_workspace_pipe,
    open_path_via_portal_or_mime, portal_choose_folder, portal_save_file, portal_screencast,
    portal_screenshot, spawn_portal_global_shortcuts, NotificationHub, PortalScreenCastStream,
};
pub use drm::{display_render_node, enumerate_drm_devices, DrmDeviceInfo};
pub use fonts::{FontEngine, FontRole};
pub use ipc::{
    bind_daemon_listener, encode_frame_header, primary_socket_path, read_incoming_command,
    send_fast_opcode, IpcOpCode,
};
pub use shm::ShmBuffer;
pub use tray::{format_timer_text, format_tray_timer_svg, TrayAction, TrayIndicator};
