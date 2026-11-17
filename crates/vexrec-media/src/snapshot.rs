use std::io::BufWriter;
use std::path::Path;
use vexrec_core::buffer::{PixelFormat, PixelImage};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::{CompressionSpeed, ImageFormat};
use vexrec_os::png_enc::encode_png_writer;
use vexrec_os::simd::{bgra_to_rgba_preserve_alpha, bgrx_to_rgba};

/// Saves a `PixelImage` to `path` in `PNG` (`fdeflate`), `JPEG` (SIMD), or `WebP` (pure Rust).
pub fn save_snapshot(
    img: &PixelImage,
    path: &Path,
    format: ImageFormat,
    speed: CompressionSpeed,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let effective_fmt = if path.extension().is_some() {
        ImageFormat::from_path(path)
    } else {
        format
    };

    let file = std::fs::File::create(path)?;
    let writer = BufWriter::with_capacity(256 * 1024, file);

    match effective_fmt {
        ImageFormat::Png => encode_png_writer(img, speed, writer),
        ImageFormat::Jpeg => {
            let rgb = to_tight_rgb(img);
            let q = match speed {
                CompressionSpeed::Fast => 88,
                CompressionSpeed::Balanced => 92,
                CompressionSpeed::Compact => 85,
            };
            let enc = jpeg_encoder::Encoder::new(writer, q);
            enc.encode(&rgb, img.width as u16, img.height as u16, jpeg_encoder::ColorType::Rgb)
                .map_err(|e| VexrecError::Encoding(format!("JPEG encode error: {e}")))
        }
        ImageFormat::Webp => {
            let rgba = to_tight_rgba(img);
            let enc = image_webp::WebPEncoder::new(writer);
            enc.encode(&rgba, img.width, img.height, image_webp::ColorType::Rgba8)
                .map_err(|e| VexrecError::Encoding(format!("WebP encode error: {e}")))
        }
    }
}

fn to_tight_rgba(img: &PixelImage) -> Vec<u8> {
    let row_bytes = (img.width as usize) * 4;
    let total = row_bytes * (img.height as usize);
    let mut rgba = vec![0u8; total];
    let src = img.as_bytes();
    let s_stride = img.stride as usize;

    for row in 0..img.height as usize {
        let s = &src[row * s_stride..row * s_stride + row_bytes];
        let d = &mut rgba[row * row_bytes..row * row_bytes + row_bytes];
        match img.format {
            PixelFormat::Rgba8 => d.copy_from_slice(s),
            PixelFormat::Bgrx8 => bgrx_to_rgba(s, d),
            PixelFormat::Bgra8 => bgra_to_rgba_preserve_alpha(s, d),
            PixelFormat::Nv12 => {}
        }
    }
    rgba
}

fn to_tight_rgb(img: &PixelImage) -> Vec<u8> {
    let w = img.width as usize;
    let h = img.height as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);
    let src = img.as_bytes();
    let s_stride = img.stride as usize;
    let is_rgba = img.format == PixelFormat::Rgba8;

    for row in 0..h {
        let row_slice = &src[row * s_stride..row * s_stride + w * 4];
        for px in row_slice.chunks_exact(4) {
            if is_rgba {
                rgb.push(px[0]);
                rgb.push(px[1]);
                rgb.push(px[2]);
            } else {
                rgb.push(px[2]);
                rgb.push(px[1]);
                rgb.push(px[0]);
            }
        }
    }
    rgb
}
