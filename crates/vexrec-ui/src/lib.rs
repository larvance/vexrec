pub mod daemon;
pub mod frame;
pub mod indicator;
pub mod overlay;
pub mod settings;
pub mod state;

pub use daemon::{build_output_path, render_filename, DaemonApp};
pub use frame::FrameController;
pub use indicator::IndicatorController;
pub use overlay::{FreezeOverlayController, OverlayOutcome, ZOOM_DURATION_MS};
pub use settings::{SettingsControlId, SettingsController, SettingsOutcome, SETTINGS_H, SETTINGS_W};
pub use state::{
    apply_drag_delta, compute_window_mosaic, compute_zoom_target_rect, ease_out_cubic_rect,
    extract_window_crop_or_fallback, hit_test_selection_handle, ActionMode, DragHandle,
    PillButtonId, PillButtonRect, SelectionAnchor, TargetMode, WindowCardLayout,
};

use vexrec_core::error::Result;

/// Starts and runs the pure-Rust epoll-driven Vexrec daemon until termination.
pub fn run_daemon() -> Result<()> {
    let mut app = DaemonApp::initialize()?;
    app.run()
}
