//! MS-RDPNSC NSCodec encoder (YCoCg + RLE). Used with SurfaceCommands
//! (`surface_commands::SetSurfaceBits`) for macOS Windows App clients that
//! advertise only NSCodec in their bitmap codec list.

const RLE_LONG_ESCAPE: u8 = 0xFF;

/// Encode a BGRX32 framebuffer region into an NSCodec frame.
pub fn encode(
    data: &[u8],
    width: u16,
    height: u16,
    stride: usize,
    color_loss_level: u8,
) -> Vec<u8> {
    debug_assert!((1..=7).contains(&color_loss_level));

    let w = usize::from(width);
    let h = usize::from(height);
    let pixels = w * h;
    let cll = i32::from(color_loss_level);

    let mut y_plane = Vec::with_capacity(pixels);
    let mut co_plane = Vec::with_capacity(pixels);
    let mut cg_plane = Vec::with_capacity(pixels);
    let mut a_plane = Vec::with_capacity(pixels);

    for row in (0..h).rev() {
        let row_off = row * stride;
        for col in 0..w {
            let off = row_off + col * 4;
            let b = data[off];
            let g = data[off + 1];
            let r = data[off + 2];
            let (y, co, cg) = rgb_to_ycocg(r, g, b, cll);
            y_plane.push(y);
            co_plane.push(co);
            cg_plane.push(cg);
            a_plane.push(0xFF);
        }
    }

    let y_rle = rle_encode(&y_plane);
    let co_rle = rle_encode(&co_plane);
    let cg_rle = rle_encode(&cg_plane);
    let a_rle = rle_encode(&a_plane);

    let plane_len = |rle: &[u8]| u32::try_from(rle.len()).unwrap_or(u32::MAX);

    let mut out = Vec::with_capacity(20 + y_rle.len() + co_rle.len() + cg_rle.len() + a_rle.len());
    out.extend_from_slice(&plane_len(&y_rle).to_le_bytes());
    out.extend_from_slice(&plane_len(&co_rle).to_le_bytes());
    out.extend_from_slice(&plane_len(&cg_rle).to_le_bytes());
    out.extend_from_slice(&plane_len(&a_rle).to_le_bytes());
    out.push(color_loss_level);
    out.push(0); // ChromaSubsamplingLevel
    out.push(0);
    out.push(0);
    out.extend_from_slice(&y_rle);
    out.extend_from_slice(&co_rle);
    out.extend_from_slice(&cg_rle);
    out.extend_from_slice(&a_rle);
    out
}

fn rgb_to_ycocg(r: u8, g: u8, b: u8, cll: i32) -> (u8, u8, u8) {
    let ri = i32::from(r);
    let gi = i32::from(g);
    let bi = i32::from(b);
    let y = u8::try_from(((ri >> 2) + (gi >> 1) + (bi >> 2)).clamp(0, 255)).unwrap_or(0);
    let co_raw = (ri - bi) >> cll;
    let cg_raw = (-(ri >> 1) + gi - (bi >> 1)) >> cll;
    let co = i8::try_from(co_raw.clamp(i32::from(i8::MIN), i32::from(i8::MAX)))
        .unwrap_or(0)
        .cast_unsigned();
    let cg = i8::try_from(cg_raw.clamp(i32::from(i8::MIN), i32::from(i8::MAX)))
        .unwrap_or(0)
        .cast_unsigned();
    (y, co, cg)
}

fn rle_encode(plane: &[u8]) -> Vec<u8> {
    let n = plane.len();
    if n <= 4 {
        return plane.to_vec();
    }
    let body_end = n - 4;
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < body_end {
        let v = plane[i];
        let mut run = 1usize;
        while i + run < body_end && plane[i + run] == v {
            run += 1;
            if u32::try_from(run).is_err() {
                break;
            }
        }
        if run == 1 {
            out.push(v);
        } else if run <= 255 {
            out.push(v);
            out.push(v);
            out.push(u8::try_from(run - 2).unwrap_or(253));
        } else {
            out.push(v);
            out.push(v);
            out.push(RLE_LONG_ESCAPE);
            out.extend_from_slice(&u32::try_from(run).unwrap_or(u32::MAX).to_le_bytes());
        }
        i += run;
    }
    out.extend_from_slice(&plane[body_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_red_tile_has_header_and_body() {
        let data = vec![0u8, 0, 255, 0xFF].repeat(4);
        let out = encode(&data, 2, 2, 4, 3);
        assert!(out.len() >= 20);
        assert_eq!(out[16], 3);
    }
}
