use std::sync::Arc;
use vexrec_core::buffer::{AlignedVec, PixelFormat, PixelImage};
use vexrec_core::geometry::Rect;
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_os::simd::{blend_pixel_bgra, fill_blend_bgra_span};

/// Analytical SDF + SIMD dirty-rect canvas writing directly to tightly packed 32-bit `BGRA8` buffers.
pub struct SurfaceCanvas {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: AlignedVec,
    fonts: Arc<FontEngine>,
}

impl SurfaceCanvas {
    pub fn new(width: u32, height: u32, fonts: Arc<FontEngine>) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        let stride = w * 4;
        Self {
            width: w,
            height: h,
            stride,
            data: AlignedVec::zeroed((stride as usize) * (h as usize)),
            fonts,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let w = width.max(1);
        let h = height.max(1);
        if self.width == w && self.height == h {
            return;
        }
        self.width = w;
        self.height = h;
        self.stride = w * 4;
        let need = (self.stride as usize) * (h as usize);
        if self.data.len() < need {
            self.data = AlignedVec::zeroed(need);
        }
    }

    pub fn fonts(&self) -> &FontEngine {
        &self.fonts
    }

    pub fn clear_transparent(&mut self, clip: Option<Rect>) {
        let r = self.effective_clip(clip);
        let slice = self.data.as_mut_slice();
        let stride = self.stride as usize;
        let x0 = r.x as usize * 4;
        let row_bytes = r.width as usize * 4;
        for py in r.y as usize..(r.y as usize + r.height as usize) {
            let off = py * stride + x0;
            if off + row_bytes <= slice.len() {
                slice[off..off + row_bytes].fill(0);
            }
        }
    }

    #[inline]
    fn effective_clip(&self, clip: Option<Rect>) -> Rect {
        match clip {
            Some(c) => c.clamp_bounds(self.width, self.height),
            None => Rect::new(0, 0, self.width, self.height),
        }
    }

    /// Blits a frozen desktop capture (`Bgrx8`/`Bgra8`) with optional Even-Odd dark vignette cutout
    /// restricted strictly to `clip` for sub-0.1ms mouse-drag partial updates.
    pub fn blit_desktop_vignette(
        &mut self,
        desktop: &PixelImage,
        cutout: Option<Rect>,
        dim_alpha: u8,
        clip: Option<Rect>,
    ) {
        let r = self.effective_clip(clip);
        if r.is_empty() {
            return;
        }
        let dst = self.data.as_mut_slice();
        let src = desktop.as_bytes();
        let dst_stride = self.stride as usize;
        let src_stride = desktop.stride as usize;

        let rx0 = r.x.max(0);
        let rx1 = (r.x + r.width as i32).min(self.width as i32).min(desktop.width as i32);
        let ry0 = r.y.max(0);
        let ry1 = (r.y + r.height as i32).min(self.height as i32).min(desktop.height as i32);
        if rx1 <= rx0 || ry1 <= ry0 {
            return;
        }

        let dim_inv = 255u32 - dim_alpha as u32;
        let (cx0, cy0, cx1, cy1) = match cutout {
            Some(c) if c.width > 0 && c.height > 0 => (
                c.x.max(rx0),
                c.y.max(ry0),
                (c.x + c.width as i32).min(rx1),
                (c.y + c.height as i32).min(ry1),
            ),
            _ => (0, 0, 0, 0),
        };
        let has_cutout = cx1 > cx0 && cy1 > cy0;

        for py in ry0..ry1 {
            let s_row = (py as usize) * src_stride;
            let d_row = (py as usize) * dst_stride;

            if has_cutout && py >= cy0 && py < cy1 {
                // Left dimmed span: [rx0 .. cx0)
                if cx0 > rx0 {
                    dim_span_copy(
                        &src[s_row + rx0 as usize * 4..s_row + cx0 as usize * 4],
                        &mut dst[d_row + rx0 as usize * 4..d_row + cx0 as usize * 4],
                        dim_inv,
                    );
                }
                // Inside selection cutout: 1:1 native BGR pixels with opaque alpha [cx0 .. cx1)
                let off_s = s_row + cx0 as usize * 4;
                let off_d = d_row + cx0 as usize * 4;
                let px_count = (cx1 - cx0) as usize;
                force_opaque_bgrx_copy(
                    &src[off_s..off_s + px_count * 4],
                    &mut dst[off_d..off_d + px_count * 4],
                );
                // Right dimmed span: [cx1 .. rx1)
                if rx1 > cx1 {
                    dim_span_copy(
                        &src[s_row + cx1 as usize * 4..s_row + rx1 as usize * 4],
                        &mut dst[d_row + cx1 as usize * 4..d_row + rx1 as usize * 4],
                        dim_inv,
                    );
                }
            } else {
                dim_span_copy(
                    &src[s_row + rx0 as usize * 4..s_row + rx1 as usize * 4],
                    &mut dst[d_row + rx0 as usize * 4..d_row + rx1 as usize * 4],
                    dim_inv,
                );
            }
        }
    }

    /// Fills the overlay canvas for live Record mode without a frozen screenshot:
    /// writes premultiplied `scrim_rgba` outside `cutout` and transparent `[0, 0, 0, 0]` inside `cutout`.
    pub fn fill_transparent_vignette(
        &mut self,
        cutout: Option<Rect>,
        scrim_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        let r = self.effective_clip(clip);
        if r.is_empty() {
            return;
        }
        let dst = self.data.as_mut_slice();
        let dst_stride = self.stride as usize;

        let rx0 = r.x.max(0);
        let rx1 = (r.x + r.width as i32).min(self.width as i32);
        let ry0 = r.y.max(0);
        let ry1 = (r.y + r.height as i32).min(self.height as i32);
        if rx1 <= rx0 || ry1 <= ry0 {
            return;
        }

        let (cx0, cy0, cx1, cy1) = match cutout {
            Some(c) if c.width > 0 && c.height > 0 => (
                c.x.max(rx0),
                c.y.max(ry0),
                (c.x + c.width as i32).min(rx1),
                (c.y + c.height as i32).min(ry1),
            ),
            _ => (0, 0, 0, 0),
        };
        let has_cutout = cx1 > cx0 && cy1 > cy0;

        let [sr, sg, sb, sa] = scrim_rgba;
        let pm = |c: u8| -> u8 { ((c as u32 * sa as u32 + 128) * 257 >> 16) as u8 };
        let scrim_word = u32::from_le_bytes([pm(sb), pm(sg), pm(sr), sa]);

        for py in ry0..ry1 {
            let d_row = (py as usize) * dst_stride;
            if has_cutout && py >= cy0 && py < cy1 {
                if cx0 > rx0 {
                    fill_u32_span(
                        &mut dst[d_row + rx0 as usize * 4..d_row + cx0 as usize * 4],
                        scrim_word,
                    );
                }
                dst[d_row + cx0 as usize * 4..d_row + cx1 as usize * 4].fill(0);
                if rx1 > cx1 {
                    fill_u32_span(
                        &mut dst[d_row + cx1 as usize * 4..d_row + rx1 as usize * 4],
                        scrim_word,
                    );
                }
            } else {
                fill_u32_span(
                    &mut dst[d_row + rx0 as usize * 4..d_row + rx1 as usize * 4],
                    scrim_word,
                );
            }
        }
    }

    /// Analytical SDF anti-aliased rounded rectangle fill (`rgba` = [R, G, B, A]).
    pub fn fill_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        if w <= 0.0 || h <= 0.0 || color_rgba[3] == 0 {
            return;
        }
        let rad = radius.min(w * 0.5).min(h * 0.5).max(0.0);
        let bx0 = (x.floor() as i32 - 1).max(0);
        let by0 = (y.floor() as i32 - 1).max(0);
        let bx1 = ((x + w).ceil() as i32 + 1).min(self.width as i32);
        let by1 = ((y + h).ceil() as i32 + 1).min(self.height as i32);

        let cr = self.effective_clip(clip);
        let x0 = bx0.max(cr.x);
        let y0 = by0.max(cr.y);
        let x1 = bx1.min(cr.x + cr.width as i32);
        let y1 = by1.min(cr.y + cr.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let [r, g, b, a] = color_rgba;
        let cx = x + w * 0.5;
        let cy = y + h * 0.5;
        let hx = w * 0.5 - rad;
        let hy = h * 0.5 - rad;

        // Fast interior span optimization for solid flat regions away from rounded corners
        let corner_top = (y + rad).ceil() as i32;
        let corner_bot = (y + h - rad).floor() as i32;

        let stride = self.stride as usize;
        let dst = self.data.as_mut_slice();

        for py in y0..y1 {
            let row_off = (py as usize) * stride;
            let p_y = py as f32 + 0.5;

            if py >= corner_top && py < corner_bot && rad > 0.0 {
                let ix0 = x.ceil() as i32;
                let ix1 = (x + w).floor() as i32;
                let span_l = ix0.max(x0);
                let span_r = ix1.min(x1);
                // Left AA edge
                for px in x0..span_l {
                    let d = sd_rounded_box(px as f32 + 0.5 - cx, p_y - cy, hx, hy, rad);
                    let cov = sdf_fill_cov(d);
                    if cov > 0 {
                        let eff_a = ((cov as u32 * a as u32 + 128) * 257 >> 16) as u8;
                        let off = row_off + (px as usize) * 4;
                        blend_pixel_bgra(&mut dst[off..off + 4], b, g, r, eff_a);
                    }
                }
                // Interior flat SIMD span
                if span_r > span_l {
                    let off_s = row_off + (span_l as usize) * 4;
                    let off_e = row_off + (span_r as usize) * 4;
                    fill_blend_bgra_span(&mut dst[off_s..off_e], b, g, r, a);
                }
                // Right AA edge
                for px in span_r.max(x0)..x1 {
                    let d = sd_rounded_box(px as f32 + 0.5 - cx, p_y - cy, hx, hy, rad);
                    let cov = sdf_fill_cov(d);
                    if cov > 0 {
                        let eff_a = ((cov as u32 * a as u32 + 128) * 257 >> 16) as u8;
                        let off = row_off + (px as usize) * 4;
                        blend_pixel_bgra(&mut dst[off..off + 4], b, g, r, eff_a);
                    }
                }
                continue;
            }

            for px in x0..x1 {
                let d = sd_rounded_box(px as f32 + 0.5 - cx, p_y - cy, hx, hy, rad);
                let cov = sdf_fill_cov(d);
                if cov == 0 {
                    continue;
                }
                let eff_a = if cov == 255 {
                    a
                } else {
                    ((cov as u32 * a as u32 + 128) * 257 >> 16) as u8
                };
                let off = row_off + (px as usize) * 4;
                blend_pixel_bgra(&mut dst[off..off + 4], b, g, r, eff_a);
            }
        }
    }

    /// Analytical SDF anti-aliased rounded rectangle border stroke.
    pub fn stroke_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        stroke_width: f32,
        color_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        if w <= 0.0 || h <= 0.0 || stroke_width <= 0.0 || color_rgba[3] == 0 {
            return;
        }
        let rad = radius.min(w * 0.5).min(h * 0.5).max(0.0);
        let half_sw = stroke_width * 0.5;
        let pad = half_sw.ceil() as i32 + 2;

        let cr = self.effective_clip(clip);
        let x0 = ((x.floor() as i32) - pad).max(cr.x);
        let y0 = ((y.floor() as i32) - pad).max(cr.y);
        let x1 = (((x + w).ceil() as i32) + pad).min(cr.x + cr.width as i32);
        let y1 = (((y + h).ceil() as i32) + pad).min(cr.y + cr.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let [r, g, b, a] = color_rgba;
        let cx = x + w * 0.5;
        let cy = y + h * 0.5;
        let hx = w * 0.5 - rad;
        let hy = h * 0.5 - rad;
        let stride = self.stride as usize;
        let dst = self.data.as_mut_slice();

        // Optimization: skip scanning pixels inside the hollow center away from the border!
        let inner_x0 = (x + half_sw + rad.max(2.0)).ceil() as i32;
        let inner_x1 = (x + w - half_sw - rad.max(2.0)).floor() as i32;
        let inner_y0 = (y + half_sw + 2.0).ceil() as i32;
        let inner_y1 = (y + h - half_sw - 2.0).floor() as i32;

        for py in y0..y1 {
            let p_y = py as f32 + 0.5;
            let row_off = (py as usize) * stride;
            let skip_middle = py >= inner_y0 && py < inner_y1 && inner_x1 > inner_x0;

            let mut px = x0;
            while px < x1 {
                if skip_middle && px == inner_x0 {
                    px = inner_x1;
                    continue;
                }
                let d = sd_rounded_box(px as f32 + 0.5 - cx, p_y - cy, hx, hy, rad);
                let ring_d = d.abs() - half_sw;
                let cov = sdf_fill_cov(ring_d);
                if cov > 0 {
                    let eff_a = if cov == 255 {
                        a
                    } else {
                        ((cov as u32 * a as u32 + 128) * 257 >> 16) as u8
                    };
                    let off = row_off + (px as usize) * 4;
                    blend_pixel_bgra(&mut dst[off..off + 4], b, g, r, eff_a);
                }
                px += 1;
            }
        }
    }

    /// Soft analytical drop shadow around a rounded box (zero extra blur pass needed).
    pub fn draw_shadow_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        spread: f32,
        max_alpha: u8,
        clip: Option<Rect>,
    ) {
        if spread <= 0.0 || max_alpha == 0 {
            return;
        }
        let rad = radius.min(w * 0.5).min(h * 0.5).max(0.0);
        let cr = self.effective_clip(clip);
        let sp_i = spread.ceil() as i32;
        let x0 = (x.floor() as i32 - sp_i).max(cr.x);
        let y0 = (y.floor() as i32 - sp_i).max(cr.y);
        let x1 = ((x + w).ceil() as i32 + sp_i).min(cr.x + cr.width as i32);
        let y1 = ((y + h).ceil() as i32 + sp_i).min(cr.y + cr.height as i32);

        let cx = x + w * 0.5;
        let cy = y + h * 0.5;
        let hx = w * 0.5 - rad;
        let hy = h * 0.5 - rad;
        let ix0 = (x + rad).ceil() as i32;
        let ix1 = (x + w - rad).floor() as i32;
        let iy0 = (y + rad).ceil() as i32;
        let iy1 = (y + h - rad).floor() as i32;
        let stride = self.stride as usize;
        let dst = self.data.as_mut_slice();

        for py in y0..y1 {
            let p_y = py as f32 + 0.5;
            let row_off = (py as usize) * stride;
            let in_y = py >= iy0 && py < iy1 && ix1 > ix0;
            let mut px = x0;
            while px < x1 {
                if in_y && px == ix0 {
                    px = ix1;
                    continue;
                }
                let d = sd_rounded_box(px as f32 + 0.5 - cx, p_y - cy, hx, hy, rad);
                if d > 0.0 && d < spread {
                    let t = 1.0 - (d / spread);
                    let falloff = t * t * (3.0 - 2.0 * t); // smooth cubic shadow ramp
                    let a = (max_alpha as f32 * falloff).round() as u8;
                    if a > 0 {
                        let off = row_off + (px as usize) * 4;
                        blend_pixel_bgra(&mut dst[off..off + 4], 0, 0, 0, a);
                    }
                }
                px += 1;
            }
        }
    }

    pub fn fill_circle(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        color_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        self.fill_rounded_rect(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
            radius,
            color_rgba,
            clip,
        );
    }

    /// Blits and scales a window/desktop `PixelImage` directly into `(dx, dy, dw, dh)` clipped by
    /// rounded corners and optional `alpha`, without allocating temporary scaled bitmaps.
    pub fn blit_scaled_rounded_image(
        &mut self,
        src: &PixelImage,
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
        corner_radius: f32,
        alpha: u8,
        clip: Option<Rect>,
    ) {
        if dw <= 1.0 || dh <= 1.0 || src.width == 0 || src.height == 0 || alpha == 0 {
            return;
        }
        let rad = corner_radius.min(dw * 0.5).min(dh * 0.5).max(0.0);
        let cr = self.effective_clip(clip);
        let x0 = (dx.floor() as i32).max(cr.x);
        let y0 = (dy.floor() as i32).max(cr.y);
        let x1 = ((dx + dw).ceil() as i32).min(cr.x + cr.width as i32);
        let y1 = ((dy + dh).ceil() as i32).min(cr.y + cr.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let cx = dx + dw * 0.5;
        let cy = dy + dh * 0.5;
        let hx = dw * 0.5 - rad;
        let hy = dh * 0.5 - rad;

        let scale_x = src.width as f32 / dw;
        let scale_y = src.height as f32 / dh;
        let src_bytes = src.as_bytes();
        let src_stride = src.stride as usize;
        let dst_bytes = self.data.as_mut_slice();
        let dst_stride = self.stride as usize;
        let is_rgba = src.format == PixelFormat::Rgba8;
        let has_alpha_px = src.format == PixelFormat::Bgra8 || src.format == PixelFormat::Rgba8;

        for py in y0..y1 {
            let p_y = py as f32 + 0.5;
            let sy = (((p_y - dy) * scale_y) as i32).clamp(0, src.height as i32 - 1) as usize;
            let s_row = sy * src_stride;
            let d_row = (py as usize) * dst_stride;

            for px in x0..x1 {
                let p_x = px as f32 + 0.5;
                let cov = if rad > 0.5 {
                    let d = sd_rounded_box(p_x - cx, p_y - cy, hx, hy, rad);
                    sdf_fill_cov(d)
                } else {
                    255
                };
                if cov == 0 {
                    continue;
                }

                let sx = (((p_x - dx) * scale_x) as i32).clamp(0, src.width as i32 - 1) as usize;
                let s_off = s_row + sx * 4;
                let (sb, sg, sr, sa) = if is_rgba {
                    (src_bytes[s_off + 2], src_bytes[s_off + 1], src_bytes[s_off], src_bytes[s_off + 3])
                } else if has_alpha_px {
                    (src_bytes[s_off], src_bytes[s_off + 1], src_bytes[s_off + 2], src_bytes[s_off + 3])
                } else {
                    (src_bytes[s_off], src_bytes[s_off + 1], src_bytes[s_off + 2], 255)
                };

                let mut eff_a = alpha;
                if sa < 255 {
                    eff_a = ((eff_a as u32 * sa as u32 + 128) * 257 >> 16) as u8;
                }
                if cov < 255 {
                    eff_a = ((eff_a as u32 * cov as u32 + 128) * 257 >> 16) as u8;
                }
                let d_off = d_row + (px as usize) * 4;
                blend_pixel_bgra(&mut dst_bytes[d_off..d_off + 4], sb, sg, sr, eff_a);
            }
        }
    }

    pub fn draw_sharp_rect(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        color_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        let cr = self.effective_clip(clip);
        let x0 = x.max(cr.x);
        let y0 = y.max(cr.y);
        let x1 = (x + w as i32).min(cr.x + cr.width as i32);
        let y1 = (y + h as i32).min(cr.y + cr.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let [r, g, b, a] = color_rgba;
        let stride = self.stride as usize;
        let dst = self.data.as_mut_slice();
        for py in y0..y1 {
            let off_s = (py as usize) * stride + (x0 as usize) * 4;
            let off_e = (py as usize) * stride + (x1 as usize) * 4;
            fill_blend_bgra_span(&mut dst[off_s..off_e], b, g, r, a);
        }
    }

    pub fn stroke_sharp_rect(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        thickness: i32,
        color_rgba: [u8; 4],
        clip: Option<Rect>,
    ) {
        if w == 0 || h == 0 || thickness <= 0 {
            return;
        }
        let t = thickness as u32;
        self.draw_sharp_rect(x, y, w, t.min(h), color_rgba, clip);
        self.draw_sharp_rect(x, y + h as i32 - t as i32, w, t.min(h), color_rgba, clip);
        if h > 2 * t {
            self.draw_sharp_rect(x, y + t as i32, t.min(w), h - 2 * t, color_rgba, clip);
            self.draw_sharp_rect(x + w as i32 - t as i32, y + t as i32, t.min(w), h - 2 * t, color_rgba, clip);
        }
    }

    pub fn draw_text(
        &mut self,
        role: FontRole,
        px_size: f32,
        x: i32,
        baseline_y: i32,
        text: &str,
        color_rgba: [u8; 4],
    ) -> i32 {
        self.fonts.draw_text_bgra(
            self.data.as_mut_slice(),
            self.width as i32,
            self.height as i32,
            self.stride as usize,
            role,
            px_size,
            x,
            baseline_y,
            text,
            color_rgba,
        )
    }

    pub fn measure_text(&self, role: FontRole, px_size: f32, text: &str) -> (i32, i32) {
        self.fonts.measure_line(role, px_size, text)
    }
}

#[inline(always)]
fn sd_rounded_box(px: f32, py: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = px.abs() - hx;
    let qy = py.abs() - hy;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

#[inline(always)]
fn sdf_fill_cov(sd: f32) -> u8 {
    // 1px analytical anti-aliasing ramp [-0.5 .. +0.5]
    if sd <= -0.5 {
        255
    } else if sd >= 0.5 {
        0
    } else {
        ((0.5 - sd) * 255.0).round().clamp(0.0, 255.0) as u8
    }
}

#[inline]
fn dim_span_copy(src: &[u8], dst: &mut [u8], keep_factor_255: u32) {
    // Multiply RGB by `keep_factor_255 / 255` cleanly and force alpha = 255
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = ((s[0] as u32 * keep_factor_255 + 128) * 257 >> 16) as u8;
        d[1] = ((s[1] as u32 * keep_factor_255 + 128) * 257 >> 16) as u8;
        d[2] = ((s[2] as u32 * keep_factor_255 + 128) * 257 >> 16) as u8;
        d[3] = 255;
    }
}

#[inline]
fn force_opaque_bgrx_copy(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let w = u32::from_le_bytes([s[0], s[1], s[2], 255]);
        d.copy_from_slice(&w.to_le_bytes());
    }
}

#[inline]
fn fill_u32_span(dst_row: &mut [u8], word: u32) {
    let bytes = word.to_le_bytes();
    let (prefix, aligned, suffix) = unsafe { dst_row.align_to_mut::<u32>() };
    for chunk in prefix.chunks_exact_mut(4) {
        chunk.copy_from_slice(&bytes);
    }
    aligned.fill(word);
    for chunk in suffix.chunks_exact_mut(4) {
        chunk.copy_from_slice(&bytes);
    }
}
