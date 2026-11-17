use std::sync::Arc;
use vexrec_core::geometry::Rect;
use vexrec_display::{CursorKind, DisplayBackend, WindowRole};
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_render::SurfaceCanvas;

pub const INDICATOR_W: u32 = 112;
pub const INDICATOR_H: u32 = 28;

/// Minimalist top-right floating outlined recording timer pill (`112x28`) with click-to-stop interaction.
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
            .fill_rounded_rect(0.0, 0.0, w, h, 14.0, [14, 14, 17, 175], None);

        let (border_rgba, text_rgba) = if paused {
            ([161, 161, 170, 165], [161, 161, 170, 235])
        } else {
            ([244, 244, 245, 210], [250, 250, 250, 255])
        };
        self.canvas.stroke_rounded_rect(
            0.75,
            0.75,
            w - 1.5,
            h - 1.5,
            13.25,
            1.35,
            border_rgba,
            None,
        );

        let text = vexrec_os::format_timer_text(secs);
        let (tw, _th) = self.canvas.measure_text(FontRole::Mono, 13.0, &text);
        let tx = ((INDICATOR_W as i32 - tw) / 2).max(8);
        self.canvas
            .draw_text(FontRole::Mono, 13.0, tx, 19, &text, text_rgba);
    }
}
