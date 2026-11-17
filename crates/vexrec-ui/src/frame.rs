use std::sync::Arc;
use vexrec_core::geometry::Rect;
use vexrec_display::{DisplayBackend, WindowRole};
use vexrec_os::fonts::FontEngine;
use vexrec_render::SurfaceCanvas;

const OUTER_PAD: i32 = 4;
const BORDER_W: i32 = 2;

/// Optional exterior click-through recording border (`2px` `#ef4444` outside `crop`, off by default, never in fullscreen).
pub struct FrameController {
    canvas: SurfaceCanvas,
    visible: bool,
    outer_geom: Rect,
}

impl FrameController {
    pub fn new(fonts: Arc<FontEngine>) -> Self {
        Self {
            canvas: SurfaceCanvas::new(64, 64, fonts),
            visible: false,
            outer_geom: Rect::default(),
        }
    }

    pub fn show(&mut self, backend: &mut dyn DisplayBackend, crop: Option<Rect>, enabled: bool) {
        // Never display a border frame in Full Screen mode or when disabled in config
        let Some(r) = crop else {
            self.hide(backend);
            return;
        };
        if !enabled || r.width < 8 || r.height < 8 {
            self.hide(backend);
            return;
        }

        let gx = (r.x - OUTER_PAD).max(0);
        let gy = (r.y - OUTER_PAD).max(0);
        let gw = r.width + (OUTER_PAD * 2) as u32;
        let gh = r.height + (OUTER_PAD * 2) as u32;
        self.outer_geom = Rect::new(gx, gy, gw, gh);
        self.canvas.resize(gw, gh);
        self.canvas.clear_transparent(None);
        self.canvas
            .stroke_sharp_rect(0, 0, gw, gh, BORDER_W, [239, 68, 68, 220], None);
        self.visible = true;

        backend.show_surface(WindowRole::RecordingFrame, self.outer_geom);
        let _ = backend.present_surface_bgra(
            WindowRole::RecordingFrame,
            self.canvas.width,
            self.canvas.height,
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
            WindowRole::RecordingFrame,
            self.canvas.width,
            self.canvas.height,
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
        backend.hide_surface(WindowRole::RecordingFrame);
    }
}
