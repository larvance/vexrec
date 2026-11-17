use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Arc;

use vexrec_core::buffer::{AlignedVec, PixelFormat, PixelImage};
use vexrec_core::config::VexrecConfig;
use vexrec_core::error::Result;
use vexrec_core::formats::{ContainerFormat, EncoderBackendPreference, ImageFormat, VideoQuality};
use vexrec_core::geometry::{Rect, Size};
use vexrec_display::{
    ClientWindowInfo, CursorKind, DisplayBackend, DisplayEvent, KeyAction, MonitorInfo,
    SessionKind, WindowRole,
};
use vexrec_os::fonts::FontEngine;
use vexrec_ui::{
    compute_window_mosaic, ease_out_cubic_rect, hit_test_selection_handle, ActionMode, DragHandle,
    FrameController, FreezeOverlayController, IndicatorController, OverlayOutcome, PillButtonId,
    SettingsControlId, SettingsController, SettingsOutcome, TargetMode,
};

struct MockBackend {
    screen_w: u32,
    screen_h: u32,
    visible: HashMap<WindowRole, Rect>,
    cursors: HashMap<WindowRole, CursorKind>,
    surfaces: HashMap<WindowRole, Vec<u8>>,
}

impl MockBackend {
    fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            screen_w,
            screen_h,
            visible: HashMap::new(),
            cursors: HashMap::new(),
            surfaces: HashMap::new(),
        }
    }

    fn pixel_bgra(&self, role: WindowRole, x: i32, y: i32) -> Option<[u8; 4]> {
        let geom = self.visible.get(&role)?;
        let buf = self.surfaces.get(&role)?;
        if x < 0 || y < 0 || x as u32 >= geom.width || y as u32 >= geom.height {
            return None;
        }
        let idx = ((y as u32 * geom.width + x as u32) * 4) as usize;
        Some([buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]])
    }
}

impl DisplayBackend for MockBackend {
    fn session_kind(&self) -> SessionKind {
        SessionKind::X11
    }

    fn primary_size(&self) -> Size {
        Size::new(self.screen_w, self.screen_h)
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            name: "MOCK-0".into(),
            rect: Rect::new(0, 0, self.screen_w, self.screen_h),
            primary: true,
            scale_factor: 1.0,
        }]
    }

    fn raw_fd(&self) -> RawFd {
        -1
    }

    fn capture_screen(&mut self, crop: Option<Rect>, _include_cursor: bool) -> Result<PixelImage> {
        let rect = crop.unwrap_or_else(|| Rect::new(0, 0, self.screen_w, self.screen_h));
        Ok(solid_bgra_image(rect.width, rect.height, [32, 32, 36, 255]))
    }

    fn enumerate_windows(&mut self, _capture_previews: bool) -> Vec<ClientWindowInfo> {
        Vec::new()
    }

    fn capture_window(&mut self, _window_id: u64) -> Result<PixelImage> {
        Ok(solid_bgra_image(320, 200, [48, 84, 140, 255]))
    }

    fn show_surface(&mut self, role: WindowRole, geom: Rect) {
        self.visible.insert(role, geom);
    }

    fn hide_surface(&mut self, role: WindowRole) {
        self.visible.remove(&role);
    }

    fn is_surface_visible(&self, role: WindowRole) -> bool {
        self.visible.contains_key(&role)
    }

    fn present_surface_bgra(
        &mut self,
        role: WindowRole,
        width: u32,
        height: u32,
        stride: u32,
        bgra: &[u8],
        _dirty: Option<Rect>,
    ) -> Result<()> {
        let mut tight = vec![0u8; (width * height * 4) as usize];
        let row_bytes = (width * 4) as usize;
        for y in 0..height as usize {
            let s = y * stride as usize;
            let d = y * row_bytes;
            tight[d..d + row_bytes].copy_from_slice(&bgra[s..s + row_bytes]);
        }
        self.surfaces.insert(role, tight);
        Ok(())
    }

    fn set_cursor(&mut self, role: WindowRole, cursor: CursorKind) {
        self.cursors.insert(role, cursor);
    }

    fn grab_overlay_keyboard(&mut self) -> bool {
        true
    }

    fn ungrab_overlay_keyboard(&mut self) {}

    fn register_global_hotkeys(&mut self) {}

    fn set_clipboard_image(&mut self, _img: PixelImage) -> Result<()> {
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<DisplayEvent> {
        Vec::new()
    }

    fn sync_and_wait_frame(&mut self) {}
}

fn solid_bgra_image(w: u32, h: u32, bgra: [u8; 4]) -> PixelImage {
    let stride = w * 4;
    let mut data = AlignedVec::zeroed((stride * h) as usize);
    for px in data.as_mut_slice().chunks_exact_mut(4) {
        px.copy_from_slice(&bgra);
    }
    PixelImage {
        width: w,
        height: h,
        stride,
        format: PixelFormat::Bgra8,
        data,
    }
}

fn mock_windows() -> Vec<ClientWindowInfo> {
    vec![
        ClientWindowInfo {
            id: 101,
            title: "Terminal".into(),
            app_id: "alacritty".into(),
            rect: Rect::new(40, 40, 640, 420),
            icon: None,
            preview: Some(solid_bgra_image(640, 420, [20, 40, 90, 255])),
        },
        ClientWindowInfo {
            id: 102,
            title: "Browser".into(),
            app_id: "firefox".into(),
            rect: Rect::new(720, 60, 500, 500),
            icon: None,
            preview: Some(solid_bgra_image(500, 500, [90, 45, 25, 255])),
        },
        ClientWindowInfo {
            id: 103,
            title: "Editor".into(),
            app_id: "hx".into(),
            rect: Rect::new(200, 320, 480, 300),
            icon: None,
            preview: Some(solid_bgra_image(480, 300, [40, 90, 40, 255])),
        },
    ]
}

fn click_pill_button(
    overlay: &mut FreezeOverlayController,
    backend: &mut MockBackend,
    id: PillButtonId,
) -> OverlayOutcome {
    let btn = overlay
        .pill_buttons()
        .iter()
        .find(|b| b.id == id)
        .copied()
        .expect("missing pill button");
    let cx = btn.rect.x as f64 + btn.rect.width as f64 / 2.0;
    let cy = btn.rect.y as f64 + btn.rect.height as f64 / 2.0;
    let out = overlay.on_pointer_button(backend, 1, true, cx, cy);
    let _ = overlay.on_pointer_button(backend, 1, false, cx, cy);
    out
}

#[test]
fn test_overlay_defaults_to_screenshot_and_done_button_no_white_border() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1280, 720);
    let mut cfg = VexrecConfig::default();
    cfg.screenshot.last_mode = Some("selection".into());
    cfg.screenshot.last_region = Some(Rect::new(120, 120, 400, 260));

    let mut overlay = FreezeOverlayController::new(fonts);
    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1280, 720, [24, 24, 28, 255]),
        mock_windows(),
    );
    assert_eq!(overlay.action_mode(), ActionMode::Screenshot);
    assert_eq!(overlay.target_mode(), TargetMode::Selection);

    // Switch to Record, close, and re-open: ActionMode must ALWAYS reset to Screenshot
    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeRecord);
    assert_eq!(overlay.action_mode(), ActionMode::Record);
    overlay.hide(&mut backend);

    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1280, 720, [24, 24, 28, 255]),
        mock_windows(),
    );
    assert_eq!(
        overlay.action_mode(),
        ActionMode::Screenshot,
        "ActionMode must reset to Screenshot on every open"
    );

    // Done button: solid white inside, NO white outer border 2px above button
    let done_btn = overlay
        .pill_buttons()
        .iter()
        .find(|b| b.id == PillButtonId::DoneOrRecord)
        .copied()
        .expect("Done button missing");
    let inside = backend
        .pixel_bgra(
            WindowRole::OverlayFullscreen,
            done_btn.rect.x + 6,
            done_btn.rect.y + 6,
        )
        .unwrap();
    assert_eq!(inside, [255, 255, 255, 255], "Done fill should be solid white");

    let outside_top = backend
        .pixel_bgra(
            WindowRole::OverlayFullscreen,
            done_btn.rect.x + done_btn.rect.width as i32 / 2,
            done_btn.rect.y - 2,
        )
        .unwrap();
    assert!(
        outside_top[0] < 80 && outside_top[1] < 80 && outside_top[2] < 80,
        "Done button must not draw a white outer border (got {:?})",
        outside_top
    );
}

#[test]
fn test_corners_set_pointer_cursor_and_fullscreen_has_no_box() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1280, 720);
    let mut cfg = VexrecConfig::default();
    cfg.screenshot.last_mode = Some("selection".into());
    cfg.screenshot.last_region = Some(Rect::new(200, 180, 300, 200));

    let sel = Rect::new(200, 180, 300, 200);
    assert_eq!(hit_test_selection_handle(sel, 200.0, 180.0), DragHandle::TopLeft);
    assert_eq!(hit_test_selection_handle(sel, 500.0, 180.0), DragHandle::TopRight);
    assert_eq!(hit_test_selection_handle(sel, 200.0, 380.0), DragHandle::BottomLeft);
    assert_eq!(hit_test_selection_handle(sel, 500.0, 380.0), DragHandle::BottomRight);
    assert_eq!(hit_test_selection_handle(sel, 350.0, 280.0), DragHandle::Body);

    let mut overlay = FreezeOverlayController::new(Arc::clone(&fonts));
    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1280, 720, [20, 40, 60, 255]),
        mock_windows(),
    );

    // Hovering TopLeft corner sets CursorKind::Pointer
    overlay.on_pointer_motion(&mut backend, 200.0, 180.0);
    assert_eq!(
        backend.cursors.get(&WindowRole::OverlayFullscreen).copied(),
        Some(CursorKind::Pointer)
    );

    // Switch to Fullscreen: desktop background must have NO white selection box around screen edges
    click_pill_button(&mut overlay, &mut backend, PillButtonId::TargetFullscreen);
    assert_eq!(overlay.target_mode(), TargetMode::Fullscreen);

    // Sample pixel near top-left edge (0,0) and (2,2): should match desktop [20, 40, 60, 255], no box!
    let top_edge = backend.pixel_bgra(WindowRole::OverlayFullscreen, 0, 0).unwrap();
    assert_eq!(top_edge, [20, 40, 60, 255]);

    // Switch to Record > Fullscreen -> StartRecording { crop: None }
    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeRecord);
    match click_pill_button(&mut overlay, &mut backend, PillButtonId::DoneOrRecord) {
        OverlayOutcome::StartRecording { crop, .. } => assert!(crop.is_none()),
        other => panic!("Expected StartRecording Fullscreen, got {:?}", other),
    }

    // FrameController MUST stay hidden for Fullscreen recording even if show_recording_frame=true
    let mut frame = FrameController::new(fonts);
    frame.show(&mut backend, None, true);
    assert!(!backend.is_surface_visible(WindowRole::RecordingFrame));

    // And for Selection recording when enabled=true, it sits 4px outside crop
    let crop = Rect::new(100, 100, 300, 200);
    frame.show(&mut backend, Some(crop), true);
    assert_eq!(
        backend.visible.get(&WindowRole::RecordingFrame).copied(),
        Some(Rect::new(96, 96, 308, 208))
    );
}

#[test]
fn test_window_mosaic_and_record_window_and_window_region_zoom() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1280, 720);
    let windows = mock_windows();

    let layouts = compute_window_mosaic(&windows, 1280, 720);
    assert_eq!(layouts.len(), 3);
    for i in 0..layouts.len() {
        assert!(layouts[i].card_rect.x >= 0 && layouts[i].card_rect.y >= 0);
        assert!(layouts[i].card_rect.y + layouts[i].card_rect.height as i32 <= 720 - 115);
        for j in (i + 1)..layouts.len() {
            let a = layouts[i].card_rect;
            let b = layouts[j].card_rect;
            let overlap_x = a.x < b.x + b.width as i32 && a.x + a.width as i32 > b.x;
            let overlap_y = a.y < b.y + b.height as i32 && a.y + a.height as i32 > b.y;
            assert!(!(overlap_x && overlap_y), "Window cards overlapped!");
        }
    }

    let cfg = VexrecConfig::default();
    let mut overlay = FreezeOverlayController::new(fonts);
    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1280, 720, [24, 24, 28, 255]),
        windows.clone(),
    );

    // Record > Window: clicking first card starts recording that window rect instead of screenshotting!
    click_pill_button(&mut overlay, &mut backend, PillButtonId::TargetWindow);
    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeRecord);
    let card0 = layouts[0].card_rect;
    let cx0 = card0.x as f64 + card0.width as f64 / 2.0;
    let cy0 = card0.y as f64 + card0.height as f64 / 2.0;
    let card_out = overlay.on_pointer_button(&mut backend, 1, true, cx0, cy0);
    let _ = overlay.on_pointer_button(&mut backend, 1, false, cx0, cy0);
    match card_out {
        OverlayOutcome::StartRecording { crop, .. } => {
            assert_eq!(crop, Some(windows[0].rect));
        }
        other => panic!("Record > Window should start recording, got {:?}", other),
    }

    // Window Region zoom interpolation + Escape backs out of zoom
    let from = Rect::new(100, 100, 300, 200);
    let to = Rect::new(140, 40, 1000, 620);
    assert_eq!(ease_out_cubic_rect(from, to, 0.0), from);
    assert_eq!(ease_out_cubic_rect(from, to, 1.0), to);
    let mid = ease_out_cubic_rect(from, to, 0.5);
    assert!(mid.width > from.width && mid.width < to.width);

    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeScreenshot);
    click_pill_button(&mut overlay, &mut backend, PillButtonId::TargetWindowRegion);
    let _ = overlay.on_pointer_button(&mut backend, 1, true, cx0, cy0);
    let _ = overlay.on_pointer_button(&mut backend, 1, false, cx0, cy0);
    assert!(overlay.is_animating(), "Window Region selection must animate smoothly");

    // Pressing Escape backs out of zoom back to overview (instead of cancelling entire overlay)
    let esc_out = overlay.on_key(&mut backend, KeyAction::Escape);
    assert!(matches!(esc_out, OverlayOutcome::None));
    assert!(overlay.is_visible());
}

#[test]
fn test_settings_controller_and_indicator_pill() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1280, 720);
    let mut cfg = VexrecConfig::default();
    cfg.recording.framerate = 60;
    cfg.recording.show_recording_frame = false;

    let mut settings = SettingsController::new(Arc::clone(&fonts));
    settings.show(&mut backend, &cfg);
    assert!(settings.is_visible());
    assert!(backend.is_surface_visible(WindowRole::SettingsDialog));

    assert!(matches!(
        settings.activate_control(SettingsControlId::CycleFps),
        SettingsOutcome::None
    ));
    assert_eq!(settings.draft_config().recording.framerate, 120);
    assert!(matches!(
        settings.activate_control(SettingsControlId::CycleFps),
        SettingsOutcome::None
    ));
    assert_eq!(settings.draft_config().recording.framerate, 30);

    settings.activate_control(SettingsControlId::CycleContainer);
    assert_eq!(settings.draft_config().recording.container, ContainerFormat::Mkv);

    settings.activate_control(SettingsControlId::CycleEncoder);
    assert_eq!(
        settings.draft_config().recording.encoder_preference,
        EncoderBackendPreference::VaApi
    );

    settings.activate_control(SettingsControlId::CycleQuality);
    assert_eq!(settings.draft_config().recording.quality, VideoQuality::High);

    settings.activate_control(SettingsControlId::CycleScreenshotFormat);
    assert_eq!(settings.draft_config().screenshot.default_format, ImageFormat::Jpeg);

    settings.activate_control(SettingsControlId::ToggleRecordingFrame);
    assert!(settings.draft_config().recording.show_recording_frame);

    assert!(matches!(
        settings.activate_control(SettingsControlId::BrowseScreenshotDir),
        SettingsOutcome::BrowseScreenshotDir
    ));
    settings.set_screenshot_dir(&mut backend, PathBuf::from("/tmp/vexrec_snaps"));

    match settings.activate_control(SettingsControlId::Save) {
        SettingsOutcome::Saved(saved) => {
            assert_eq!(saved.recording.framerate, 30);
            assert_eq!(saved.storage.screenshot_dir, PathBuf::from("/tmp/vexrec_snaps"));
        }
        other => panic!("Expected Saved outcome, got {:?}", other),
    }

    settings.hide(&mut backend);
    assert!(!settings.is_visible());

    // IndicatorController dot transitions (red -> paused yellow)
    let mut ind = IndicatorController::new(fonts);
    ind.show(&mut backend);
    assert!(ind.is_visible());
    let rec_dot = backend.pixel_bgra(WindowRole::IndicatorPill, 18, 14).unwrap();
    assert_eq!(rec_dot, [68, 68, 239, 255], "Expected #ef4444 recording dot in BGRA");

    ind.update(&mut backend, 65, true);
    let pause_dot = backend.pixel_bgra(WindowRole::IndicatorPill, 18, 14).unwrap();
    assert_eq!(pause_dot, [8, 179, 234, 255], "Expected paused yellow dot in BGRA");

    ind.hide(&mut backend);
    assert!(!ind.is_visible());
}

#[test]
fn test_record_mode_hides_frozen_screenshot_and_shows_live_transparent_view() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1280, 720);
    let mut cfg = VexrecConfig::default();
    cfg.screenshot.last_mode = Some("selection".into());
    cfg.screenshot.last_region = Some(Rect::new(200, 150, 320, 220));

    let mut overlay = FreezeOverlayController::new(fonts);
    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1280, 720, [90, 50, 30, 255]),
        mock_windows(),
    );

    // Screenshot mode shows frozen captured desktop pixel inside selection
    let snap_inside = backend.pixel_bgra(WindowRole::OverlayFullscreen, 300, 220).unwrap();
    assert_eq!(snap_inside, [90, 50, 30, 255]);

    // Switching to Record hides frozen screenshot: transparent hole inside selection, translucent scrim outside
    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeRecord);
    assert_eq!(overlay.action_mode(), ActionMode::Record);

    let live_inside = backend.pixel_bgra(WindowRole::OverlayFullscreen, 300, 220).unwrap();
    assert_eq!(
        live_inside,
        [0, 0, 0, 0],
        "Record Selection cutout must be transparent to show live screen"
    );

    let live_outside = backend.pixel_bgra(WindowRole::OverlayFullscreen, 20, 20).unwrap();
    assert_eq!(
        live_outside,
        [0, 0, 0, 115],
        "Record Selection exterior should dim live screen without showing frozen desktop"
    );

    // Switching to Fullscreen in Record mode keeps entire screen transparent (only bottom pill drawn)
    click_pill_button(&mut overlay, &mut backend, PillButtonId::TargetFullscreen);
    let live_fs = backend.pixel_bgra(WindowRole::OverlayFullscreen, 20, 20).unwrap();
    assert_eq!(live_fs, [0, 0, 0, 0]);

    // Switching back to Selection and clicking Record starts recording cleanly
    click_pill_button(&mut overlay, &mut backend, PillButtonId::TargetSelection);
    match click_pill_button(&mut overlay, &mut backend, PillButtonId::DoneOrRecord) {
        OverlayOutcome::StartRecording { crop, .. } => {
            assert_eq!(crop, Some(Rect::new(200, 150, 320, 220)));
        }
        other => panic!("Expected StartRecording with selection crop, got {:?}", other),
    }

    // Switching back from Record to Screenshot restores frozen screenshot cleanly
    click_pill_button(&mut overlay, &mut backend, PillButtonId::ModeScreenshot);
    let restored = backend.pixel_bgra(WindowRole::OverlayFullscreen, 300, 220).unwrap();
    assert_eq!(restored, [90, 50, 30, 255]);
}

#[test]
fn test_hover_over_pill_and_settings_button_text_centers() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut backend = MockBackend::new(1920, 1080);
    let cfg = VexrecConfig::default();

    let mut overlay = FreezeOverlayController::new(fonts.clone());
    overlay.show(
        &mut backend,
        &cfg,
        solid_bgra_image(1920, 1080, [32, 32, 36, 255]),
        mock_windows(),
    );

    // Every pill button must activate CursorKind::Pointer at its exact text center (b.rect center & baseline-4)
    let buttons = overlay.pill_buttons().to_vec();
    assert!(!buttons.is_empty());
    for btn in &buttons {
        let base_corner = backend
            .pixel_bgra(WindowRole::OverlayFullscreen, btn.rect.x + 3, btn.rect.y + 4)
            .unwrap();

        let text_cx = btn.rect.x as f64 + btn.rect.width as f64 * 0.5;
        let text_cy = btn.rect.y as f64 + 14.0;
        overlay.on_pointer_motion(&mut backend, text_cx, text_cy);
        assert_eq!(
            backend.cursors.get(&WindowRole::OverlayFullscreen).copied(),
            Some(CursorKind::Pointer),
            "Hovering center text of {:?} must set Pointer cursor",
            btn.id
        );

        let hovered_corner = backend
            .pixel_bgra(WindowRole::OverlayFullscreen, btn.rect.x + 3, btn.rect.y + 4)
            .unwrap();
        if !matches!(
            btn.id,
            PillButtonId::ModeScreenshot | PillButtonId::TargetSelection
        ) {
            assert_ne!(
                hovered_corner, base_corner,
                "Hovering text center of inactive button {:?} must update button highlight",
                btn.id
            );
        }
    }

    // Moving off the pill must revert cursor away from Pointer
    overlay.on_pointer_motion(&mut backend, 100.0, 100.0);
    assert_eq!(
        backend.cursors.get(&WindowRole::OverlayFullscreen).copied(),
        Some(CursorKind::Crosshair)
    );

    // Settings footer buttons must also hover cleanly over their text centers
    let mut settings = SettingsController::new(fonts);
    settings.show(&mut backend, &cfg);
    let cancel_cx = (540 - 22 - 96 - 10 - 44) as f64;
    let footer_cy = 531.0;
    let cancel_before = backend
        .pixel_bgra(WindowRole::SettingsDialog, cancel_cx as i32 - 35, 520)
        .unwrap();
    settings.on_pointer_motion(&mut backend, cancel_cx, footer_cy);
    assert_eq!(
        backend.cursors.get(&WindowRole::SettingsDialog).copied(),
        Some(CursorKind::Pointer)
    );
    let cancel_after = backend
        .pixel_bgra(WindowRole::SettingsDialog, cancel_cx as i32 - 35, 520)
        .unwrap();
    assert_ne!(cancel_before, cancel_after);
}
