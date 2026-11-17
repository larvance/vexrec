use std::sync::Arc;
use std::time::{Duration, Instant};
use vexrec_core::buffer::PixelImage;
use vexrec_core::config::VexrecConfig;
use vexrec_core::geometry::{Point, Rect};
use vexrec_display::{ClientWindowInfo, CursorKind, DisplayBackend, KeyAction, WindowRole};
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_render::SurfaceCanvas;

use crate::state::{
    apply_drag_delta, compute_window_mosaic, compute_zoom_target_rect, ease_out_cubic_rect,
    extract_window_crop_or_fallback, hit_test_selection_handle, ActionMode, DragHandle,
    PillButtonId, PillButtonRect, SelectionAnchor, TargetMode, WindowCardLayout,
};

pub const ZOOM_DURATION_MS: u64 = 220;

#[derive(Debug)]
pub enum OverlayOutcome {
    None,
    Cancel,
    CaptureScreenshot { img: PixelImage, save_as: bool },
    StartRecording { crop: Option<Rect>, record_mic: bool, record_desktop: bool },
    OpenSettings,
    RefreshCapture,
}

pub struct FreezeOverlayController {
    canvas: SurfaceCanvas,
    visible: bool,
    screen_w: u32,
    screen_h: u32,
    action_mode: ActionMode,
    target_mode: TargetMode,
    mic_enabled: bool,
    audio_enabled: bool,
    desktop: Option<PixelImage>,
    selection: Option<Rect>,
    drag_anchor: Option<SelectionAnchor>,
    cards: Vec<WindowCardLayout>,
    hovered_card: Option<usize>,
    zoomed_card: Option<usize>,
    zoom_from: Rect,
    zoom_to: Rect,
    zoom_start: Option<Instant>,
    zoom_selection: Option<Rect>,
    pill_rect: Rect,
    pill_buttons: Vec<PillButtonRect>,
    hovered_btn: Option<PillButtonId>,
    last_cursor: Option<CursorKind>,
}

impl FreezeOverlayController {
    pub fn new(fonts: Arc<FontEngine>) -> Self {
        Self {
            canvas: SurfaceCanvas::new(1920, 1080, fonts),
            visible: false,
            screen_w: 1920,
            screen_h: 1080,
            action_mode: ActionMode::Screenshot,
            target_mode: TargetMode::Selection,
            mic_enabled: false,
            audio_enabled: true,
            desktop: None,
            selection: None,
            drag_anchor: None,
            cards: Vec::new(),
            hovered_card: None,
            zoomed_card: None,
            zoom_from: Rect::default(),
            zoom_to: Rect::default(),
            zoom_start: None,
            zoom_selection: None,
            pill_rect: Rect::default(),
            pill_buttons: Vec::new(),
            hovered_btn: None,
            last_cursor: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn is_animating(&self) -> bool {
        if let Some(start) = self.zoom_start {
            start.elapsed() < Duration::from_millis(ZOOM_DURATION_MS)
        } else {
            false
        }
    }

    pub fn needs_live_window_refresh(&self) -> bool {
        self.visible
            && self.action_mode == ActionMode::Record
            && matches!(self.target_mode, TargetMode::Window | TargetMode::WindowRegion)
    }

    pub fn pill_buttons(&self) -> &[PillButtonRect] {
        &self.pill_buttons
    }

    pub fn action_mode(&self) -> ActionMode {
        self.action_mode
    }

    pub fn target_mode(&self) -> TargetMode {
        self.target_mode
    }

    /// Shows the fullscreen freeze overlay. Always resets `ActionMode::Screenshot` cleanly.
    pub fn show(
        &mut self,
        backend: &mut dyn DisplayBackend,
        cfg: &VexrecConfig,
        desktop: PixelImage,
        windows: Vec<ClientWindowInfo>,
    ) {
        let sz = backend.primary_size();
        self.screen_w = desktop.width.max(sz.width).max(1);
        self.screen_h = desktop.height.max(sz.height).max(1);
        self.canvas.resize(self.screen_w, self.screen_h);

        // Enforce Screenshot mode default every time overlay opens
        self.action_mode = ActionMode::Screenshot;
        self.target_mode = match cfg.screenshot.last_mode.as_deref() {
            Some("window") => TargetMode::Window,
            Some("window_region") => TargetMode::WindowRegion,
            Some("fullscreen") => TargetMode::Fullscreen,
            _ => TargetMode::Selection,
        };
        self.mic_enabled = cfg.audio.capture_microphone;
        self.audio_enabled = cfg.audio.capture_desktop;
        self.desktop = Some(desktop);

        self.selection = cfg.screenshot.last_region.and_then(|r| {
            let clamped = r.clamp_bounds(self.screen_w, self.screen_h);
            if clamped.width >= 16 && clamped.height >= 16 {
                Some(clamped)
            } else {
                None
            }
        });
        self.drag_anchor = None;
        self.cards = compute_window_mosaic(&windows, self.screen_w, self.screen_h);
        self.hovered_card = None;
        self.zoomed_card = None;
        self.zoom_start = None;
        self.zoom_selection = None;
        self.hovered_btn = None;
        self.last_cursor = None;
        self.visible = true;

        self.layout_pill();
        self.render(None);

        backend.show_surface(
            WindowRole::OverlayFullscreen,
            Rect::new(0, 0, self.screen_w, self.screen_h),
        );
        let _ = backend.present_surface_bgra(
            WindowRole::OverlayFullscreen,
            self.canvas.width,
            self.canvas.height,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
        backend.grab_overlay_keyboard();
        self.update_cursor(backend, -1.0, -1.0);
    }

    pub fn hide(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.action_mode = ActionMode::Screenshot;
        self.drag_anchor = None;
        self.zoom_start = None;
        self.zoomed_card = None;
        self.zoom_selection = None;
        self.desktop = None;
        self.cards.clear();
        self.last_cursor = None;
        backend.ungrab_overlay_keyboard();
        backend.hide_surface(WindowRole::OverlayFullscreen);
    }

    pub fn on_expose(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        let _ = backend.present_surface_bgra(
            WindowRole::OverlayFullscreen,
            self.canvas.width,
            self.canvas.height,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
    }

    pub fn on_window_damaged(&mut self, backend: &mut dyn DisplayBackend, window_id: u64) {
        if !self.visible {
            return;
        }
        let show_overview = matches!(
            self.target_mode,
            TargetMode::Window | TargetMode::WindowRegion
        );
        if !show_overview {
            return;
        }
        if let Ok(fresh) = backend.capture_window(window_id) {
            if let Some(zoom_idx) = self.zoomed_card {
                if let Some(card) = self.cards.get_mut(zoom_idx) {
                    if card.window.id == window_id {
                        card.window.preview = Some(fresh);
                        self.present_dirty(backend, Some(self.zoom_to.outset(8)));
                        return;
                    }
                }
            }
            for card in &mut self.cards {
                if card.window.id == window_id {
                    let dirty = card.card_rect.outset(8);
                    card.window.preview = Some(fresh);
                    self.present_dirty(backend, Some(dirty));
                    return;
                }
            }
        }
    }

    pub fn tick(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible || self.zoom_start.is_none() {
            return;
        }
        let done = self
            .zoom_start
            .map(|s| s.elapsed() >= Duration::from_millis(ZOOM_DURATION_MS))
            .unwrap_or(true);
        self.present_dirty(backend, None);
        if done {
            self.zoom_start = None;
        }
    }

    fn capture_fresh_window_previews(&mut self, backend: &mut dyn DisplayBackend) {
        for card in &mut self.cards {
            if let Ok(fresh) = backend.capture_window(card.window.id) {
                card.window.preview = Some(fresh);
            }
        }
    }

    pub fn refresh_live_windows(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.needs_live_window_refresh() {
            return;
        }
        if let Some(zoom_idx) = self.zoomed_card {
            if let Some(card) = self.cards.get_mut(zoom_idx) {
                if let Ok(fresh) = backend.capture_window(card.window.id) {
                    card.window.preview = Some(fresh);
                    self.present_dirty(backend, Some(self.zoom_to.outset(8)));
                }
            }
            return;
        }
        if self.cards.is_empty() {
            return;
        }
        let mut any_updated = false;
        for card in &mut self.cards {
            if let Ok(fresh) = backend.capture_window(card.window.id) {
                card.window.preview = Some(fresh);
                any_updated = true;
            }
        }
        if any_updated {
            let mosaic_area = Rect::new(0, 0, self.screen_w, self.pill_rect.y.max(1) as u32);
            self.present_dirty(backend, Some(mosaic_area));
        }
    }

    pub fn on_key(&mut self, backend: &mut dyn DisplayBackend, key: KeyAction) -> OverlayOutcome {
        if !self.visible {
            return OverlayOutcome::None;
        }
        match key {
            KeyAction::Escape => {
                if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                    self.zoomed_card = None;
                    self.zoom_start = None;
                    self.zoom_selection = None;
                    self.drag_anchor = None;
                    self.present_dirty(backend, None);
                    return OverlayOutcome::None;
                }
                OverlayOutcome::Cancel
            }
            KeyAction::Print => OverlayOutcome::RefreshCapture,
            KeyAction::Return => self.trigger_primary_action(false),
            KeyAction::CtrlC => self.trigger_primary_action(false),
            KeyAction::CtrlS => {
                if self.action_mode == ActionMode::Screenshot {
                    self.trigger_primary_action(true)
                } else {
                    OverlayOutcome::None
                }
            }
            KeyAction::Num(n) => {
                let next = match n {
                    1 => TargetMode::Selection,
                    2 => TargetMode::Window,
                    3 => TargetMode::WindowRegion,
                    4 => TargetMode::Fullscreen,
                    _ => return OverlayOutcome::None,
                };
                if next != self.target_mode {
                    self.switch_target_mode(backend, next);
                }
                OverlayOutcome::None
            }
            _ => OverlayOutcome::None,
        }
    }

    pub fn on_pointer_motion(&mut self, backend: &mut dyn DisplayBackend, x: f64, y: f64) {
        if !self.visible {
            return;
        }

        if let Some(ref anchor) = self.drag_anchor {
            let bounds = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                self.zoom_to
            } else {
                Rect::new(0, 0, self.screen_w, self.screen_h)
            };
            let new_rect = apply_drag_delta(anchor, x, y, bounds);

            let old_opt = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                let prev = self.zoom_selection.replace(new_rect);
                prev
            } else {
                let prev = self.selection.replace(new_rect);
                prev
            };

            let dirty = match old_opt {
                Some(old) => old.union(new_rect).outset(64),
                None => new_rect.outset(64),
            };
            self.present_dirty(backend, Some(dirty));
            self.update_cursor(backend, x, y);
            return;
        }

        let old_btn = self.hovered_btn;
        let old_card = self.hovered_card;
        let px = x.round() as i32;
        let py = y.round() as i32;

        self.hovered_btn = self
            .pill_buttons
            .iter()
            .find(|b| b.rect.contains(Point::new(px, py)))
            .map(|b| b.id);

        self.hovered_card = if self.hovered_btn.is_none()
            && ((self.target_mode == TargetMode::Window)
                || (self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_none()))
        {
            self.cards
                .iter()
                .find(|c| c.card_rect.contains(Point::new(px, py)))
                .map(|c| c.index)
        } else {
            None
        };

        if old_btn != self.hovered_btn {
            self.present_dirty(backend, Some(self.pill_rect.outset(16)));
        }
        if old_card != self.hovered_card {
            let mut dirty = None;
            if let Some(i) = old_card {
                if let Some(c) = self.cards.get(i) {
                    dirty = Some(c.card_rect.outset(12));
                }
            }
            if let Some(i) = self.hovered_card {
                if let Some(c) = self.cards.get(i) {
                    let r = c.card_rect.outset(12);
                    dirty = Some(match dirty {
                        Some(d) => d.union(r),
                        None => r,
                    });
                }
            }
            if let Some(d) = dirty {
                self.present_dirty(backend, Some(d));
            }
        }

        self.update_cursor(backend, x, y);
    }

    pub fn on_pointer_button(
        &mut self,
        backend: &mut dyn DisplayBackend,
        button: u32,
        pressed: bool,
        x: f64,
        y: f64,
    ) -> OverlayOutcome {
        if !self.visible {
            return OverlayOutcome::None;
        }
        if button != 1 {
            if button == 3 && !pressed {
                return OverlayOutcome::Cancel;
            }
            return OverlayOutcome::None;
        }

        let px = x.round() as i32;
        let py = y.round() as i32;

        if pressed {
            // Check pill clicks first
            if let Some(btn) = self
                .pill_buttons
                .iter()
                .find(|b| b.rect.contains(Point::new(px, py)))
                .map(|b| b.id)
            {
                return self.handle_pill_click(backend, btn);
            }

            // Overview mosaic clicks
            if self.target_mode == TargetMode::Window {
                if let Some(card_idx) = self
                    .cards
                    .iter()
                    .find(|c| c.card_rect.contains(Point::new(px, py)))
                    .map(|c| c.index)
                {
                    return self.trigger_window_action(card_idx);
                }
                return OverlayOutcome::None;
            }

            if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_none() {
                if let Some(card_idx) = self
                    .cards
                    .iter()
                    .find(|c| c.card_rect.contains(Point::new(px, py)))
                    .map(|c| c.index)
                {
                    let card = &self.cards[card_idx];
                    let src_w = card
                        .window
                        .preview
                        .as_ref()
                        .map(|p| p.width)
                        .unwrap_or(card.window.rect.width);
                    let src_h = card
                        .window
                        .preview
                        .as_ref()
                        .map(|p| p.height)
                        .unwrap_or(card.window.rect.height);
                    self.zoom_from = card.thumb_rect;
                    self.zoom_to = compute_zoom_target_rect(src_w, src_h, self.screen_w, self.screen_h);
                    self.zoom_start = Some(Instant::now());
                    self.zoomed_card = Some(card_idx);
                    self.zoom_selection = None;
                    self.hovered_card = None;
                    self.present_dirty(backend, None);
                    self.update_cursor(backend, x, y);
                }
                return OverlayOutcome::None;
            }

            if self.target_mode == TargetMode::Fullscreen {
                return OverlayOutcome::None;
            }

            // Selection / Zoomed Window Region drag start
            let active_opt = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                self.zoom_selection
            } else {
                self.selection
            };
            let bounds = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                self.zoom_to
            } else {
                Rect::new(0, 0, self.screen_w, self.screen_h)
            };

            let handle = match active_opt {
                Some(r) => hit_test_selection_handle(r, x, y),
                None => DragHandle::None,
            };

            if handle != DragHandle::None {
                self.drag_anchor = Some(SelectionAnchor {
                    handle,
                    start_pointer: Point::new(px, py),
                    orig_rect: active_opt.unwrap_or_default(),
                });
            } else if bounds.contains(Point::new(px, py)) {
                let start_r = Rect::new(px, py, 1, 1);
                if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                    let prev = self.zoom_selection.replace(start_r);
                    self.drag_anchor = Some(SelectionAnchor {
                        handle: DragHandle::Creating,
                        start_pointer: Point::new(px, py),
                        orig_rect: start_r,
                    });
                    if let Some(p) = prev {
                        self.present_dirty(backend, Some(p.outset(64)));
                    }
                } else {
                    let prev = self.selection.replace(start_r);
                    self.drag_anchor = Some(SelectionAnchor {
                        handle: DragHandle::Creating,
                        start_pointer: Point::new(px, py),
                        orig_rect: start_r,
                    });
                    if let Some(p) = prev {
                        self.present_dirty(backend, Some(p.outset(64)));
                    }
                }
            }
            self.update_cursor(backend, x, y);
            return OverlayOutcome::None;
        }

        // Release left mouse button
        if let Some(anchor) = self.drag_anchor.take() {
            if anchor.handle == DragHandle::Creating {
                let active = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                    self.zoom_selection
                } else {
                    self.selection
                };
                if let Some(r) = active {
                    if r.width < 6 || r.height < 6 {
                        if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                            self.zoom_selection = None;
                        } else {
                            self.selection = None;
                        }
                        self.present_dirty(backend, Some(r.outset(64)));
                    }
                }
            }
            self.update_cursor(backend, x, y);
        }
        OverlayOutcome::None
    }

    fn switch_target_mode(&mut self, backend: &mut dyn DisplayBackend, next: TargetMode) {
        self.target_mode = next;
        self.drag_anchor = None;
        self.zoomed_card = None;
        self.zoom_start = None;
        self.zoom_selection = None;
        self.hovered_card = None;
        self.layout_pill();
        if self.action_mode == ActionMode::Record
            && matches!(self.target_mode, TargetMode::Window | TargetMode::WindowRegion)
        {
            self.capture_fresh_window_previews(backend);
        }
        self.present_dirty(backend, None);
    }

    fn handle_pill_click(
        &mut self,
        backend: &mut dyn DisplayBackend,
        btn: PillButtonId,
    ) -> OverlayOutcome {
        match btn {
            PillButtonId::ModeScreenshot => {
                if self.action_mode != ActionMode::Screenshot {
                    self.action_mode = ActionMode::Screenshot;
                    self.layout_pill();
                    self.present_dirty(backend, None);
                }
            }
            PillButtonId::ModeRecord => {
                if self.action_mode != ActionMode::Record {
                    self.action_mode = ActionMode::Record;
                    self.layout_pill();
                    if matches!(self.target_mode, TargetMode::Window | TargetMode::WindowRegion) {
                        self.capture_fresh_window_previews(backend);
                    }
                    self.present_dirty(backend, None);
                }
            }
            PillButtonId::TargetSelection => self.switch_target_mode(backend, TargetMode::Selection),
            PillButtonId::TargetWindow => self.switch_target_mode(backend, TargetMode::Window),
            PillButtonId::TargetWindowRegion => {
                self.switch_target_mode(backend, TargetMode::WindowRegion)
            }
            PillButtonId::TargetFullscreen => {
                self.switch_target_mode(backend, TargetMode::Fullscreen)
            }
            PillButtonId::ToggleMic => {
                self.mic_enabled = !self.mic_enabled;
                self.layout_pill();
                self.present_dirty(backend, Some(self.pill_rect.outset(16)));
            }
            PillButtonId::ToggleAudio => {
                self.audio_enabled = !self.audio_enabled;
                self.layout_pill();
                self.present_dirty(backend, Some(self.pill_rect.outset(16)));
            }
            PillButtonId::DoneOrRecord => return self.trigger_primary_action(false),
            PillButtonId::SaveAs => return self.trigger_primary_action(true),
            PillButtonId::Settings => return OverlayOutcome::OpenSettings,
            PillButtonId::Cancel => return OverlayOutcome::Cancel,
        }
        OverlayOutcome::None
    }

    fn trigger_window_action(&self, card_idx: usize) -> OverlayOutcome {
        let Some(card) = self.cards.get(card_idx) else {
            return OverlayOutcome::None;
        };
        let win_rect = card.window.rect.clamp_bounds(self.screen_w, self.screen_h);
        match self.action_mode {
            ActionMode::Record => {
                if win_rect.width >= 16 && win_rect.height >= 16 {
                    OverlayOutcome::StartRecording {
                        crop: Some(win_rect),
                        record_mic: self.mic_enabled,
                        record_desktop: self.audio_enabled,
                    }
                } else {
                    OverlayOutcome::None
                }
            }
            ActionMode::Screenshot => {
                if let Some(ref p) = card.window.preview {
                    return OverlayOutcome::CaptureScreenshot {
                        img: p.clone(),
                        save_as: false,
                    };
                }
                if let Some(ref desktop) = self.desktop {
                    if let Some(cropped) = desktop.crop_rect(
                        win_rect.x.max(0) as u32,
                        win_rect.y.max(0) as u32,
                        win_rect.width,
                        win_rect.height,
                    ) {
                        return OverlayOutcome::CaptureScreenshot {
                            img: cropped,
                            save_as: false,
                        };
                    }
                }
                OverlayOutcome::None
            }
        }
    }

    fn trigger_primary_action(&self, save_as: bool) -> OverlayOutcome {
        let Some(ref desktop) = self.desktop else {
            return OverlayOutcome::None;
        };

        match self.target_mode {
            TargetMode::Fullscreen => match self.action_mode {
                ActionMode::Screenshot => OverlayOutcome::CaptureScreenshot {
                    img: desktop.clone(),
                    save_as,
                },
                ActionMode::Record => OverlayOutcome::StartRecording {
                    crop: None,
                    record_mic: self.mic_enabled,
                    record_desktop: self.audio_enabled,
                },
            },
            TargetMode::Selection => {
                let r = self
                    .selection
                    .filter(|s| s.width >= 4 && s.height >= 4)
                    .unwrap_or_else(|| Rect::new(0, 0, self.screen_w, self.screen_h));
                match self.action_mode {
                    ActionMode::Screenshot => {
                        if let Some(img) = desktop.crop_rect(
                            r.x.max(0) as u32,
                            r.y.max(0) as u32,
                            r.width,
                            r.height,
                        ) {
                            OverlayOutcome::CaptureScreenshot { img, save_as }
                        } else {
                            OverlayOutcome::None
                        }
                    }
                    ActionMode::Record => {
                        let crop = if r.width == self.screen_w && r.height == self.screen_h {
                            None
                        } else {
                            Some(r)
                        };
                        OverlayOutcome::StartRecording {
                            crop,
                            record_mic: self.mic_enabled,
                            record_desktop: self.audio_enabled,
                        }
                    }
                }
            }
            TargetMode::Window => {
                let idx = self.hovered_card.or(if self.cards.is_empty() { None } else { Some(0) });
                match idx {
                    Some(i) => self.trigger_window_action(i),
                    None => OverlayOutcome::None,
                }
            }
            TargetMode::WindowRegion => {
                let Some(idx) = self.zoomed_card else {
                    return OverlayOutcome::None;
                };
                let Some(card) = self.cards.get(idx) else {
                    return OverlayOutcome::None;
                };
                let src_w = card
                    .window
                    .preview
                    .as_ref()
                    .map(|p| p.width)
                    .unwrap_or(card.window.rect.width)
                    .max(1);
                let src_h = card
                    .window
                    .preview
                    .as_ref()
                    .map(|p| p.height)
                    .unwrap_or(card.window.rect.height)
                    .max(1);
                let local_crop = if let Some(zsel) = self.zoom_selection.filter(|r| r.width >= 4 && r.height >= 4) {
                    let sx = src_w as f64 / self.zoom_to.width.max(1) as f64;
                    let sy = src_h as f64 / self.zoom_to.height.max(1) as f64;
                    let lx = ((zsel.x - self.zoom_to.x) as f64 * sx).round().max(0.0) as i32;
                    let ly = ((zsel.y - self.zoom_to.y) as f64 * sy).round().max(0.0) as i32;
                    let lw = (zsel.width as f64 * sx).round().clamp(4.0, src_w as f64) as u32;
                    let lh = (zsel.height as f64 * sy).round().clamp(4.0, src_h as f64) as u32;
                    Rect::new(lx, ly, lw, lh)
                } else {
                    Rect::new(0, 0, src_w, src_h)
                };

                match self.action_mode {
                    ActionMode::Screenshot => {
                        if let Some(img) = extract_window_crop_or_fallback(
                            card.window.preview.as_ref(),
                            desktop,
                            card.window.rect,
                            local_crop,
                        ) {
                            OverlayOutcome::CaptureScreenshot { img, save_as }
                        } else {
                            OverlayOutcome::None
                        }
                    }
                    ActionMode::Record => {
                        let abs_rect = Rect::new(
                            card.window.rect.x + local_crop.x,
                            card.window.rect.y + local_crop.y,
                            local_crop.width,
                            local_crop.height,
                        )
                        .clamp_bounds(self.screen_w, self.screen_h);
                        OverlayOutcome::StartRecording {
                            crop: Some(abs_rect),
                            record_mic: self.mic_enabled,
                            record_desktop: self.audio_enabled,
                        }
                    }
                }
            }
        }
    }

    fn update_cursor(&mut self, backend: &mut dyn DisplayBackend, x: f64, y: f64) {
        let next = if self.hovered_btn.is_some() {
            CursorKind::Pointer
        } else if (self.target_mode == TargetMode::Window)
            || (self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_none())
        {
            if self.hovered_card.is_some() {
                CursorKind::Pointer
            } else {
                CursorKind::Default
            }
        } else if self.target_mode == TargetMode::Fullscreen {
            CursorKind::Default
        } else if let Some(ref anchor) = self.drag_anchor {
            match anchor.handle {
                DragHandle::Body => CursorKind::Move,
                DragHandle::Creating | DragHandle::None => CursorKind::Crosshair,
                _ => CursorKind::Pointer,
            }
        } else {
            let active = if self.target_mode == TargetMode::WindowRegion && self.zoomed_card.is_some() {
                self.zoom_selection
            } else {
                self.selection
            };
            match active {
                Some(r) => match hit_test_selection_handle(r, x, y) {
                    DragHandle::Body => CursorKind::Move,
                    DragHandle::None => CursorKind::Crosshair,
                    // Corner and edge handles strictly use Pointer per design spec
                    _ => CursorKind::Pointer,
                },
                None => CursorKind::Crosshair,
            }
        };

        if self.last_cursor != Some(next) {
            self.last_cursor = Some(next);
            backend.set_cursor(WindowRole::OverlayFullscreen, next);
        }
    }

    pub fn last_selection_to_persist(&self) -> Option<Rect> {
        self.selection.filter(|r| r.width >= 16 && r.height >= 16)
    }

    fn present_dirty(&mut self, backend: &mut dyn DisplayBackend, dirty: Option<Rect>) {
        self.render(dirty);
        let _ = backend.present_surface_bgra(
            WindowRole::OverlayFullscreen,
            self.canvas.width,
            self.canvas.height,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            dirty,
        );
    }

    /// Computes exact responsive pill coordinates and clickable hitboxes cleanly before drawing.
    pub fn layout_pill(&mut self) {
        self.pill_buttons.clear();
        let pad_x = 12i32;
        let btn_h = 28i32;
        let pill_h = 42i32;
        let gap = 5i32;
        let sep_w = 9i32;

        struct Spec {
            id: PillButtonId,
            label: &'static str,
            sep_after: bool,
        }

        let mut items = Vec::with_capacity(12);
        items.push(Spec { id: PillButtonId::ModeScreenshot, label: "Screenshot", sep_after: false });
        items.push(Spec { id: PillButtonId::ModeRecord, label: "Record", sep_after: true });

        items.push(Spec { id: PillButtonId::TargetSelection, label: "Selection", sep_after: false });
        items.push(Spec { id: PillButtonId::TargetWindow, label: "Window", sep_after: false });
        items.push(Spec { id: PillButtonId::TargetWindowRegion, label: "Win Region", sep_after: false });
        items.push(Spec { id: PillButtonId::TargetFullscreen, label: "Full Screen", sep_after: true });

        if self.action_mode == ActionMode::Record {
            items.push(Spec {
                id: PillButtonId::ToggleMic,
                label: if self.mic_enabled { "Mic: ON" } else { "Mic: OFF" },
                sep_after: false,
            });
            items.push(Spec {
                id: PillButtonId::ToggleAudio,
                label: if self.audio_enabled { "Audio: ON" } else { "Audio: OFF" },
                sep_after: true,
            });
            items.push(Spec { id: PillButtonId::DoneOrRecord, label: "Record", sep_after: true });
        } else {
            items.push(Spec { id: PillButtonId::DoneOrRecord, label: "Done", sep_after: false });
            items.push(Spec { id: PillButtonId::SaveAs, label: "Save As", sep_after: true });
        }

        items.push(Spec { id: PillButtonId::Settings, label: "Settings", sep_after: false });
        items.push(Spec { id: PillButtonId::Cancel, label: "Cancel", sep_after: false });

        let widths: Vec<i32> = items
            .iter()
            .map(|s| {
                let (tw, _) = self.canvas.measure_text(FontRole::Sans, 12.5, s.label);
                tw + 20
            })
            .collect();

        let content_w: i32 = widths.iter().sum::<i32>()
            + gap * (items.len() as i32 - 1)
            + items.iter().filter(|s| s.sep_after).count() as i32 * sep_w;

        let pill_w = content_w + pad_x * 2;
        let pill_x = ((self.screen_w as i32 - pill_w) / 2).max(12);
        let pill_y = (self.screen_h as i32 - 32 - pill_h).max(12);
        self.pill_rect = Rect::new(pill_x, pill_y, pill_w as u32, pill_h as u32);

        let mut cx = pill_x + pad_x;
        let btn_y = pill_y + (pill_h - btn_h) / 2;
        for (i, item) in items.iter().enumerate() {
            let bw = widths[i];
            self.pill_buttons.push(PillButtonRect {
                id: item.id,
                rect: Rect::new(cx, btn_y, bw as u32, btn_h as u32),
            });
            cx += bw + gap;
            if item.sep_after {
                cx += sep_w;
            }
        }
    }

    pub fn render(&mut self, clip: Option<Rect>) {
        let Some(desktop) = self.desktop.take() else {
            return;
        };
        let w = self.screen_w;
        let h = self.screen_h;
        let is_record = self.action_mode == ActionMode::Record;

        match self.target_mode {
            TargetMode::Fullscreen => {
                // Full screen draws zero border boxes or dimming; in Record mode show live desktop directly
                if is_record {
                    self.canvas.clear_transparent(clip);
                } else {
                    self.canvas.blit_desktop_vignette(
                        &desktop,
                        Some(Rect::new(0, 0, w, h)),
                        0,
                        clip,
                    );
                }
            }
            TargetMode::Selection => {
                let active = self.selection.filter(|r| r.width >= 2 && r.height >= 2);
                if is_record {
                    self.canvas
                        .fill_transparent_vignette(active, [0, 0, 0, 115], clip);
                } else {
                    self.canvas.blit_desktop_vignette(&desktop, active, 115, clip);
                }
                if let Some(r) = active {
                    self.draw_selection_chrome(r, r.width, r.height, clip);
                }
            }
            TargetMode::Window => {
                if is_record {
                    self.canvas
                        .fill_transparent_vignette(None, [12, 12, 18, 190], clip);
                } else {
                    self.canvas.blit_desktop_vignette(&desktop, None, 185, clip);
                    self.canvas.draw_sharp_rect(0, 0, w, h, [15, 15, 23, 130], clip);
                }
                let title = if is_record {
                    "Click a window to record  (Esc to cancel)"
                } else {
                    "Click a window to capture  (Esc to cancel)"
                };
                self.draw_overview_mosaic(title, clip);
            }
            TargetMode::WindowRegion => {
                if is_record {
                    self.canvas
                        .fill_transparent_vignette(None, [12, 12, 18, 200], clip);
                } else {
                    self.canvas.blit_desktop_vignette(&desktop, None, 185, clip);
                    self.canvas.draw_sharp_rect(0, 0, w, h, [15, 15, 23, 145], clip);
                }
                if let Some(idx) = self.zoomed_card {
                    self.draw_zoomed_window(idx, &desktop, clip);
                } else {
                    let hint = if is_record {
                        "Click a window to zoom in & select a region to record  (Esc to cancel)"
                    } else {
                        "Click a window to zoom in & select a region  (Esc to cancel)"
                    };
                    self.draw_overview_mosaic(hint, clip);
                }
            }
        }

        self.draw_pill(clip);
        self.desktop = Some(desktop);
    }

    fn draw_selection_chrome(&mut self, r: Rect, label_w: u32, label_h: u32, clip: Option<Rect>) {
        // 2px crisp #6366f1 border
        self.canvas.stroke_sharp_rect(
            r.x,
            r.y,
            r.width,
            r.height,
            2,
            [99, 102, 241, 245],
            clip,
        );

        // 8 crisp 8x8 resize handles
        if r.width >= 12 && r.height >= 12 {
            let x0 = r.x as f32;
            let y0 = r.y as f32;
            let x1 = (r.x + r.width as i32) as f32;
            let y1 = (r.y + r.height as i32) as f32;
            let cx = (x0 + x1) * 0.5;
            let cy = (y0 + y1) * 0.5;
            let pts = [
                (x0, y0),
                (cx, y0),
                (x1, y0),
                (x1, cy),
                (x1, y1),
                (cx, y1),
                (x0, y1),
                (x0, cy),
            ];
            for (hx, hy) in pts {
                self.canvas.draw_sharp_rect(
                    hx.round() as i32 - 4,
                    hy.round() as i32 - 4,
                    8,
                    8,
                    [255, 255, 255, 255],
                    clip,
                );
                self.canvas.stroke_sharp_rect(
                    hx.round() as i32 - 4,
                    hy.round() as i32 - 4,
                    8,
                    8,
                    1,
                    [99, 102, 241, 255],
                    clip,
                );
            }
        }

        // Crisp Mono dimension badge
        let txt = format!("{label_w} x {label_h} px");
        let (tw, _th) = self.canvas.measure_text(FontRole::Mono, 11.5, &txt);
        let bw = tw + 16;
        let bh = 22i32;
        let bx = r.x.clamp(8, (self.screen_w as i32 - bw - 8).max(8));
        let by = if r.y >= 34 {
            r.y - bh - 6
        } else if r.y + r.height as i32 + bh + 10 < self.pill_rect.y {
            r.y + r.height as i32 + 6
        } else {
            r.y + 8
        };

        let badge_r = Rect::new(bx, by, bw as u32, bh as u32);
        if let Some(c) = clip {
            if !rects_intersect(c, badge_r.outset(4)) {
                return;
            }
        }
        self.canvas.fill_rounded_rect(
            bx as f32,
            by as f32,
            bw as f32,
            bh as f32,
            6.0,
            [24, 24, 27, 236],
            clip,
        );
        self.canvas.stroke_rounded_rect(
            bx as f32,
            by as f32,
            bw as f32,
            bh as f32,
            6.0,
            1.0,
            [255, 255, 255, 38],
            clip,
        );
        self.canvas
            .draw_text(FontRole::Mono, 11.5, bx + 8, by + 15, &txt, [244, 244, 245, 255]);
    }

    fn draw_overview_mosaic(&mut self, header: &str, clip: Option<Rect>) {
        let (hw, _) = self.canvas.measure_text(FontRole::Sans, 14.0, header);
        let hx = ((self.screen_w as i32 - hw) / 2).max(16);
        self.canvas
            .draw_text(FontRole::Sans, 14.0, hx, 48, header, [244, 244, 245, 255]);

        if self.cards.is_empty() {
            let empty = "No open application windows detected";
            let (ew, _) = self.canvas.measure_text(FontRole::Sans, 13.0, empty);
            self.canvas.draw_text(
                FontRole::Sans,
                13.0,
                (self.screen_w as i32 - ew) / 2,
                (self.screen_h as i32) / 2,
                empty,
                [161, 161, 170, 255],
            );
            return;
        }

        let card_count = self.cards.len();
        for idx in 0..card_count {
            let card_rect = self.cards[idx].card_rect;
            let thumb_rect = self.cards[idx].thumb_rect;
            if let Some(c) = clip {
                if !rects_intersect(c, card_rect.outset(14)) {
                    continue;
                }
            }
            let hovered = self.hovered_card == Some(idx);

            self.canvas.draw_shadow_rounded_rect(
                thumb_rect.x as f32,
                thumb_rect.y as f32,
                thumb_rect.width as f32,
                thumb_rect.height as f32,
                10.0,
                12.0,
                if hovered { 130 } else { 85 },
                clip,
            );
            self.canvas.fill_rounded_rect(
                thumb_rect.x as f32,
                thumb_rect.y as f32,
                thumb_rect.width as f32,
                thumb_rect.height as f32,
                10.0,
                [24, 24, 27, 235],
                clip,
            );

            let preview_opt = self.cards[idx].window.preview.clone();
            if let Some(ref preview) = preview_opt {
                self.canvas.blit_scaled_rounded_image(
                    preview,
                    thumb_rect.x as f32,
                    thumb_rect.y as f32,
                    thumb_rect.width as f32,
                    thumb_rect.height as f32,
                    10.0,
                    255,
                    clip,
                );
            }

            let border_rgba = if hovered {
                [99, 102, 241, 255]
            } else {
                [255, 255, 255, 36]
            };
            self.canvas.stroke_rounded_rect(
                thumb_rect.x as f32,
                thumb_rect.y as f32,
                thumb_rect.width as f32,
                thumb_rect.height as f32,
                10.0,
                if hovered { 2.5 } else { 1.0 },
                border_rgba,
                clip,
            );

            // Title caption centered under card thumbnail
            let win = &self.cards[idx].window;
            let raw_title = if win.title.is_empty() {
                win.app_id.as_str()
            } else {
                win.title.as_str()
            };
            let title_short: String = if raw_title.chars().count() > 36 {
                format!("{}...", raw_title.chars().take(33).collect::<String>())
            } else {
                raw_title.to_string()
            };
            let (tw, _) = self.canvas.measure_text(FontRole::Sans, 12.0, &title_short);
            let tx = card_rect.x + ((card_rect.width as i32 - tw) / 2).max(0);
            let ty = thumb_rect.y + thumb_rect.height as i32 + 18;
            self.canvas.draw_text(
                FontRole::Sans,
                12.0,
                tx,
                ty,
                &title_short,
                if hovered { [255, 255, 255, 255] } else { [212, 212, 216, 255] },
            );
        }
    }

    fn draw_zoomed_window(&mut self, idx: usize, desktop: &PixelImage, clip: Option<Rect>) {
        let Some(card) = self.cards.get(idx).cloned() else {
            return;
        };

        let anim_t = match self.zoom_start {
            Some(s) => (s.elapsed().as_secs_f32() / (ZOOM_DURATION_MS as f32 / 1000.0)).clamp(0.0, 1.0),
            None => 1.0,
        };
        let draw_rect = if anim_t < 1.0 {
            ease_out_cubic_rect(self.zoom_from, self.zoom_to, anim_t)
        } else {
            self.zoom_to
        };

        let title = format!("Drag to select a region inside \"{}\"  (Esc to back)", card.window.title);
        let (hw, _) = self.canvas.measure_text(FontRole::Sans, 13.0, &title);
        self.canvas.draw_text(
            FontRole::Sans,
            13.0,
            ((self.screen_w as i32 - hw) / 2).max(16),
            48,
            &title,
            [244, 244, 245, 255],
        );

        self.canvas.draw_shadow_rounded_rect(
            draw_rect.x as f32,
            draw_rect.y as f32,
            draw_rect.width as f32,
            draw_rect.height as f32,
            10.0,
            16.0,
            135,
            clip,
        );

        let src_img = card.window.preview.as_ref().or_else(|| {
            let wr = card.window.rect.clamp_bounds(self.screen_w, self.screen_h);
            let _ = (wr, desktop);
            None
        });
        if let Some(img) = src_img {
            self.canvas.blit_scaled_rounded_image(
                img,
                draw_rect.x as f32,
                draw_rect.y as f32,
                draw_rect.width as f32,
                draw_rect.height as f32,
                10.0,
                255,
                clip,
            );
        } else if let Some(cropped) = desktop.crop_rect(
            card.window.rect.x.max(0) as u32,
            card.window.rect.y.max(0) as u32,
            card.window.rect.width,
            card.window.rect.height,
        ) {
            self.canvas.blit_scaled_rounded_image(
                &cropped,
                draw_rect.x as f32,
                draw_rect.y as f32,
                draw_rect.width as f32,
                draw_rect.height as f32,
                10.0,
                255,
                clip,
            );
        }

        self.canvas.stroke_rounded_rect(
            draw_rect.x as f32,
            draw_rect.y as f32,
            draw_rect.width as f32,
            draw_rect.height as f32,
            10.0,
            1.5,
            [255, 255, 255, 48],
            clip,
        );

        if anim_t >= 1.0 {
            if let Some(sel) = self.zoom_selection.filter(|r| r.width >= 2 && r.height >= 2) {
                let src_w = card
                    .window
                    .preview
                    .as_ref()
                    .map(|p| p.width)
                    .unwrap_or(card.window.rect.width)
                    .max(1);
                let src_h = card
                    .window
                    .preview
                    .as_ref()
                    .map(|p| p.height)
                    .unwrap_or(card.window.rect.height)
                    .max(1);
                let aw = ((sel.width as f64 / draw_rect.width.max(1) as f64) * src_w as f64).round() as u32;
                let ah = ((sel.height as f64 / draw_rect.height.max(1) as f64) * src_h as f64).round() as u32;
                self.draw_selection_chrome(sel, aw.max(1), ah.max(1), clip);
            }
        }
    }

    fn draw_pill(&mut self, clip: Option<Rect>) {
        if let Some(c) = clip {
            if !rects_intersect(c, self.pill_rect.outset(16)) {
                return;
            }
        }

        let p = self.pill_rect;
        self.canvas.draw_shadow_rounded_rect(
            p.x as f32,
            p.y as f32,
            p.width as f32,
            p.height as f32,
            20.0,
            14.0,
            115,
            clip,
        );
        self.canvas.fill_rounded_rect(
            p.x as f32,
            p.y as f32,
            p.width as f32,
            p.height as f32,
            20.0,
            [24, 24, 27, 242],
            clip,
        );
        self.canvas.stroke_rounded_rect(
            p.x as f32,
            p.y as f32,
            p.width as f32,
            p.height as f32,
            20.0,
            1.0,
            [255, 255, 255, 31],
            clip,
        );

        let btns = self.pill_buttons.clone();
        for (idx, b) in btns.iter().enumerate() {
            let hovered = self.hovered_btn == Some(b.id);
            let active = match b.id {
                PillButtonId::ModeScreenshot => self.action_mode == ActionMode::Screenshot,
                PillButtonId::ModeRecord => self.action_mode == ActionMode::Record,
                PillButtonId::TargetSelection => self.target_mode == TargetMode::Selection,
                PillButtonId::TargetWindow => self.target_mode == TargetMode::Window,
                PillButtonId::TargetWindowRegion => self.target_mode == TargetMode::WindowRegion,
                PillButtonId::TargetFullscreen => self.target_mode == TargetMode::Fullscreen,
                PillButtonId::ToggleMic => self.mic_enabled,
                PillButtonId::ToggleAudio => self.audio_enabled,
                _ => false,
            };

            let label = match b.id {
                PillButtonId::ModeScreenshot => "Screenshot",
                PillButtonId::ModeRecord => "Record",
                PillButtonId::TargetSelection => "Selection",
                PillButtonId::TargetWindow => "Window",
                PillButtonId::TargetWindowRegion => "Win Region",
                PillButtonId::TargetFullscreen => "Full Screen",
                PillButtonId::ToggleMic => if self.mic_enabled { "Mic: ON" } else { "Mic: OFF" },
                PillButtonId::ToggleAudio => if self.audio_enabled { "Audio: ON" } else { "Audio: OFF" },
                PillButtonId::DoneOrRecord => {
                    if self.action_mode == ActionMode::Record {
                        "Record"
                    } else {
                        "Done"
                    }
                }
                PillButtonId::SaveAs => "Save As",
                PillButtonId::Settings => "Settings",
                PillButtonId::Cancel => "Cancel",
            };

            let (bg, fg) = match b.id {
                PillButtonId::DoneOrRecord => {
                    if self.action_mode == ActionMode::Record {
                        (
                            if hovered { [220, 38, 38, 255] } else { [239, 68, 68, 255] },
                            [255, 255, 255, 255],
                        )
                    } else {
                        (
                            if hovered { [228, 228, 231, 255] } else { [255, 255, 255, 255] },
                            [9, 9, 11, 255],
                        )
                    }
                }
                PillButtonId::ToggleMic | PillButtonId::ToggleAudio => {
                    if active {
                        (
                            if hovered { [22, 163, 74, 235] } else { [22, 163, 74, 185] },
                            [255, 255, 255, 255],
                        )
                    } else {
                        (
                            if hovered { [63, 63, 70, 210] } else { [39, 39, 42, 175] },
                            [161, 161, 170, 255],
                        )
                    }
                }
                PillButtonId::TargetSelection
                | PillButtonId::TargetWindow
                | PillButtonId::TargetWindowRegion
                | PillButtonId::TargetFullscreen
                | PillButtonId::ModeScreenshot
                | PillButtonId::ModeRecord => {
                    if active {
                        ([63, 63, 70, 240], [255, 255, 255, 255])
                    } else if hovered {
                        ([63, 63, 70, 140], [244, 244, 245, 255])
                    } else {
                        ([0, 0, 0, 0], [161, 161, 170, 255])
                    }
                }
                PillButtonId::SaveAs | PillButtonId::Settings | PillButtonId::Cancel => {
                    if hovered {
                        ([63, 63, 70, 170], [255, 255, 255, 255])
                    } else {
                        ([39, 39, 42, 140], [212, 212, 216, 255])
                    }
                }
            };

            if bg[3] > 0 {
                self.canvas.fill_rounded_rect(
                    b.rect.x as f32,
                    b.rect.y as f32,
                    b.rect.width as f32,
                    b.rect.height as f32,
                    12.0,
                    bg,
                    clip,
                );
            }

            let (tw, _) = self.canvas.measure_text(FontRole::Sans, 12.5, label);
            let tx = b.rect.x + ((b.rect.width as i32 - tw) / 2).max(0);
            let ty = b.rect.y + 18;
            self.canvas.draw_text(FontRole::Sans, 12.5, tx, ty, label, fg);

            // Subtle 1px divider between logical pill button groups
            let has_sep = match b.id {
                PillButtonId::ModeRecord
                | PillButtonId::TargetFullscreen
                | PillButtonId::ToggleAudio
                | PillButtonId::SaveAs => true,
                PillButtonId::DoneOrRecord => self.action_mode == ActionMode::Record,
                _ => false,
            };
            if has_sep && idx + 1 < btns.len() {
                let sx = b.rect.x + b.rect.width as i32 + 9;
                self.canvas.draw_sharp_rect(
                    sx,
                    p.y + 11,
                    1,
                    (p.height as i32 - 22).max(4) as u32,
                    [255, 255, 255, 32],
                    clip,
                );
            }
        }
    }
}

#[inline]
fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width as i32
        && a.x + a.width as i32 > b.x
        && a.y < b.y + b.height as i32
        && a.y + a.height as i32 > b.y
}
