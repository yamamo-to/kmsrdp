use std::sync::Arc;
use rdpcore_server::diff::{Rect, find_dirty_rects};

use super::types::CaptureCompare;

/// Compare `src` to `prev` *before* allocating. Identical frames return an
/// empty `Arc` plus `unchanged = true` so the caller can keep the last buffer.
pub fn take_pixels(
    src: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
    force_full: bool,
    prev: Option<CaptureCompare<'_>>,
) -> (Arc<[u8]>, bool, Option<Vec<Rect>>) {
    if !force_full
        && let Some(prev) = prev
        && prev.width == width
        && prev.height == height
    {
        let dirty = find_dirty_rects(
            prev.data,
            prev.stride,
            src,
            src_stride,
            width as usize,
            height as usize,
            4,
        );
        if dirty.is_empty() {
            return (Arc::from(&[][..]), true, Some(Vec::new()));
        }
        return (Arc::from(src), false, Some(dirty));
    }
    (Arc::from(src), false, None)
}

/// Copy a tightly-or-padded BGRX source rectangle into `dst` at (`dst_x`,`dst_y`).
#[allow(clippy::too_many_arguments)]
pub fn blit_bgrx(
    dst: &mut [u8],
    dst_stride: usize,
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_stride: usize,
    src_w: u32,
    src_h: u32,
    dst_x: i32,
    dst_y: i32,
) {
    let src_w = src_w as i32;
    let src_h = src_h as i32;
    let dst_w = dst_w as i32;
    let dst_h = dst_h as i32;

    if dst_x == 0 && dst_y == 0 && src_w == dst_w && src_h == dst_h && src_stride == dst_stride {
        let total_bytes = (src_h as usize).saturating_mul(src_stride);
        if total_bytes <= src.len() && total_bytes <= dst.len() {
            dst[..total_bytes].copy_from_slice(&src[..total_bytes]);
            return;
        }
    }
    for row in 0..src_h {
        let dy = dst_y + row;
        if dy < 0 || dy >= dst_h {
            continue;
        }
        let mut src_col0 = 0i32;
        let mut dst_col0 = dst_x;
        let mut copy_w = src_w;
        if dst_col0 < 0 {
            src_col0 = -dst_col0;
            copy_w += dst_col0;
            dst_col0 = 0;
        }
        if dst_col0 + copy_w > dst_w {
            copy_w = dst_w - dst_col0;
        }
        if copy_w <= 0 || src_col0 >= src_w {
            continue;
        }
        let bytes = copy_w as usize * 4;
        let s = row as usize * src_stride + src_col0 as usize * 4;
        let d = dy as usize * dst_stride + dst_col0 as usize * 4;
        if s + bytes <= src.len() && d + bytes <= dst.len() {
            dst[d..d + bytes].copy_from_slice(&src[s..s + bytes]);
        }
    }
}
