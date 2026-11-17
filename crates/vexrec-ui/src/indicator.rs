use std::sync::Arc;
use vexrec_core::geometry::Rect;
use vexrec_display::{CursorKind, DisplayBackend, WindowRole};
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_render::SurfaceCanvas;

pub const INDICATOR_W: u32 = 100;
pub const INDICATOR_H: u32 = 28;

/// Minimalist top-right floating recording timer pill (`100x28`) with click-to-stop interaction.
pub struct IndicatorController {
    canvas: SurfaceCanvas,
    visible: bool,
    last_secs: u64,
    last_paused: bool,
    last_geom: Rect,
}

impl IndicatorController {
    pub fn new(fonts: Arc<FontEngine>) -> Self {
        Self {
            canvas: SurfaceCanvas::new(INDICATOR_W, INDICATOR_H, fonts),
            visible: false,
            last_secs: u64::MAX,
            last_paused: false,
            last_geom: Rect::new(0, 0, INDICATOR_W, INDICATOR_H),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self, backend: &mut dyn DisplayBackend) {
        let screen = backend.primary_size();
        let x = (screen.width as i32 - INDICATOR_W as i32 - 20).max(16);
        let y = 38;
        self.last_geom = Rect::new(x, y, INDICATOR_W, INDICATOR_H);
        self.last_secs = u64::MAX;
        self.last_paused = false;
        self.visible = true;

        self.render(0, false);
        backend.show_surface(WindowRole::IndicatorPill, self.last_geom);
        backend.set_cursor(WindowRole::IndicatorPill, CursorKind::Pointer);
        let _ = backend.present_surface_bgra(
            WindowRole::IndicatorPill,
            INDICATOR_W,
            INDICATOR_H,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
    }

    pub fn update(&mut self, backend: &mut dyn DisplayBackend, secs: u64, paused: bool) {
        if !self.visible {
            return;
        }
        if secs == self.last_secs && paused == self.last_paused {
            return;
        }
        self.render(secs, paused);
        let _ = backend.present_surface_bgra(
            WindowRole::IndicatorPill,
            INDICATOR_W,
            INDICATOR_H,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
    }

    pub fn on_expose(&self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        let _ = backend.present_surface_bgra(
            WindowRole::IndicatorPill,
            INDICATOR_W,
            INDICATOR_H,
            self.canvas.stride,
            self.canvas.data.as_slice(),
            None,
        );
    }

    pub fn hide(&mut self, backend: &mut dyn DisplayBackend) {
        if !self.visible {
            return;
        }
        self.visible = false;
        backend.hide_surface(WindowRole::IndicatorPill);
    }

    fn render(&mut self, secs: u64, paused: bool) {
        self.last_secs = secs;
        self.last_paused = paused;
        self.canvas.clear_transparent(None);

        let w = INDICATOR_W as f32;
        let h = INDICATOR_H as f32;
        self.canvas
            .fill_rounded_rect(0.0, 0.0, w, h, 14.0, [24, 24, 27, 242], None);
        self.canvas.stroke_rounded_rect(
            0.5,
            0.5,
            w - 1.0,
            h - 1.0,
            13.5,
            1.0,
            [255, 255, 255, 36],
            None,
        );

        let dot_rgba = if paused {
            [234, 179, 8, 255]
        } else {
            [239, 68, 68, 255]
        };
        self.canvas.fill_circle(18.0, 14.0, 4.5, dot_rgba, None);

        let mins = secs / 60;
        let rem = secs % 60;
        let text = format!("{mins:02}:{rem:02}");
        self.canvas.draw_text(
            FontRole::Mono,
            12.5,
            34,
            18,
            &text,
            [244, 244, 245, 255],
        );
    }
}
