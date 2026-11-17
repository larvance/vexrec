use cairo::Region;
use gtk::prelude::*;
use vexrec_core::geometry::Rect;

pub struct RecordingFrameWindow {
    window: gtk::Window,
}

impl RecordingFrameWindow {
    pub fn new() -> Self {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Vexrec Frame");
        window.set_decorated(false);
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_keep_above(true);
        window.set_accept_focus(false);
        window.set_app_paintable(true);

        if let Some(screen) = gtk::prelude::GtkWindowExt::screen(&window) {
            if let Some(visual) = screen.rgba_visual() {
                window.set_visual(Some(&visual));
            }
        }

        // Set input shape to empty region on map so all clicks pass through completely
        window.connect_map(|win| {
            if let Some(gdk_win) = win.window() {
                let empty_region = Region::create();
                gdk_win.input_shape_combine_region(&empty_region, 0, 0);
            }
        });

        // Draw 2px border strictly on outer perimeter without inner glow
        window.connect_draw(|_win, cr| {
            let width = _win.allocated_width() as f64;
            let height = _win.allocated_height() as f64;

            // Clear to transparent
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            let _ = cr.paint();

            // Draw outer perimeter border: 2px red (#ef4444)
            cr.set_operator(cairo::Operator::Over);
            cr.set_source_rgba(0.937, 0.267, 0.267, 0.95);
            cr.set_line_width(2.0);
            cr.rectangle(1.0, 1.0, width - 2.0, height - 2.0);
            let _ = cr.stroke();

            glib::Propagation::Proceed
        });

        Self { window }
    }

    pub fn show_frame(&self, crop: Option<Rect>) {
        // Never show a recording frame for fullscreen recording
        let Some(r) = crop else {
            self.window.hide();
            return;
        };

        const BORDER: i32 = 4;
        let px = (r.x - BORDER).max(0);
        let py = (r.y - BORDER).max(0);
        let pw = r.width as i32 + 2 * BORDER;
        let ph = r.height as i32 + 2 * BORDER;

        self.window.move_(px, py);
        self.window.resize(pw, ph);
        self.window.show_all();

        // Re-assert click-through region
        if let Some(gdk_win) = self.window.window() {
            let empty_region = Region::create();
            gdk_win.input_shape_combine_region(&empty_region, 0, 0);
        }
    }

    pub fn hide_frame(&self) {
        self.window.hide();
    }
}
