use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use fontdue::{Font, FontSettings, Metrics};
use memmap2::Mmap;
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontRole {
    Sans,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    role: FontRole,
    size_tenths: u16,
    ch: char,
}

#[derive(Clone)]
pub struct RasterGlyph {
    pub metrics: Metrics,
    pub bitmap: Vec<u8>,
}

pub struct FontEngine {
    sans: Option<Font>,
    mono: Option<Font>,
    cache: Mutex<HashMap<GlyphKey, RasterGlyph>>,
}

const SANS_CANDIDATES: &[&str] = &[
    "Inter-SemiBold.ttf",
    "Inter-Regular.ttf",
    "Inter.ttf",
    "Ubuntu-R.ttf",
    "Ubuntu-B.ttf",
    "UbuntuSans-Regular.ttf",
    "Cantarell-Regular.otf",
    "NotoSans-Regular.ttf",
    "DejaVuSans.ttf",
    "LiberationSans-Regular.ttf",
];

const MONO_CANDIDATES: &[&str] = &[
    "JetBrainsMono-Bold.ttf",
    "JetBrainsMono-Regular.ttf",
    "UbuntuMono-B.ttf",
    "UbuntuMono-R.ttf",
    "NotoSansMono-Regular.ttf",
    "DejaVuSansMono-Bold.ttf",
    "DejaVuSansMono.ttf",
    "LiberationMono-Regular.ttf",
];

fn font_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
        PathBuf::from("/run/current-system/sw/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let h = PathBuf::from(home);
        dirs.push(h.join(".local/share/fonts"));
        dirs.push(h.join(".fonts"));
    }
    dirs
}

fn find_font_file(candidates: &[&str]) -> Option<PathBuf> {
    let mut all_fonts: Vec<PathBuf> = Vec::new();
    let mut stack = font_search_dirs();

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                if ext.eq_ignore_ascii_case("ttf") || ext.eq_ignore_ascii_case("otf") {
                    all_fonts.push(p);
                }
            }
        }
    }

    for &target in candidates {
        if let Some(found) = all_fonts.iter().find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case(target))
                .unwrap_or(false)
        }) {
            return Some(found.clone());
        }
    }

    all_fonts.into_iter().next()
}

fn load_font_mmap(path: &Path) -> Option<Font> {
    let f = File::open(path).ok()?;
    // SAFETY: Font files in system directories are read-only immutable during runtime.
    let mmap = unsafe { Mmap::map(&f).ok()? };
    Font::from_bytes(mmap.as_ref(), FontSettings::default()).ok()
}

impl FontEngine {
    pub fn load_system_fonts() -> Self {
        let sans_path = find_font_file(SANS_CANDIDATES);
        let mono_path = find_font_file(MONO_CANDIDATES).or_else(|| sans_path.clone());

        debug!(?sans_path, ?mono_path, "Resolved zero-Pango system fonts");

        let sans = sans_path.as_deref().and_then(load_font_mmap);
        let mono = mono_path.as_deref().and_then(load_font_mmap);

        Self {
            sans,
            mono,
            cache: Mutex::new(HashMap::with_capacity(256)),
        }
    }

    pub fn get_glyph(&self, role: FontRole, px_size: f32, ch: char) -> Option<RasterGlyph> {
        let size_tenths = (px_size * 10.0).round().clamp(40.0, 2000.0) as u16;
        let key = GlyphKey {
            role,
            size_tenths,
            ch,
        };
        if let Some(hit) = self.cache.lock().unwrap().get(&key).cloned() {
            return Some(hit);
        }

        let font = match role {
            FontRole::Sans => self.sans.as_ref().or(self.mono.as_ref())?,
            FontRole::Mono => self.mono.as_ref().or(self.sans.as_ref())?,
        };
        let (metrics, bitmap) = font.rasterize(ch, px_size);
        let g = RasterGlyph { metrics, bitmap };
        self.cache.lock().unwrap().insert(key, g.clone());
        Some(g)
    }

    /// Computes horizontal pixel advance width and baseline ascent for a single-line string.
    pub fn measure_line(&self, role: FontRole, px_size: f32, text: &str) -> (i32, i32) {
        let mut width_f = 0.0f32;
        for ch in text.chars() {
            if let Some(g) = self.get_glyph(role, px_size, ch) {
                width_f += g.metrics.advance_width;
            } else {
                width_f += px_size * 0.5;
            }
        }
        (width_f.ceil() as i32, px_size.ceil() as i32)
    }

    /// Blits anti-aliased glyphs directly onto a `BGRA8` buffer starting at `(x, baseline_y)`.
    pub fn draw_text_bgra(
        &self,
        dst: &mut [u8],
        buf_w: i32,
        buf_h: i32,
        stride: usize,
        role: FontRole,
        px_size: f32,
        x: i32,
        baseline_y: i32,
        text: &str,
        color_rgba: [u8; 4],
    ) -> i32 {
        let mut pen_x = x as f32;
        let [cr, cg, cb, ca] = color_rgba;

        for ch in text.chars() {
            let Some(g) = self.get_glyph(role, px_size, ch) else {
                pen_x += px_size * 0.5;
                continue;
            };

            let gx = pen_x.round() as i32 + g.metrics.xmin;
            let gy = baseline_y - g.metrics.height as i32 - g.metrics.ymin;
            let gw = g.metrics.width as i32;
            let gh = g.metrics.height as i32;

            for row in 0..gh {
                let py = gy + row;
                if py < 0 || py >= buf_h {
                    continue;
                }
                let row_off = (py as usize) * stride;
                let g_row_off = (row as usize) * (gw as usize);

                for col in 0..gw {
                    let px = gx + col;
                    if px < 0 || px >= buf_w {
                        continue;
                    }
                    let cov = g.bitmap[g_row_off + col as usize];
                    if cov == 0 {
                        continue;
                    }
                    let eff_a = ((cov as u32 * ca as u32 + 128) * 257 >> 16) as u8;
                    let off = row_off + (px as usize) * 4;
                    if off + 4 <= dst.len() {
                        crate::simd::blend_pixel_bgra(&mut dst[off..off + 4], cb, cg, cr, eff_a);
                    }
                }
            }

            pen_x += g.metrics.advance_width;
        }

        (pen_x.round() as i32) - x
    }
}
