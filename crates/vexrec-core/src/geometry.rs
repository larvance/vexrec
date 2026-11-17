use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    pub fn from_points(p1: Point, p2: Point) -> Self {
        let x = p1.x.min(p2.x);
        let y = p1.y.min(p2.y);
        let width = (p1.x - p2.x).unsigned_abs();
        let height = (p1.y - p2.y).unsigned_abs();
        Self { x, y, width, height }
    }

    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x
            && p.x < self.x + self.width as i32
            && p.y >= self.y
            && p.y < self.y + self.height as i32
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Snaps dimensions down to macroblock even alignment (min 32px) required by VA-API/NVENC/NV12.
    pub fn snap_even(self) -> Self {
        let w = (self.width.max(32) / 2) * 2;
        let h = (self.height.max(32) / 2) * 2;
        Self::new(self.x.max(0), self.y.max(0), w, h)
    }

    /// Clamps rectangle inside `(0, 0, max_w, max_h)`.
    pub fn clamp_bounds(self, max_w: u32, max_h: u32) -> Self {
        let x = self.x.clamp(0, max_w.saturating_sub(1) as i32);
        let y = self.y.clamp(0, max_h.saturating_sub(1) as i32);
        let avail_w = max_w.saturating_sub(x as u32);
        let avail_h = max_h.saturating_sub(y as u32);
        Self::new(x, y, self.width.min(avail_w), self.height.min(avail_h))
    }

    /// Returns bounding-box union of two rectangles (used for partial SHM damage updates).
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = (self.x + self.width as i32).max(other.x + other.width as i32);
        let y2 = (self.y + self.height as i32).max(other.y + other.height as i32);
        Self::new(x1, y1, (x2 - x1).max(0) as u32, (y2 - y1).max(0) as u32)
    }

    /// Expands rectangle outward on every side by `pad` pixels.
    pub fn outset(self, pad: i32) -> Self {
        let x = self.x - pad;
        let y = self.y - pad;
        let w = (self.width as i64 + 2 * pad as i64).max(0) as u32;
        let h = (self.height as i64 + 2 * pad as i64).max(0) as u32;
        Self::new(x, y, w, h)
    }
}
