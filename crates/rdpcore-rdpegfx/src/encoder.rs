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
    let w = usize::from(width);
    let h = usize::from(height);
    let ow = usize::from(out_w);
    let oh = usize::from(out_h);
    if !ow.is_multiple_of(2) || !oh.is_multiple_of(2) {
        return Err(format!("I420 size must be even, got {ow}x{oh}"));
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

    let y_size = ow * oh;
    let uv_size = (ow / 2) * (oh / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    for row in 0..h {
        let src_row = &pixels[row * stride..row * stride + w * 4];
        for col in 0..w {
            let o = col * 4;
            let b = u32::from(src_row[o]);
            let g = u32::from(src_row[o + 1]);
            let r = u32::from(src_row[o + 2]);
            // BT.601 full-range-ish integer approx used by many RDP stacks.
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row * ow + col] = y.min(255) as u8;
        }
    }

    for row in (0..h).step_by(2) {
        let row2 = (row + 1).min(h - 1);
        for col in (0..w).step_by(2) {
            let col2 = (col + 1).min(w - 1);
            let sample = |rr: usize, cc: usize| {
                let o = rr * stride + cc * 4;
                (
                    u32::from(pixels[o + 2]),
                    u32::from(pixels[o + 1]),
                    u32::from(pixels[o]),
                )
            };
            let (r0, g0, b0) = sample(row, col);
            let (r1, g1, b1) = sample(row, col2);
            let (r2, g2, b2) = sample(row2, col);
            let (r3, g3, b3) = sample(row2, col2);
            let r = ((r0 + r1 + r2 + r3) / 4) as i32;
            let g = ((g0 + g1 + g2 + g3) / 4) as i32;
            let b = ((b0 + b1 + b2 + b3) / 4) as i32;
            let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            let uv_index = (row / 2) * (ow / 2) + (col / 2);
            u_plane[uv_index] = u;
            v_plane[uv_index] = v;
        }
    }

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
    let i420 = bgrx_to_i420(width, height, stride, pixels, out_w, out_h)?;
    let ow = usize::from(out_w);
    let oh = usize::from(out_h);
    let y_size = ow * oh;
    let uv_size = (ow / 2) * (oh / 2);
    let mut nv12 = vec![0u8; y_size + 2 * uv_size];
    nv12[..y_size].copy_from_slice(&i420[..y_size]);
    let u = &i420[y_size..y_size + uv_size];
    let v = &i420[y_size + uv_size..];
    for i in 0..uv_size {
        nv12[y_size + i * 2] = u[i];
        nv12[y_size + i * 2 + 1] = v[i];
    }
    Ok(nv12)
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
}
