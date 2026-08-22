//! H.264 encoder abstraction used by the GFX session.
//!
//! Concrete backends (OpenH264 today; VAAPI/NVENC later) implement
//! [`H264Encoder`]. The session always feeds BGRX32 host frames.

/// One encoded Access Unit in Annex B byte-stream form (start-code prefixed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAu {
    pub annex_b: Vec<u8>,
    /// Quantization parameter hint for the AVC420 metablock (0–51).
    pub qp: u8,
}

pub trait H264Encoder: Send {
    /// Encode one BGRX32 frame. `width`/`height` are the visible desktop size;
    /// the implementation may pad to a multiple of 16 internally.
    fn encode_bgrx(
        &mut self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
        force_idr: bool,
    ) -> Result<EncodedAu, String>;

    /// Drop any resolution-specific encoder state (call on desktop resize).
    fn reset(&mut self);
}

/// Align up to a multiple of 16 (H.264 macroblock).
pub fn align16(v: u16) -> u16 {
    v.saturating_add(15) & !15
}

pub(crate) fn check_bgrx_geometry(
    width: u16,
    height: u16,
    stride: usize,
    pixels: &[u8],
    out_w: u16,
    out_h: u16,
) -> Result<(usize, usize, usize, usize), String> {
    let w = usize::from(width);
    let h = usize::from(height);
    let ow = usize::from(out_w);
    let oh = usize::from(out_h);
    if !ow.is_multiple_of(2) || !oh.is_multiple_of(2) {
        return Err(format!("planar YUV size must be even, got {ow}x{oh}"));
    }
    if w > ow || h > oh {
        return Err(format!("visible {w}x{h} larger than padded {ow}x{oh}"));
    }
    let needed = h
        .saturating_sub(1)
        .saturating_mul(stride)
        .saturating_add(w.saturating_mul(4));
    if pixels.len() < needed {
        return Err(format!(
            "pixel buffer too short: have {}, need at least {needed}",
            pixels.len()
        ));
    }
    Ok((w, h, ow, oh))
}

#[inline]
fn bt601_y(r: u32, g: u32, b: u32) -> u8 {
    // BT.601 full-range-ish integer approx used by many RDP stacks.
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).min(255) as u8
}

#[inline]
fn bt601_uv(samples: [(u32, u32, u32); 4]) -> (u8, u8) {
    let r = (samples.iter().map(|s| s.0).sum::<u32>() / 4) as i32;
    let g = (samples.iter().map(|s| s.1).sum::<u32>() / 4) as i32;
    let b = (samples.iter().map(|s| s.2).sum::<u32>() / 4) as i32;
    let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    (u, v)
}

/// Reads one BGRX pixel at (row, col) as (r, g, b).
#[inline]
fn read_rgb(pixels: &[u8], stride: usize, row: usize, col: usize) -> (u32, u32, u32) {
    let o = row * stride + col * 4;
    (
        u32::from(pixels[o + 2]),
        u32::from(pixels[o + 1]),
        u32::from(pixels[o]),
    )
}

/// Walks the frame in 2x2 blocks, computing every pixel's luma and each
/// block's averaged chroma from a single read of that block's 4 pixels
/// (rather than reading every pixel once for luma and then again for
/// chroma) - halves the total pixel reads for the biggest, most
/// bandwidth-sensitive loop in the encode path. `emit_y` is called once
/// per *visible* pixel with its plane offset; `emit_uv` once per block.
#[inline]
fn for_each_block(
    w: usize,
    h: usize,
    ow: usize,
    stride: usize,
    pixels: &[u8],
    mut emit_y: impl FnMut(usize, u8),
    mut emit_uv: impl FnMut(usize, usize, u8, u8),
) {
    for row in (0..h).step_by(2) {
        let has_row2 = row + 1 < h;
        let row2 = row + (has_row2 as usize);
        for col in (0..w).step_by(2) {
            let has_col2 = col + 1 < w;
            let col2 = col + (has_col2 as usize);

            let s0 = read_rgb(pixels, stride, row, col);
            let s1 = read_rgb(pixels, stride, row, col2);
            let s2 = read_rgb(pixels, stride, row2, col);
            let s3 = read_rgb(pixels, stride, row2, col2);

            emit_y(row * ow + col, bt601_y(s0.0, s0.1, s0.2));
            if has_col2 {
                emit_y(row * ow + col + 1, bt601_y(s1.0, s1.1, s1.2));
            }
            if has_row2 {
                emit_y(row2 * ow + col, bt601_y(s2.0, s2.1, s2.2));
                if has_col2 {
                    emit_y(row2 * ow + col + 1, bt601_y(s3.0, s3.1, s3.2));
                }
            }

            let (u, v) = bt601_uv([s0, s1, s2, s3]);
            emit_uv(row / 2, col / 2, u, v);
        }
    }
}

/// Convert a BGRX32 framebuffer into planar I420, padding to `out_w`×`out_h`
/// (both even; typically 16-aligned). Padding pixels are black.
pub fn bgrx_to_i420(
    width: u16,
    height: u16,
    stride: usize,
    pixels: &[u8],
    out_w: u16,
    out_h: u16,
) -> Result<Vec<u8>, String> {
    let (w, h, ow, oh) = check_bgrx_geometry(width, height, stride, pixels, out_w, out_h)?;

    let y_size = ow * oh;
    let uv_w = ow / 2;
    let uv_size = uv_w * (oh / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    for_each_block(
        w,
        h,
        ow,
        stride,
        pixels,
        |offset, y| y_plane[offset] = y,
        |block_row, block_col, u, v| {
            let uv_index = block_row * uv_w + block_col;
            u_plane[uv_index] = u;
            v_plane[uv_index] = v;
        },
    );

    Ok(out)
}

/// Convert a BGRX32 framebuffer into contiguous NV12, padding to `out_w`×`out_h`.
pub fn bgrx_to_nv12(
    width: u16,
    height: u16,
    stride: usize,
    pixels: &[u8],
    out_w: u16,
    out_h: u16,
) -> Result<Vec<u8>, String> {
    let (w, h, ow, oh) = check_bgrx_geometry(width, height, stride, pixels, out_w, out_h)?;

    let y_size = ow * oh;
    let uv_w = ow / 2;
    let uv_size = uv_w * (oh / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];
    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    for_each_block(
        w,
        h,
        ow,
        stride,
        pixels,
        |offset, y| y_plane[offset] = y,
        |block_row, block_col, u, v| {
            let uv_index = (block_row * uv_w + block_col) * 2;
            uv_plane[uv_index] = u;
            uv_plane[uv_index + 1] = v;
        },
    );

    Ok(out)
}

/// A trivial encoder that emits a fixed fake Annex-B blob (for unit tests).
#[derive(Debug, Default)]
pub struct MockH264Encoder {
    pub frames: u32,
}

impl H264Encoder for MockH264Encoder {
    fn encode_bgrx(
        &mut self,
        _width: u16,
        _height: u16,
        _stride: usize,
        _pixels: &[u8],
        force_idr: bool,
    ) -> Result<EncodedAu, String> {
        self.frames += 1;
        let mut annex_b = vec![0x00, 0x00, 0x00, 0x01, if force_idr { 0x65 } else { 0x41 }];
        annex_b.extend_from_slice(&self.frames.to_le_bytes());
        Ok(EncodedAu { annex_b, qp: 22 })
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align16_rounds_up() {
        assert_eq!(align16(1920), 1920);
        assert_eq!(align16(1080), 1088);
        assert_eq!(align16(1), 16);
        assert_eq!(align16(0), 0);
    }

    #[test]
    fn bgrx_to_i420_size() {
        let w = 4u16;
        let h = 4u16;
        let pixels = vec![0u8; usize::from(w) * usize::from(h) * 4];
        let i420 = bgrx_to_i420(w, h, usize::from(w) * 4, &pixels, 16, 16).unwrap();
        assert_eq!(i420.len(), 16 * 16 + 2 * (8 * 8));
    }

    #[test]
    fn bgrx_to_i420_rejects_bad_geometry() {
        let pixels = vec![0u8; 4 * 4 * 4];
        assert!(bgrx_to_i420(4, 4, 16, &pixels, 15, 16).is_err()); // odd width
        assert!(bgrx_to_i420(8, 8, 32, &pixels, 16, 16).is_err()); // visible > padded
        assert!(bgrx_to_i420(4, 4, 16, &[0u8; 8], 16, 16).is_err()); // short buffer
    }

    #[test]
    fn bgrx_to_i420_black_and_white_luma() {
        let mut black = vec![0u8; 2 * 2 * 4];
        let i420 = bgrx_to_i420(2, 2, 8, &black, 2, 2).unwrap();
        // BT.601 limited-range black ≈ 16
        assert!(i420[0] <= 20);

        // White BGRX
        for px in black.chunks_exact_mut(4) {
            px[0] = 255;
            px[1] = 255;
            px[2] = 255;
        }
        let i420 = bgrx_to_i420(2, 2, 8, &black, 2, 2).unwrap();
        assert!(i420[0] >= 230);
    }

    #[test]
    fn bgrx_to_nv12_interleaves_uv() {
        let mut pixels = vec![0u8; 2 * 2 * 4];
        // Pure red → distinctive U/V
        for px in pixels.chunks_exact_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 255;
        }
        let nv12 = bgrx_to_nv12(2, 2, 8, &pixels, 2, 2).unwrap();
        assert_eq!(nv12.len(), 6); // 2x2 Y + 1x1 UV interleaved (2 bytes)
        let y_size = 4;
        let u = nv12[y_size];
        let v = nv12[y_size + 1];
        // Red: U low, V high in BT.601
        assert!(u < 128);
        assert!(v > 128);
    }

    #[test]
    fn bgrx_to_i420_and_nv12_agree_on_luma_and_chroma() {
        // A non-uniform frame (distinct per-pixel colors) so the fused
        // single-pass conversion can't accidentally agree with itself by
        // averaging identical values - checks the two independently
        // written passes (I420's planar output, NV12's interleaved
        // output) derive the same Y/U/V from the same source pixels.
        let w = 6u16;
        let h = 4u16;
        let stride = usize::from(w) * 4;
        let mut pixels = vec![0u8; stride * usize::from(h)];
        for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
            px[0] = (i * 17) as u8; // B
            px[1] = (i * 31) as u8; // G
            px[2] = (i * 53) as u8; // R
        }

        let i420 = bgrx_to_i420(w, h, stride, &pixels, w, h).unwrap();
        let nv12 = bgrx_to_nv12(w, h, stride, &pixels, w, h).unwrap();

        let y_size = usize::from(w) * usize::from(h);
        let uv_size = y_size / 4;
        assert_eq!(&i420[..y_size], &nv12[..y_size], "Y plane must match");

        let (u_plane, v_plane) = i420[y_size..].split_at(uv_size);
        for i in 0..uv_size {
            assert_eq!(nv12[y_size + i * 2], u_plane[i], "U mismatch at block {i}");
            assert_eq!(
                nv12[y_size + i * 2 + 1],
                v_plane[i],
                "V mismatch at block {i}"
            );
        }
    }

    #[test]
    fn bgrx_to_i420_handles_odd_visible_dimensions() {
        // 3x3 visible, padded to 4x4 - exercises the has_col2/has_row2
        // edge-duplication path in for_each_block for both an odd width
        // and an odd height.
        let w = 3u16;
        let h = 3u16;
        let stride = usize::from(w) * 4;
        let mut pixels = vec![0u8; stride * usize::from(h)];
        for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
            px[0] = (i * 23) as u8;
            px[1] = (i * 37) as u8;
            px[2] = (i * 41) as u8;
        }

        let i420 = bgrx_to_i420(w, h, stride, &pixels, 4, 4).unwrap();
        assert_eq!(i420.len(), 4 * 4 + 2 * (2 * 2));
        // Every visible pixel's luma must be written (non-default for at
        // least the non-black ones) - just confirm no panic/short read
        // and the padding row/column past the visible 3x3 stay zero.
        let y_plane = &i420[..16];
        assert_eq!(y_plane[3], 0, "padding column must stay black");
        assert_eq!(y_plane[12], 0, "padding row must stay black");
    }

    #[test]
    fn mock_encoder_marks_idr_nal() {
        let mut enc = MockH264Encoder::default();
        let idr = enc
            .encode_bgrx(16, 16, 64, &[0u8; 16 * 16 * 4], true)
            .unwrap();
        assert_eq!(idr.annex_b[4], 0x65);
        let p = enc
            .encode_bgrx(16, 16, 64, &[0u8; 16 * 16 * 4], false)
            .unwrap();
        assert_eq!(p.annex_b[4], 0x41);
    }
}
