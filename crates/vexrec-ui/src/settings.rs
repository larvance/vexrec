use std::path::{Path, PathBuf};
use std::sync::Arc;
use vexrec_core::config::VexrecConfig;
use vexrec_core::formats::{
    ContainerFormat, EncoderBackendPreference, ImageFormat, VideoQuality,
};
use vexrec_core::geometry::{Point, Rect};
use vexrec_display::{CursorKind, DisplayBackend, KeyAction, WindowRole};
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_render::SurfaceCanvas;

pub const SETTINGS_W: u32 = 540;
pub const SETTINGS_H: u32 = 560;

#[derive(Debug, Clone)]
pub enum SettingsOutcome {
    None,
    Minimize,
    Close,
    Saved(VexrecConfig),
    BrowseScreenshotDir,
    BrowseRecordingDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsControlId {
    Minimize,
    Close,
    CycleFps,
    CycleContainer,
    CycleEncoder,
    CycleQuality,
    ToggleHardwareAccel,
    ToggleShowCursor,
    ToggleRecordingFrame,
    ToggleDesktopAudio,
    ToggleMicAudio,
    ToggleNoiseSuppression,
    CycleScreenshotFormat,
    ToggleCopyClipboard,
    BrowseScreenshotDir,
    BrowseRecordingDir,
    Cancel,
    Save,
}

#[derive(Debug, Clone)]
struct ControlRect {
    id: SettingsControlId,
    rect: Rect,
}

/// Pure-Rust borderless preferences window (`540x560`) rendered directly onto a display surface.
pub struct SettingsController {
    canvas: SurfaceCanvas,
    visible: bool,
    draft: VexrecConfig,
    controls: Vec<ControlRect>,
    non_drag_rects: Vec<Rect>,
    hovered: Option<SettingsControlId>,
    last_geom: Rect,
}

impl SettingsController {
    pub fn new(fonts: Arc<FontEngine>) -> Self {
        Self {
            canvas: SurfaceCanvas::new(SETTINGS_W, SETTINGS_H, fonts),
            visible: false,
            draft: VexrecConfig::default(),
            controls: Vec::with_capacity(20),
            non_drag_rects: Vec::with_capacity(4),
            hovered: None,
            last_geom: Rect::new(0, 0, SETTINGS_W, SETTINGS_H),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn draft_config(&self) -> &VexrecConfig {
        &self.draft
    }

    pub fn show(&mut self, backend: &mut dyn DisplayBackend, cfg: &VexrecConfig) {
        self.draft = cfg.clone();
        self.hovered = None;
        let screen = backend.primary_size();
        let x = ((screen.width as i32 - SETTINGS_W as i32) / 2).max(0);
        let y = ((screen.height as i32 - SETTINGS_H as i32) / 2).max(0);
        self.last_geom = Rect::new(x, y, SETTINGS_W, SETTINGS_H);
        self.visible = true;

        self.rebuild_layout_and_render();
        backend.show_surface(WindowRole::SettingsDialog, self.last_geom);
        backend.set_cursor(WindowRole::SettingsDialog, CursorKind::Default);
        self.present(backend);
    }

    pub fn hide(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.hovered = None;
        backend.hide_surface(WindowRole::SettingsDialog);
    }

    pub fn set_screenshot_dir(&mut self, backend: &mut dyn DisplayBackend, dir: PathBuf) {
        self.draft.storage.screenshot_dir = vexrec_core::expand_tilde(&dir);
        if self.visible {
            self.rebuild_layout_and_render();
            self.present(backend);
        }
    }

    pub fn set_recording_dir(&mut self, backend: &mut dyn DisplayBackend, dir: PathBuf) {
        self.draft.storage.recording_dir = vexrec_core::expand_tilde(&dir);
        if self.visible {
            self.rebuild_layout_and_render();
            self.present(backend);
        }
    }

    pub fn on_expose(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        self.present(backend);
    }

    pub fn on_pointer_motion(&mut self, backend: &mut dyn DisplayBackend, x: f64, y: f64) {
        if !self.visible {
            return;
        }
        let next = self.hit_test(x, y);
        if next != self.hovered {
            self.hovered = next;
            let cursor = if next.is_some() {
                CursorKind::Pointer
            } else {
                CursorKind::Default
            };
            backend.set_cursor(WindowRole::SettingsDialog, cursor);
            self.rebuild_layout_and_render();
            self.present(backend);
        }
    }

    pub fn on_pointer_leave(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        if self.hovered.take().is_some() {
            backend.set_cursor(WindowRole::SettingsDialog, CursorKind::Default);
            self.rebuild_layout_and_render();
            self.present(backend);
        }
    }

    pub fn on_pointer_button(
        &mut self,
        backend: &mut dyn DisplayBackend,
        button: u32,
        pressed: bool,
        x: f64,
        y: f64,
    ) -> SettingsOutcome {
        if !self.visible || button != 1 {
            return SettingsOutcome::None;
        }
        if pressed {
            if self.is_draggable_point(x, y) {
                backend.begin_surface_move(WindowRole::SettingsDialog);
            }
            return SettingsOutcome::None;
        }
        let Some(ctrl) = self.hit_test(x, y) else {
            return SettingsOutcome::None;
        };
        let outcome = self.activate_control(ctrl);
        if self.visible {
            self.rebuild_layout_and_render();
            self.present(backend);
        }
        outcome
    }

    pub fn on_key(&mut self, _backend: &mut dyn DisplayBackend, key: KeyAction) -> SettingsOutcome {
        if !self.visible {
            return SettingsOutcome::None;
        }
        match key {
            KeyAction::Escape => SettingsOutcome::Close,
            KeyAction::Return => SettingsOutcome::Saved(self.draft.clone()),
            _ => SettingsOutcome::None,
        }
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<SettingsControlId> {
        let px = x.round() as i32;
        let py = y.round() as i32;
        self.controls
            .iter()
            .find(|c| c.rect.contains(Point::new(px, py)))
            .map(|c| c.id)
    }

    /// Returns true if `(x, y)` lies on a draggable region (anywhere inside the surface
    /// except interactive buttons, toggles, dropdown pills, and path input fields).
    pub fn is_draggable_point(&self, x: f64, y: f64) -> bool {
        let pt = Point::new(x.round() as i32, y.round() as i32);
        if !Rect::new(0, 0, SETTINGS_W, SETTINGS_H).contains(pt) {
            return false;
        }
        self.hit_test(x, y).is_none() && !self.non_drag_rects.iter().any(|r| r.contains(pt))
    }

    pub fn activate_control(&mut self, id: SettingsControlId) -> SettingsOutcome {
        match id {
            SettingsControlId::Minimize => SettingsOutcome::Minimize,
            SettingsControlId::Close => SettingsOutcome::Close,
            SettingsControlId::CycleFps => {
                self.draft.recording.framerate = match self.draft.recording.framerate {
                    30 => 60,
                    60 => 120,
                    _ => 30,
                };
                SettingsOutcome::None
            }
            SettingsControlId::CycleContainer => {
                self.draft.recording.container = match self.draft.recording.container {
                    ContainerFormat::Mp4 => ContainerFormat::Mkv,
                    ContainerFormat::Mkv => ContainerFormat::Webm,
                    ContainerFormat::Webm => ContainerFormat::Gif,
                    ContainerFormat::Gif => ContainerFormat::Mp4,
                };
                SettingsOutcome::None
            }
            SettingsControlId::CycleEncoder => {
                self.draft.recording.encoder_preference =
                    match self.draft.recording.encoder_preference {
                        EncoderBackendPreference::Auto => EncoderBackendPreference::VaApi,
                        EncoderBackendPreference::VaApi => EncoderBackendPreference::VulkanVideo,
                        EncoderBackendPreference::VulkanVideo => EncoderBackendPreference::Nvenc,
                        EncoderBackendPreference::Nvenc => EncoderBackendPreference::Cpu,
                        EncoderBackendPreference::Cpu => EncoderBackendPreference::Auto,
                    };
                SettingsOutcome::None
            }
            SettingsControlId::CycleQuality => {
                self.draft.recording.quality = match self.draft.recording.quality {
                    VideoQuality::Low => VideoQuality::Medium,
                    VideoQuality::Medium => VideoQuality::High,
                    VideoQuality::High => VideoQuality::Lossless,
                    VideoQuality::Lossless => VideoQuality::Low,
                };
                SettingsOutcome::None
            }
            SettingsControlId::ToggleHardwareAccel => {
                self.draft.recording.hardware_accel = !self.draft.recording.hardware_accel;
                SettingsOutcome::None
            }
            SettingsControlId::ToggleShowCursor => {
                self.draft.recording.show_cursor = !self.draft.recording.show_cursor;
                SettingsOutcome::None
            }
            SettingsControlId::ToggleRecordingFrame => {
                self.draft.recording.show_recording_frame =
                    !self.draft.recording.show_recording_frame;
                SettingsOutcome::None
            }
            SettingsControlId::ToggleDesktopAudio => {
                self.draft.audio.capture_desktop = !self.draft.audio.capture_desktop;
                SettingsOutcome::None
            }
            SettingsControlId::ToggleMicAudio => {
                self.draft.audio.capture_microphone = !self.draft.audio.capture_microphone;
                SettingsOutcome::None
            }
            SettingsControlId::ToggleNoiseSuppression => {
                self.draft.audio.noise_suppression = !self.draft.audio.noise_suppression;
                SettingsOutcome::None
            }
            SettingsControlId::CycleScreenshotFormat => {
                self.draft.screenshot.default_format = match self.draft.screenshot.default_format {
                    ImageFormat::Png => ImageFormat::Jpeg,
                    ImageFormat::Jpeg => ImageFormat::Webp,
                    ImageFormat::Webp => ImageFormat::Png,
                };
                SettingsOutcome::None
            }
            SettingsControlId::ToggleCopyClipboard => {
                self.draft.screenshot.copy_to_clipboard =
                    !self.draft.screenshot.copy_to_clipboard;
                SettingsOutcome::None
            }
            SettingsControlId::BrowseScreenshotDir => SettingsOutcome::BrowseScreenshotDir,
            SettingsControlId::BrowseRecordingDir => SettingsOutcome::BrowseRecordingDir,
            SettingsControlId::Cancel => SettingsOutcome::Close,
            SettingsControlId::Save => SettingsOutcome::Saved(self.draft.clone()),
        }
    }

    fn present(&self, backend: &mut dyn DisplayBackend) {
        let _ = backend.present_surface_bgra(
            WindowRole::SettingsDialog,
            SETTINGS_W,
            SETTINGS_H,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
    }

    pub fn rebuild_layout_and_render(&mut self) {
        self.controls.clear();
        self.non_drag_rects.clear();
        self.canvas.clear_transparent(None);

        let w = SETTINGS_W as f32;
        let h = SETTINGS_H as f32;

        // Solid dark surface + crisp zinc border
        self.canvas
            .fill_rounded_rect(0.0, 0.0, w, h, 10.0, [24, 24, 27, 255], None);
        self.canvas.stroke_rounded_rect(
            0.5,
            0.5,
            w - 1.0,
            h - 1.0,
            9.5,
            1.0,
            [63, 63, 70, 220],
            None,
        );

        // Borderless Header (draggable except Minimize & Close buttons)
        self.canvas
            .draw_text(FontRole::Sans, 16.0, 22, 31, "Vexrec Preferences", [250, 250, 250, 255]);

        let close_rect = Rect::new(SETTINGS_W as i32 - 14 - 28, 11, 28, 28);
        let min_rect = Rect::new(close_rect.x - 6 - 28, 11, 28, 28);

        let min_hover = self.hovered == Some(SettingsControlId::Minimize);
        if min_hover {
            self.canvas.fill_rounded_rect(
                min_rect.x as f32,
                min_rect.y as f32,
                min_rect.width as f32,
                min_rect.height as f32,
                6.0,
                [63, 63, 70, 220],
                None,
            );
        }
        let min_fg = if min_hover {
            [255, 255, 255, 255]
        } else {
            [180, 180, 188, 255]
        };
        self.canvas.draw_line(
            min_rect.x as f32 + 9.0,
            min_rect.y as f32 + 14.0,
            min_rect.x as f32 + 19.0,
            min_rect.y as f32 + 14.0,
            1.6,
            min_fg,
            None,
        );
        self.controls.push(ControlRect {
            id: SettingsControlId::Minimize,
            rect: min_rect,
        });

        let close_hover = self.hovered == Some(SettingsControlId::Close);
        if close_hover {
            self.canvas.fill_rounded_rect(
                close_rect.x as f32,
                close_rect.y as f32,
                close_rect.width as f32,
                close_rect.height as f32,
                6.0,
                [220, 38, 38, 215],
                None,
            );
        }
        let close_fg = if close_hover {
            [255, 255, 255, 255]
        } else {
            [180, 180, 188, 255]
        };
        self.canvas.draw_line(
            close_rect.x as f32 + 9.5,
            close_rect.y as f32 + 9.5,
            close_rect.x as f32 + 18.5,
            close_rect.y as f32 + 18.5,
            1.6,
            close_fg,
            None,
        );
        self.canvas.draw_line(
            close_rect.x as f32 + 9.5,
            close_rect.y as f32 + 18.5,
            close_rect.x as f32 + 18.5,
            close_rect.y as f32 + 9.5,
            1.6,
            close_fg,
            None,
        );
        self.controls.push(ControlRect {
            id: SettingsControlId::Close,
            rect: close_rect,
        });

        self.canvas
            .draw_sharp_rect(0, 50, SETTINGS_W, 1, [39, 39, 42, 255], None);

        // Section: RECORDING
        self.draw_section_header(22, 68, "RECORDING");

        let mut y = 76;
        let step = 27;

        let fps_label = format!("{} FPS", self.draft.recording.framerate);
        self.draw_cycle_row(
            y,
            "Framerate",
            &fps_label,
            SettingsControlId::CycleFps,
        );
        y += step;

        let fmt_label = match self.draft.recording.container {
            ContainerFormat::Mp4 => "MP4 (fMP4 / Opus)",
            ContainerFormat::Mkv => "MKV (Matroska / Opus)",
            ContainerFormat::Webm => "WebM (VP9 / Opus)",
            ContainerFormat::Gif => "GIF (Animated)",
        };
        self.draw_cycle_row(
            y,
            "Container Format",
            fmt_label,
            SettingsControlId::CycleContainer,
        );
        y += step;

        let enc_label = match self.draft.recording.encoder_preference {
            EncoderBackendPreference::Auto => "Auto (HW -> CPU)",
            EncoderBackendPreference::VaApi => "VA-API (DRM)",
            EncoderBackendPreference::VulkanVideo => "Vulkan Video",
            EncoderBackendPreference::Nvenc => "NVIDIA NVENC",
            EncoderBackendPreference::Cpu => "CPU (Software)",
        };
        self.draw_cycle_row(
            y,
            "Encoder Backend",
            enc_label,
            SettingsControlId::CycleEncoder,
        );
        y += step;

        let qual_label = match self.draft.recording.quality {
            VideoQuality::Low => "Low",
            VideoQuality::Medium => "Medium",
            VideoQuality::High => "High",
            VideoQuality::Lossless => "Lossless",
        };
        self.draw_cycle_row(
            y,
            "Quality Preset",
            qual_label,
            SettingsControlId::CycleQuality,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "Hardware Acceleration",
            self.draft.recording.hardware_accel,
            SettingsControlId::ToggleHardwareAccel,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "Capture Mouse Cursor",
            self.draft.recording.show_cursor,
            SettingsControlId::ToggleShowCursor,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "Show Recording Frame",
            self.draft.recording.show_recording_frame,
            SettingsControlId::ToggleRecordingFrame,
        );

        // Section: AUDIO
        self.draw_section_header(22, 276, "AUDIO");
        y = 284;

        self.draw_toggle_row(
            y,
            "Desktop Audio Monitor",
            self.draft.audio.capture_desktop,
            SettingsControlId::ToggleDesktopAudio,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "Microphone Input",
            self.draft.audio.capture_microphone,
            SettingsControlId::ToggleMicAudio,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "RNNoise Suppression",
            self.draft.audio.noise_suppression,
            SettingsControlId::ToggleNoiseSuppression,
        );

        // Section: SCREENSHOT & STORAGE
        self.draw_section_header(22, 376, "SCREENSHOT & STORAGE");
        y = 384;

        let snap_fmt = match self.draft.screenshot.default_format {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Webp => "WebP",
        };
        self.draw_cycle_row(
            y,
            "Screenshot Format",
            snap_fmt,
            SettingsControlId::CycleScreenshotFormat,
        );
        y += step;

        self.draw_toggle_row(
            y,
            "Copy Screenshot to Clipboard",
            self.draft.screenshot.copy_to_clipboard,
            SettingsControlId::ToggleCopyClipboard,
        );
        y += step;

        let snap_dir = self.draft.storage.screenshot_dir.clone();
        self.draw_path_row(
            y,
            "Screenshots Dir",
            &snap_dir,
            SettingsControlId::BrowseScreenshotDir,
        );
        y += step;

        let rec_dir = self.draft.storage.recording_dir.clone();
        self.draw_path_row(
            y,
            "Recordings Dir",
            &rec_dir,
            SettingsControlId::BrowseRecordingDir,
        );

        // Footer divider & buttons
        self.canvas
            .draw_sharp_rect(0, 504, SETTINGS_W, 1, [39, 39, 42, 255], None);

        let save_rect = Rect::new(SETTINGS_W as i32 - 22 - 96, 515, 96, 32);
        let cancel_rect = Rect::new(save_rect.x - 10 - 88, 515, 88, 32);

        let cancel_hover = self.hovered == Some(SettingsControlId::Cancel);
        let cancel_fill = if cancel_hover {
            [63, 63, 70, 255]
        } else {
            [39, 39, 42, 255]
        };
        self.canvas.fill_rounded_rect(
            cancel_rect.x as f32,
            cancel_rect.y as f32,
            cancel_rect.width as f32,
            cancel_rect.height as f32,
            6.0,
            cancel_fill,
            None,
        );
        self.draw_centered_text("Cancel", 12.5, cancel_rect, [244, 244, 245, 255]);
        self.controls.push(ControlRect {
            id: SettingsControlId::Cancel,
            rect: cancel_rect,
        });

        let save_hover = self.hovered == Some(SettingsControlId::Save);
        let save_fill = if save_hover {
            [228, 228, 231, 255]
        } else {
            [255, 255, 255, 255]
        };
        self.canvas.fill_rounded_rect(
            save_rect.x as f32,
            save_rect.y as f32,
            save_rect.width as f32,
            save_rect.height as f32,
            6.0,
            save_fill,
            None,
        );
        self.draw_centered_text("Save", 12.5, save_rect, [9, 9, 11, 255]);
        self.controls.push(ControlRect {
            id: SettingsControlId::Save,
            rect: save_rect,
        });
    }

    fn draw_section_header(&mut self, x: i32, baseline_y: i32, title: &str) {
        self.canvas.draw_text(
            FontRole::Sans,
            10.5,
            x,
            baseline_y,
            title,
            [113, 113, 122, 255],
        );
    }

    fn draw_cycle_row(&mut self, y: i32, label: &str, value: &str, id: SettingsControlId) {
        self.canvas.draw_text(
            FontRole::Sans,
            12.5,
            22,
            y + 17,
            label,
            [228, 228, 231, 255],
        );
        let pill = Rect::new(SETTINGS_W as i32 - 22 - 196, y + 1, 196, 23);
        let hovered = self.hovered == Some(id);
        let bg = if hovered {
            [52, 52, 58, 255]
        } else {
            [39, 39, 42, 255]
        };
        let border = if hovered {
            [113, 113, 122, 255]
        } else {
            [63, 63, 70, 255]
        };
        self.canvas.fill_rounded_rect(
            pill.x as f32,
            pill.y as f32,
            pill.width as f32,
            pill.height as f32,
            5.0,
            bg,
            None,
        );
        self.canvas.stroke_rounded_rect(
            pill.x as f32 + 0.5,
            pill.y as f32 + 0.5,
            pill.width as f32 - 1.0,
            pill.height as f32 - 1.0,
            4.5,
            1.0,
            border,
            None,
        );
        self.draw_centered_text(value, 12.0, pill, [244, 244, 245, 255]);
        self.controls.push(ControlRect { id, rect: pill });
    }

    fn draw_toggle_row(&mut self, y: i32, label: &str, enabled: bool, id: SettingsControlId) {
        self.canvas.draw_text(
            FontRole::Sans,
            12.5,
            22,
            y + 17,
            label,
            [228, 228, 231, 255],
        );
        let pill = Rect::new(SETTINGS_W as i32 - 22 - 68, y + 2, 68, 22);
        let hovered = self.hovered == Some(id);
        let bg = if enabled {
            if hovered {
                [228, 228, 231, 255]
            } else {
                [255, 255, 255, 255]
            }
        } else if hovered {
            [52, 52, 58, 255]
        } else {
            [39, 39, 42, 255]
        };
        let fg = if enabled {
            [9, 9, 11, 255]
        } else {
            [161, 161, 170, 255]
        };
        self.canvas.fill_rounded_rect(
            pill.x as f32,
            pill.y as f32,
            pill.width as f32,
            pill.height as f32,
            11.0,
            bg,
            None,
        );
        let txt = if enabled { "ON" } else { "OFF" };
        self.draw_centered_text(txt, 11.5, pill, fg);
        self.controls.push(ControlRect { id, rect: pill });
    }

    fn draw_path_row(&mut self, y: i32, label: &str, path: &Path, id: SettingsControlId) {
        self.canvas.draw_text(
            FontRole::Sans,
            12.5,
            22,
            y + 17,
            label,
            [228, 228, 231, 255],
        );
        let browse_rect = Rect::new(SETTINGS_W as i32 - 22 - 74, y + 1, 74, 23);
        let path_rect = Rect::new(160, y + 1, (browse_rect.x - 160 - 8).max(0) as u32, 23);
        self.non_drag_rects.push(path_rect);

        self.canvas.fill_rounded_rect(
            path_rect.x as f32,
            path_rect.y as f32,
            path_rect.width as f32,
            path_rect.height as f32,
            5.0,
            [30, 30, 34, 255],
            None,
        );
        let truncated = compact_path(path, 34);
        self.canvas.draw_text(
            FontRole::Mono,
            11.0,
            path_rect.x + 8,
            path_rect.y + 15,
            &truncated,
            [212, 212, 216, 255],
        );

        let hovered = self.hovered == Some(id);
        let bg = if hovered {
            [63, 63, 70, 255]
        } else {
            [39, 39, 42, 255]
        };
        self.canvas.fill_rounded_rect(
            browse_rect.x as f32,
            browse_rect.y as f32,
            browse_rect.width as f32,
            browse_rect.height as f32,
            5.0,
            bg,
            None,
        );
        self.canvas.stroke_rounded_rect(
            browse_rect.x as f32 + 0.5,
            browse_rect.y as f32 + 0.5,
            browse_rect.width as f32 - 1.0,
            browse_rect.height as f32 - 1.0,
            4.5,
            1.0,
            [63, 63, 70, 255],
            None,
        );
        self.draw_centered_text("Browse", 11.5, browse_rect, [244, 244, 245, 255]);
        self.controls.push(ControlRect {
            id,
            rect: browse_rect,
        });
    }

    fn draw_centered_text(&mut self, text: &str, px: f32, rect: Rect, color: [u8; 4]) {
        let (tw, _) = self.canvas.measure_text(FontRole::Sans, px, text);
        let tx = rect.x + (rect.width as i32 - tw) / 2;
        let ty = rect.y + (rect.height as i32 + (px * 0.72).round() as i32) / 2;
        self.canvas
            .draw_text(FontRole::Sans, px, tx, ty, text, color);
    }
}

fn compact_path(path: &Path, max_chars: usize) -> String {
    let s = path.to_string_lossy();
    let count = s.chars().count();
    if count <= max_chars {
        return s.into_owned();
    }
    let keep = max_chars.saturating_sub(3);
    let tail: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}
