//! SIMD-accelerated pixel format conversions, BT.709 YUV420 NV12 packing, and SDF span blending.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// Converts X11 32-bit `BGRx` (alpha forced to `0xFF`) to canonical `RGBA` (R, G, B, 255).
pub fn bgrx_to_rgba(src: &[u8], dst: &mut [u8]) {
    let count = (src.len() / 4).min(dst.len() / 4);
    let src_bytes = &src[..count * 4];
    let dst_bytes = &mut dst[..count * 4];

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 availability verified above; slices match `count * 4`.
            unsafe { bgrx_to_rgba_avx2(src_bytes, dst_bytes) };
            return;
        }
        if is_x86_feature_detected!("ssse3") {
            // SAFETY: SSSE3 availability verified above; slices match `count * 4`.
            unsafe { bgrx_to_rgba_ssse3(src_bytes, dst_bytes) };
            return;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is baseline on AArch64.
        unsafe { bgrx_to_rgba_neon(src_bytes, dst_bytes) };
        return;
    }

    bgrx_to_rgba_scalar(src_bytes, dst_bytes);
}

#[inline]
pub fn bgrx_to_rgba_scalar(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
    }
}

/// Converts `BGRA` to `RGBA` while preserving the alpha channel.
pub fn bgra_to_rgba_preserve_alpha(src: &[u8], dst: &mut [u8]) {
    let count = (src.len() / 4).min(dst.len() / 4);
    let src_bytes = &src[..count * 4];
    let dst_bytes = &mut dst[..count * 4];

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 checked at runtime.
            unsafe { bgra_to_rgba_avx2(src_bytes, dst_bytes) };
            return;
        }
    }

    for (s, d) in src_bytes.chunks_exact(4).zip(dst_bytes.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bgrx_to_rgba_avx2(src: &[u8], dst: &mut [u8]) {
    let shuf = _mm256_setr_epi8(
        2, 1, 0, -1, 6, 5, 4, -1, 10, 9, 8, -1, 14, 13, 12, -1,
        2, 1, 0, -1, 6, 5, 4, -1, 10, 9, 8, -1, 14, 13, 12, -1,
    );
    let alpha_mask = _mm256_set1_epi32(0xFF00_0000u32 as i32);

    let mut i = 0usize;
    while i + 32 <= src.len() {
        let v = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
        let swizzled = _mm256_or_si256(_mm256_shuffle_epi8(v, shuf), alpha_mask);
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, swizzled);
        i += 32;
    }
    if i < src.len() {
        bgrx_to_rgba_scalar(&src[i..], &mut dst[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bgra_to_rgba_avx2(src: &[u8], dst: &mut [u8]) {
    let shuf = _mm256_setr_epi8(
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
    );
    let mut i = 0usize;
    while i + 32 <= src.len() {
        let v = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
        let swizzled = _mm256_shuffle_epi8(v, shuf);
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, swizzled);
        i += 32;
    }
    for (s, d) in src[i..].chunks_exact(4).zip(dst[i..].chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn bgrx_to_rgba_ssse3(src: &[u8], dst: &mut [u8]) {
    let shuf = _mm_setr_epi8(2, 1, 0, -1, 6, 5, 4, -1, 10, 9, 8, -1, 14, 13, 12, -1);
    let alpha_mask = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let mut i = 0usize;
    while i + 16 <= src.len() {
        let v = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let swizzled = _mm_or_si128(_mm_shuffle_epi8(v, shuf), alpha_mask);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, swizzled);
        i += 16;
    }
    if i < src.len() {
        bgrx_to_rgba_scalar(&src[i..], &mut dst[i..]);
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn bgrx_to_rgba_neon(src: &[u8], dst: &mut [u8]) {
    use std::arch::aarch64::*;
    let mut i = 0usize;
    let alpha = vdupq_n_u8(255);
    while i + 64 <= src.len() {
        let bgra = vld4q_u8(src.as_ptr().add(i));
        let rgba = uint8x16x4_t(bgra.2, bgra.1, bgra.0, alpha);
        vst4q_u8(dst.as_mut_ptr().add(i), rgba);
        i += 64;
    }
    if i < src.len() {
        bgrx_to_rgba_scalar(&src[i..], &mut dst[i..]);
    }
}

/// Converts packed `BGRA8`/`BGRx8` frame into BT.709 limited-range `NV12` (`Y` + interleaved `UV`).
/// Requires even `width` and `height`.
pub fn bgra_to_nv12_bt709(
    src_bgra: &[u8],
    width: u32,
    height: u32,
    src_stride: u32,
    y_plane: &mut [u8],
    y_stride: u32,
    uv_plane: &mut [u8],
    uv_stride: u32,
) {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 checked at runtime; `w` and `h` are even.
            unsafe {
                bgra_to_nv12_bt709_avx2(
                    src_bgra,
                    w,
                    h,
                    src_stride as usize,
                    y_plane,
                    y_stride as usize,
                    uv_plane,
                    uv_stride as usize,
                );
            }
            return;
        }
    }

    bgra_to_nv12_bt709_scalar(
        src_bgra,
        w,
        h,
        src_stride as usize,
        y_plane,
        y_stride as usize,
        uv_plane,
        uv_stride as usize,
    );
}

#[inline]
fn rgb_to_y_bt709(r: i32, g: i32, b: i32) -> u8 {
    // BT.709 Studio Swing: Y = ((47 * R + 157 * G + 16 * B + 128) >> 8) + 16
    ((((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235)) as u8
}

#[inline]
fn rgb_to_uv_bt709(r: i32, g: i32, b: i32) -> (u8, u8) {
    // BT.709 Studio Swing:
    // U = ((-26 * R - 87 * G + 112 * B + 128) >> 8) + 128
    // V = ((112 * R - 102 * G - 10 * B + 128) >> 8) + 128
    let u = ((((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240)) as u8;
    let v = ((((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240)) as u8;
    (u, v)
}

#[allow(clippy::too_many_arguments)]
pub fn bgra_to_nv12_bt709_scalar(
    src_bgra: &[u8],
    w: usize,
    h: usize,
    src_stride: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) {
    for row in (0..h).step_by(2) {
        let s0 = &src_bgra[row * src_stride..];
        let s1 = &src_bgra[(row + 1) * src_stride..];
        let (y_head, y_tail) = y_plane.split_at_mut((row + 1) * y_stride);
        let y0 = &mut y_head[row * y_stride..];
        let y1 = y_tail;
        let uv = &mut uv_plane[(row / 2) * uv_stride..];

        for col in (0..w).step_by(2) {
            let o0 = col * 4;
            let o1 = (col + 1) * 4;

            let (b00, g00, r00) = (s0[o0] as i32, s0[o0 + 1] as i32, s0[o0 + 2] as i32);
            let (b01, g01, r01) = (s0[o1] as i32, s0[o1 + 1] as i32, s0[o1 + 2] as i32);
            let (b10, g10, r10) = (s1[o0] as i32, s1[o0 + 1] as i32, s1[o0 + 2] as i32);
            let (b11, g11, r11) = (s1[o1] as i32, s1[o1 + 1] as i32, s1[o1 + 2] as i32);

            y0[col] = rgb_to_y_bt709(r00, g00, b00);
            y0[col + 1] = rgb_to_y_bt709(r01, g01, b01);
            y1[col] = rgb_to_y_bt709(r10, g10, b10);
            y1[col + 1] = rgb_to_y_bt709(r11, g11, b11);

            let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;

            let (u, v) = rgb_to_uv_bt709(r_avg, g_avg, b_avg);
            uv[col] = u;
            uv[col + 1] = v;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn bgra_to_nv12_bt709_avx2(
    src_bgra: &[u8],
    w: usize,
    h: usize,
    src_stride: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) {
    // 256-bit fixed-point Y kernel processes 16 pixels (64 src bytes) per step.
    // Coefficients stored as i8 / i16 pairs matching (B, G, R, A):
    // Y = (16*B + 157*G + 47*R + 128) >> 8 + 16 -> split G(157) fits unsigned u8 maddubs if folded,
    // so use i16 _mm256_madd_epi16 on unpacked 16-bit channels.
    let coeff_y = _mm256_setr_epi16(
        16, 157, 47, 0, 16, 157, 47, 0,
        16, 157, 47, 0, 16, 157, 47, 0,
    );
    let coeff_u = _mm256_setr_epi16(
        112, -87, -26, 0, 112, -87, -26, 0,
        112, -87, -26, 0, 112, -87, -26, 0,
    );
    let coeff_v = _mm256_setr_epi16(
        -10, -102, 112, 0, -10, -102, 112, 0,
        -10, -102, 112, 0, -10, -102, 112, 0,
    );
    let round_128 = _mm256_set1_epi32(128);
    let bias_16 = _mm256_set1_epi16(16);
    let bias_128 = _mm256_set1_epi16(128);
    let zero = _mm256_setzero_si256();
    let y_min = _mm256_set1_epi8(16);
    let y_max = _mm256_set1_epi8(235u8 as i8);
    let uv_max = _mm256_set1_epi8(240u8 as i8);

    #[inline(always)]
    unsafe fn y_row_8px(
        p: *const u8,
        coeff_y: __m256i,
        round_128: __m256i,
        bias_16: __m256i,
        zero: __m256i,
    ) -> __m128i {
        let px_lo = _mm256_cvtepu8_epi16(_mm_loadu_si128(p as *const __m128i));
        let px_hi = _mm256_cvtepu8_epi16(_mm_loadu_si128(p.add(16) as *const __m128i));

        // madd_epi16 gives 4 i32s for lo [B*16+G*157, R*47+0] per pixel -> phadd collapses to 4 pixels
        let p_lo = _mm256_madd_epi16(px_lo, coeff_y);
        let p_hi = _mm256_madd_epi16(px_hi, coeff_y);
        let sum_lo = _mm256_hadd_epi32(p_lo, p_hi);
        // Fix AVX2 128-bit lane permute after hadd
        let sum = _mm256_permute4x64_epi64(sum_lo, 0b11_01_10_00);
        let y32 = _mm256_srai_epi32(_mm256_add_epi32(sum, round_128), 8);
        let y16 = _mm256_add_epi16(_mm256_packs_epi32(y32, zero), bias_16);
        let y16_fixed = _mm256_permute4x64_epi64(y16, 0b11_01_10_00);
        _mm256_castsi256_si128(_mm256_packus_epi16(y16_fixed, zero))
    }

    for row in (0..h).step_by(2) {
        let s0_ptr = src_bgra.as_ptr().add(row * src_stride);
        let s1_ptr = src_bgra.as_ptr().add((row + 1) * src_stride);
        let y0_ptr = y_plane.as_mut_ptr().add(row * y_stride);
        let y1_ptr = y_plane.as_mut_ptr().add((row + 1) * y_stride);
        let uv_ptr = uv_plane.as_mut_ptr().add((row / 2) * uv_stride);

        let mut col = 0usize;
        while col + 16 <= w {
            let p0 = s0_ptr.add(col * 4);
            let p1 = s1_ptr.add(col * 4);

            // Compute 16 Y pixels for row 0 & row 1
            let y0_a = y_row_8px(p0, coeff_y, round_128, bias_16, zero);
            let y0_b = y_row_8px(p0.add(32), coeff_y, round_128, bias_16, zero);
            let y0_vec = _mm_unpacklo_epi64(y0_a, y0_b);
            let y0_clamped = _mm_min_epu8(_mm_max_epu8(y0_vec, _mm256_castsi256_si128(y_min)), _mm256_castsi256_si128(y_max));
            _mm_storeu_si128(y0_ptr.add(col) as *mut __m128i, y0_clamped);

            let y1_a = y_row_8px(p1, coeff_y, round_128, bias_16, zero);
            let y1_b = y_row_8px(p1.add(32), coeff_y, round_128, bias_16, zero);
            let y1_vec = _mm_unpacklo_epi64(y1_a, y1_b);
            let y1_clamped = _mm_min_epu8(_mm_max_epu8(y1_vec, _mm256_castsi256_si128(y_min)), _mm256_castsi256_si128(y_max));
            _mm_storeu_si128(y1_ptr.add(col) as *mut __m128i, y1_clamped);

            // Compute 8x (U, V) chroma pairs for 16px wide 2x2 blocks:
            // Average top-bottom with _mm256_avg_epu8? No, keep exact `(a+b+c+d+2)>>2` to match scalar 1:1.
            for block_half in 0..2 {
                let off = block_half * 32;
                let r0 = _mm256_loadu_si256(p0.add(off) as *const __m256i);
                let r1 = _mm256_loadu_si256(p1.add(off) as *const __m256i);

                let r0_lo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(r0));
                let r0_hi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(r0, 1));
                let r1_lo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(r1));
                let r1_hi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(r1, 1));

                let sum_lo = _mm256_add_epi16(r0_lo, r1_lo);
                let sum_hi = _mm256_add_epi16(r0_hi, r1_hi);
                // Adjacent pair pixels sit inside 64-bit chunks: swap 64-bit halves inside 128-bit lanes
                let pair_lo = _mm256_add_epi16(sum_lo, _mm256_shuffle_epi32(sum_lo, 0b01_00_11_10));
                let pair_hi = _mm256_add_epi16(sum_hi, _mm256_shuffle_epi32(sum_hi, 0b01_00_11_10));
                let two = _mm256_set1_epi16(2);
                let avg_lo = _mm256_srli_epi16(_mm256_add_epi16(pair_lo, two), 2);
                let avg_hi = _mm256_srli_epi16(_mm256_add_epi16(pair_hi, two), 2);

                // Extract 1 pixel (BGRA 16-bit) per pair -> 4 averaged pixels packed in one __m256i
                let avg4 = _mm256_unpacklo_epi64(avg_lo, avg_hi);
                let u_p = _mm256_madd_epi16(avg4, coeff_u);
                let v_p = _mm256_madd_epi16(avg4, coeff_v);

                let uv_hadd = _mm256_hadd_epi32(u_p, v_p);
                let uv32 = _mm256_srai_epi32(_mm256_add_epi32(uv_hadd, round_128), 8);
                // uv32 has 4 U results + 4 V results per half; pack and interleave U,V
                let u_a = _mm256_extract_epi32(uv32, 0) + 128;
                let u_b = _mm256_extract_epi32(uv32, 1) + 128;
                let v_a = _mm256_extract_epi32(uv32, 2) + 128;
                let v_b = _mm256_extract_epi32(uv32, 3) + 128;
                let u_c = _mm256_extract_epi32(uv32, 4) + 128;
                let u_d = _mm256_extract_epi32(uv32, 5) + 128;
                let v_c = _mm256_extract_epi32(uv32, 6) + 128;
                let v_d = _mm256_extract_epi32(uv32, 7) + 128;

                let uv_out_off = col + block_half * 8;
                *uv_ptr.add(uv_out_off) = u_a.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 1) = v_a.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 2) = u_c.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 3) = v_c.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 4) = u_b.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 5) = v_b.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 6) = u_d.clamp(16, 240) as u8;
                *uv_ptr.add(uv_out_off + 7) = v_d.clamp(16, 240) as u8;
            }
            let _ = (bias_128, uv_max);

            col += 16;
        }

        // Scalar tail
        if col < w {
            let s0 = &src_bgra[row * src_stride..];
            let s1 = &src_bgra[(row + 1) * src_stride..];
            let (y_head, y_tail) = y_plane.split_at_mut((row + 1) * y_stride);
            let y0 = &mut y_head[row * y_stride..];
            let y1 = y_tail;
            let uv = &mut uv_plane[(row / 2) * uv_stride..];
            while col < w {
                let o0 = col * 4;
                let o1 = (col + 1) * 4;
                let (b00, g00, r00) = (s0[o0] as i32, s0[o0 + 1] as i32, s0[o0 + 2] as i32);
                let (b01, g01, r01) = (s0[o1] as i32, s0[o1 + 1] as i32, s0[o1 + 2] as i32);
                let (b10, g10, r10) = (s1[o0] as i32, s1[o0 + 1] as i32, s1[o0 + 2] as i32);
                let (b11, g11, r11) = (s1[o1] as i32, s1[o1 + 1] as i32, s1[o1 + 2] as i32);

                y0[col] = rgb_to_y_bt709(r00, g00, b00);
                y0[col + 1] = rgb_to_y_bt709(r01, g01, b01);
                y1[col] = rgb_to_y_bt709(r10, g10, b10);
                y1[col + 1] = rgb_to_y_bt709(r11, g11, b11);

                let (u, v) = rgb_to_uv_bt709(
                    (r00 + r01 + r10 + r11 + 2) >> 2,
                    (g00 + g01 + g10 + g11 + 2) >> 2,
                    (b00 + b01 + b10 + b11 + 2) >> 2,
                );
                uv[col] = u;
                uv[col + 1] = v;
                col += 2;
            }
        }
    }
}

/// Blends a single solid `BGRA8` source pixel over a contiguous `BGRA8` destination row with coverage `[0..255]`.
#[inline]
pub fn blend_pixel_bgra(dst: &mut [u8], b: u8, g: u8, r: u8, a: u8) {
    if a == 0 {
        return;
    }
    if a == 255 {
        dst[0] = b;
        dst[1] = g;
        dst[2] = r;
        dst[3] = 255;
        return;
    }
    let sa = a as u32;
    let inv = 255 - sa;
    dst[0] = ((b as u32 * sa + dst[0] as u32 * inv + 128) * 257 >> 16) as u8;
    dst[1] = ((g as u32 * sa + dst[1] as u32 * inv + 128) * 257 >> 16) as u8;
    dst[2] = ((r as u32 * sa + dst[2] as u32 * inv + 128) * 257 >> 16) as u8;
    dst[3] = ((sa * 255 + dst[3] as u32 * inv + 128) * 257 >> 16).min(255) as u8;
}

/// Fills a horizontal `BGRA8` pixel span with a uniform 32-bit `BGRA` word blended at `alpha`.
pub fn fill_blend_bgra_span(dst_row: &mut [u8], b: u8, g: u8, r: u8, alpha: u8) {
    if alpha == 0 {
        return;
    }
    if alpha == 255 {
        let word = u32::from_le_bytes([b, g, r, 255]);
        let (prefix, aligned, suffix) = unsafe { dst_row.align_to_mut::<u32>() };
        for chunk in prefix.chunks_exact_mut(4) {
            chunk[0] = b;
            chunk[1] = g;
            chunk[2] = r;
            chunk[3] = 255;
        }
        aligned.fill(word);
        for chunk in suffix.chunks_exact_mut(4) {
            chunk[0] = b;
            chunk[1] = g;
            chunk[2] = r;
            chunk[3] = 255;
        }
        return;
    }
    for px in dst_row.chunks_exact_mut(4) {
        blend_pixel_bgra(px, b, g, r, alpha);
    }
}
