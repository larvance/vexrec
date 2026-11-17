use vexrec_core::buffer::PixelImage;
use vexrec_core::geometry::{Point, Rect};
use vexrec_display::ClientWindowInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActionMode {
    #[default]
    Screenshot,
    Record,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetMode {
    #[default]
    Selection,
    Window,
    WindowRegion,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragHandle {
    None,
    Body,
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    Creating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillButtonId {
    ModeScreenshot,
    ModeRecord,
    TargetSelection,
    TargetWindow,
    TargetWindowRegion,
    TargetFullscreen,
    ToggleMic,
    ToggleAudio,
    DoneOrRecord,
    SaveAs,
    Settings,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub struct PillButtonRect {
    pub id: PillButtonId,
    pub rect: Rect,
}

#[derive(Debug, Clone)]
pub struct WindowCardLayout {
    pub index: usize,
    pub window: ClientWindowInfo,
    pub card_rect: Rect,
    pub thumb_rect: Rect,
}

#[derive(Debug, Clone, Copy)]
pub struct SelectionAnchor {
    pub handle: DragHandle,
    pub start_pointer: Point,
    pub orig_rect: Rect,
}

/// Computes minimum square distance / hit handle for an active selection box,
/// prioritizing corners first cleanly so small selections remain effortlessly resizable.
pub fn hit_test_selection_handle(rect: Rect, px: f64, py: f64) -> DragHandle {
    let x = rect.x as f64;
    let y = rect.y as f64;
    let w = rect.width as f64;
    let h = rect.height as f64;
    if w <= 2.0 || h <= 2.0 {
        return DragHandle::None;
    }

    let cx = x + w * 0.5;
    let cy = y + h * 0.5;
    let hr = (w.min(h) * 0.5).clamp(6.0, 14.0);
    let hr_sq = hr * hr;

    let corners = [
        (DragHandle::TopLeft, x, y),
        (DragHandle::TopRight, x + w, y),
        (DragHandle::BottomRight, x + w, y + h),
        (DragHandle::BottomLeft, x, y + h),
    ];
    for (handle, hx, hy) in corners {
        let dx = px - hx;
        let dy = py - hy;
        if dx * dx + dy * dy <= hr_sq {
            return handle;
        }
    }

    let edges = [
        (DragHandle::Top, cx, y),
        (DragHandle::Right, x + w, cy),
        (DragHandle::Bottom, cx, y + h),
        (DragHandle::Left, x, cy),
    ];
    for (handle, hx, hy) in edges {
        let dx = px - hx;
        let dy = py - hy;
        if dx * dx + dy * dy <= hr_sq {
            return handle;
        }
    }

    // Also allow grabbing thin borders within +/- 5px of edge lines
    let edge_tol = 6.0;
    let in_v = py >= y - edge_tol && py <= y + h + edge_tol;
    let in_h = px >= x - edge_tol && px <= x + w + edge_tol;
    if in_v && (px - x).abs() <= edge_tol {
        return DragHandle::Left;
    }
    if in_v && (px - (x + w)).abs() <= edge_tol {
        return DragHandle::Right;
    }
    if in_h && (py - y).abs() <= edge_tol {
        return DragHandle::Top;
    }
    if in_h && (py - (y + h)).abs() <= edge_tol {
        return DragHandle::Bottom;
    }

    if px >= x && px <= x + w && py >= y && py <= y + h {
        return DragHandle::Body;
    }
    DragHandle::None
}

/// Applies handle drag delta inside `[bound_x..bound_x+bound_w, bound_y..bound_y+bound_h]`.
pub fn apply_drag_delta(
    anchor: &SelectionAnchor,
    px: f64,
    py: f64,
    bounds: Rect,
) -> Rect {
    let min_x = bounds.x;
    let min_y = bounds.y;
    let max_x = bounds.x + bounds.width as i32;
    let max_y = bounds.y + bounds.height as i32;
    let sx = anchor.start_pointer.x;
    let sy = anchor.start_pointer.y;
    let dx = px.round() as i32 - sx;
    let dy = py.round() as i32 - sy;

    if anchor.handle == DragHandle::Creating {
        let cx = px.round() as i32;
        let cy = py.round() as i32;
        let x0 = sx.min(cx).clamp(min_x, max_x);
        let y0 = sy.min(cy).clamp(min_y, max_y);
        let x1 = sx.max(cx).clamp(min_x, max_x);
        let y1 = sy.max(cy).clamp(min_y, max_y);
        return Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32);
    }

    let r = anchor.orig_rect;
    let mut x0 = r.x;
    let mut y0 = r.y;
    let mut x1 = r.x + r.width as i32;
    let mut y1 = r.y + r.height as i32;

    if anchor.handle == DragHandle::Body {
        let w = r.width as i32;
        let h = r.height as i32;
        let nx = (x0 + dx).clamp(min_x, (max_x - w).max(min_x));
        let ny = (y0 + dy).clamp(min_y, (max_y - h).max(min_y));
        return Rect::new(nx, ny, r.width, r.height);
    }

    const MIN_SZ: i32 = 16;
    match anchor.handle {
        DragHandle::TopLeft => {
            x0 = (x0 + dx).clamp(min_x, (x1 - MIN_SZ).max(min_x));
            y0 = (y0 + dy).clamp(min_y, (y1 - MIN_SZ).max(min_y));
        }
        DragHandle::Top => {
            y0 = (y0 + dy).clamp(min_y, (y1 - MIN_SZ).max(min_y));
        }
        DragHandle::TopRight => {
            x1 = (x1 + dx).clamp((x0 + MIN_SZ).min(max_x), max_x);
            y0 = (y0 + dy).clamp(min_y, (y1 - MIN_SZ).max(min_y));
        }
        DragHandle::Right => {
            x1 = (x1 + dx).clamp((x0 + MIN_SZ).min(max_x), max_x);
        }
        DragHandle::BottomRight => {
            x1 = (x1 + dx).clamp((x0 + MIN_SZ).min(max_x), max_x);
            y1 = (y1 + dy).clamp((y0 + MIN_SZ).min(max_y), max_y);
        }
        DragHandle::Bottom => {
            y1 = (y1 + dy).clamp((y0 + MIN_SZ).min(max_y), max_y);
        }
        DragHandle::BottomLeft => {
            x0 = (x0 + dx).clamp(min_x, (x1 - MIN_SZ).max(min_x));
            y1 = (y1 + dy).clamp((y0 + MIN_SZ).min(max_y), max_y);
        }
        DragHandle::Left => {
            x0 = (x0 + dx).clamp(min_x, (x1 - MIN_SZ).max(min_x));
        }
        DragHandle::None | DragHandle::Body | DragHandle::Creating => {}
    }

    Rect::new(x0, y0, (x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32)
}

/// Computes an optimal responsive mosaic grid fitting every open window cleanly inside screen margins.
pub fn compute_window_mosaic(
    windows: &[ClientWindowInfo],
    screen_w: u32,
    screen_h: u32,
) -> Vec<WindowCardLayout> {
    let count = windows.len();
    if count == 0 || screen_w < 300 || screen_h < 250 {
        return Vec::new();
    }

    let margin_x = 70i32;
    let margin_top = 85i32;
    let margin_bottom = 115i32;
    let gap_x = 28i32;
    let gap_y = 36i32;

    let avail_w = (screen_w as i32 - margin_x * 2).max(200);
    let avail_h = (screen_h as i32 - margin_top - margin_bottom).max(160);

    // Choose column count that maximizes average card area
    let mut best_cols = 1usize;
    let mut best_area = 0i64;
    for cols in 1..=count.min(6) {
        let rows = (count + cols - 1) / cols;
        let cw = (avail_w - gap_x * (cols as i32 - 1)) / cols as i32;
        let ch = (avail_h - gap_y * (rows as i32 - 1)) / rows as i32;
        if cw >= 140 && ch >= 110 {
            let area = cw as i64 * ch as i64;
            if area > best_area {
                best_area = area;
                best_cols = cols;
            }
        }
    }

    let cols = best_cols.max(1);
    let rows = (count + cols - 1) / cols;
    let cell_w = (avail_w - gap_x * (cols as i32 - 1)) / cols as i32;
    let cell_h = (avail_h - gap_y * (rows as i32 - 1)) / rows as i32;
    let label_h = 26i32;
    let thumb_avail_h = (cell_h - label_h).max(60);

    let mut out = Vec::with_capacity(count);
    for (idx, win) in windows.iter().enumerate() {
        let row = idx / cols;
        let col = idx % cols;
        // Center the last row if partially filled
        let row_items = if row + 1 == rows {
            count - row * cols
        } else {
            cols
        };
        let row_total_w = row_items as i32 * cell_w + (row_items as i32 - 1) * gap_x;
        let row_start_x = (screen_w as i32 - row_total_w) / 2;

        let cx = row_start_x + col as i32 * (cell_w + gap_x);
        let cy = margin_top + row as i32 * (cell_h + gap_y);

        let src_w = win
            .preview
            .as_ref()
            .map(|p| p.width)
            .unwrap_or(win.rect.width)
            .max(1) as f32;
        let src_h = win
            .preview
            .as_ref()
            .map(|p| p.height)
            .unwrap_or(win.rect.height)
            .max(1) as f32;

        let scale = (cell_w as f32 / src_w).min(thumb_avail_h as f32 / src_h).min(1.0);
        let tw = (src_w * scale).round().max(48.0) as u32;
        let th = (src_h * scale).round().max(36.0) as u32;
        let tx = cx + (cell_w - tw as i32) / 2;
        let ty = cy + (thumb_avail_h - th as i32) / 2;

        out.push(WindowCardLayout {
            index: idx,
            window: win.clone(),
            card_rect: Rect::new(cx, cy, cell_w as u32, cell_h as u32),
            thumb_rect: Rect::new(tx, ty, tw, th),
        });
    }
    out
}

/// Computes target centered destination rect for `Window Region` zoom fit inside workspace margins.
pub fn compute_zoom_target_rect(
    src_w: u32,
    src_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Rect {
    let margin_x = 70.0f32;
    let margin_top = 85.0f32;
    let margin_bottom = 115.0f32;
    let avail_w = (screen_w as f32 - margin_x * 2.0).max(160.0);
    let avail_h = (screen_h as f32 - margin_top - margin_bottom).max(120.0);
    let sw = src_w.max(1) as f32;
    let sh = src_h.max(1) as f32;
    let scale = (avail_w / sw).min(avail_h / sh).clamp(0.15, 2.5);
    let dw = (sw * scale).round().max(64.0) as u32;
    let dh = (sh * scale).round().max(48.0) as u32;
    let dx = ((screen_w as i32 - dw as i32) / 2).max(0);
    let dy = (margin_top as i32 + ((avail_h as i32 - dh as i32) / 2)).max(0);
    Rect::new(dx, dy, dw, dh)
}

/// Smooth cubic ease-out interpolator matching legacy 220ms `1 - (1 - t)^3` animation curve.
#[inline]
pub fn ease_out_cubic_rect(from: Rect, to: Rect, t: f32) -> Rect {
    let c = t.clamp(0.0, 1.0);
    let k = 1.0 - (1.0 - c).powi(3);
    let lerp = |a: f32, b: f32| -> f32 { a + (b - a) * k };
    Rect::new(
        lerp(from.x as f32, to.x as f32).round() as i32,
        lerp(from.y as f32, to.y as f32).round() as i32,
        lerp(from.width as f32, to.width as f32).round().max(1.0) as u32,
        lerp(from.height as f32, to.height as f32).round().max(1.0) as u32,
    )
}

/// Crops `img` strictly with zero allocation overflow if bounds touch edge.
pub fn extract_window_crop_or_fallback(
    preview: Option<&PixelImage>,
    desktop: &PixelImage,
    win_rect: Rect,
    local_crop: Rect,
) -> Option<PixelImage> {
    if let Some(p) = preview {
        if let Some(cropped) = p.crop_rect(
            local_crop.x.max(0) as u32,
            local_crop.y.max(0) as u32,
            local_crop.width,
            local_crop.height,
        ) {
            return Some(cropped);
        }
    }
    let abs_x = (win_rect.x + local_crop.x).max(0) as u32;
    let abs_y = (win_rect.y + local_crop.y).max(0) as u32;
    desktop.crop_rect(abs_x, abs_y, local_crop.width, local_crop.height)
}
