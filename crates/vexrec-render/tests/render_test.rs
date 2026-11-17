use std::sync::Arc;
use vexrec_core::buffer::{PixelFormat, PixelImage};
use vexrec_core::geometry::Rect;
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_render::SurfaceCanvas;

#[test]
fn test_canvas_vignette_cutout_and_sdf_primitives() {
    let fonts = Arc::new(FontEngine::load_system_fonts());
    let mut canvas = SurfaceCanvas::new(320, 200, fonts);

    let mut desktop = PixelImage::new(320, 200, PixelFormat::Bgrx8);
    desktop.as_mut_bytes().fill(200);

    canvas.blit_desktop_vignette(&desktop, Some(Rect::new(50, 40, 100, 80)), 115, None);

    // Pixel inside cutout must remain 100% pure untouched desktop pixel (200)
    let inside_off = (60 * canvas.stride + 80 * 4) as usize;
    assert_eq!(canvas.data.as_slice()[inside_off], 200);
    assert_eq!(canvas.data.as_slice()[inside_off + 3], 255);

    // Pixel outside cutout must be dimmed
    let outside_off = (10 * canvas.stride + 10 * 4) as usize;
    assert!(canvas.data.as_slice()[outside_off] < 130);

    // Rounded pill + drop shadow + crisp text renders without panicking
    canvas.draw_shadow_rounded_rect(20.0, 130.0, 260.0, 44.0, 20.0, 12.0, 150, None);
    canvas.fill_rounded_rect(20.0, 130.0, 260.0, 44.0, 20.0, [24, 24, 27, 242], None);
    canvas.stroke_rounded_rect(20.0, 130.0, 260.0, 44.0, 20.0, 1.0, [255, 255, 255, 31], None);
    let adv = canvas.draw_text(FontRole::Sans, 12.0, 40, 156, "Done", [255, 255, 255, 255]);
    assert!(adv > 0);
}
