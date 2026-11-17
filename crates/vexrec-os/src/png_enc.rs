use std::io::Write;
use vexrec_core::buffer::{PixelFormat, PixelImage};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::CompressionSpeed;

/// Encodes an image (`Bgrx8`, `Bgra8`, or `Rgba8`) into PNG bytes using `fdeflate`-backed fast deflate.
pub fn encode_png_bytes(img: &PixelImage, speed: CompressionSpeed) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((img.width as usize * img.height as usize) / 2);
    encode_png_writer(img, speed, &mut out)?;
    Ok(out)
}

/// Streams a PNG straight to any `Write` sink without intermediate encoding buffers for `Rgba8`.
pub fn encode_png_writer<W: Write>(
    img: &PixelImage,
    speed: CompressionSpeed,
    writer: W,
) -> Result<()> {
    let mut encoder = png::Encoder::new(writer, img.width, img.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let comp = match speed {
        CompressionSpeed::Fast => png::Compression::Fast,
        CompressionSpeed::Balanced => png::Compression::Balanced,
        CompressionSpeed::Compact => png::Compression::High,
    };
    encoder.set_compression(comp);
    encoder.set_filter(match speed {
        CompressionSpeed::Fast => png::Filter::Sub,
        CompressionSpeed::Balanced | CompressionSpeed::Compact => png::Filter::Paeth,
    });

    let mut png_writer = encoder
        .write_header()
        .map_err(|e| VexrecError::Encoding(format!("PNG header error: {e}")))?;

    let row_bytes = (img.width as usize) * 4;
    let src_stride = img.stride as usize;

    match img.format {
        PixelFormat::Rgba8 if src_stride == row_bytes => {
            png_writer
                .write_image_data(&img.as_bytes()[..row_bytes * img.height as usize])
                .map_err(|e| VexrecError::Encoding(format!("PNG encode error: {e}")))?;
        }
        PixelFormat::Rgba8 => {
            let mut tight = vec![0u8; row_bytes * img.height as usize];
            for row in 0..img.height as usize {
                let s = row * src_stride;
                let d = row * row_bytes;
                tight[d..d + row_bytes].copy_from_slice(&img.as_bytes()[s..s + row_bytes]);
            }
            png_writer
                .write_image_data(&tight)
                .map_err(|e| VexrecError::Encoding(format!("PNG encode error: {e}")))?;
        }
        PixelFormat::Bgrx8 => {
            let mut rgba = vec![0u8; row_bytes * img.height as usize];
            if src_stride == row_bytes {
                crate::simd::bgrx_to_rgba(
                    &img.as_bytes()[..row_bytes * img.height as usize],
                    &mut rgba,
                );
            } else {
                for row in 0..img.height as usize {
                    let s = row * src_stride;
                    let d = row * row_bytes;
                    crate::simd::bgrx_to_rgba(
                        &img.as_bytes()[s..s + row_bytes],
                        &mut rgba[d..d + row_bytes],
                    );
                }
            }
            png_writer
                .write_image_data(&rgba)
                .map_err(|e| VexrecError::Encoding(format!("PNG encode error: {e}")))?;
        }
        PixelFormat::Bgra8 => {
            let mut rgba = vec![0u8; row_bytes * img.height as usize];
            if src_stride == row_bytes {
                crate::simd::bgra_to_rgba_preserve_alpha(
                    &img.as_bytes()[..row_bytes * img.height as usize],
                    &mut rgba,
                );
            } else {
                for row in 0..img.height as usize {
                    let s = row * src_stride;
                    let d = row * row_bytes;
                    crate::simd::bgra_to_rgba_preserve_alpha(
                        &img.as_bytes()[s..s + row_bytes],
                        &mut rgba[d..d + row_bytes],
                    );
                }
            }
            png_writer
                .write_image_data(&rgba)
                .map_err(|e| VexrecError::Encoding(format!("PNG encode error: {e}")))?;
        }
        PixelFormat::Nv12 => {
            return Err(VexrecError::Encoding(
                "Cannot encode NV12 frame directly to PNG".to_string(),
            ));
        }
    }

    png_writer
        .finish()
        .map_err(|e| VexrecError::Encoding(format!("PNG finalize error: {e}")))?;
    Ok(())
}

/// Decodes a PNG file from disk into a tight `PixelFormat::Bgrx8` `PixelImage`.
pub fn decode_png_to_bgrx(path: &std::path::Path) -> Result<PixelImage> {
    let file = std::fs::File::open(path)?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| VexrecError::Pipeline(format!("PNG info error: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(1920 * 1080 * 4)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| VexrecError::Pipeline(format!("PNG decode frame error: {e}")))?;

    let mut img = PixelImage::new(info.width, info.height, PixelFormat::Bgrx8);
    let dst = img.as_mut_bytes();
    match info.color_type {
        png::ColorType::Rgba => {
            for (s, d) in buf[..info.buffer_size()]
                .chunks_exact(4)
                .zip(dst.chunks_exact_mut(4))
            {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
        png::ColorType::Rgb => {
            for (s, d) in buf[..info.buffer_size()]
                .chunks_exact(3)
                .zip(dst.chunks_exact_mut(4))
            {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
        _ => {}
    }
    Ok(img)
}
