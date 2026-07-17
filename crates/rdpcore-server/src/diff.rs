//! Dirty-rect diffing between two same-sized frames: a static or
//! mostly-static screen should cost one capture+memcmp per tick, not a
//! full-frame re-encode+send every time. Tiles the frame into 64x64
//! blocks, byte-compares each, then greedily merges adjacent dirty tiles
//! into maximal rectangles (row-wise expansion, then vertical growth
//! while every tile in the row stays dirty) - the same two-pass shape
//! used by every reasonable implementation of this idea, reimplemented
//! independently here (no shared code, just the same well-known
//! approach) rather than pulled in as a dependency.

const TILE_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }
}

fn tile_differs(image1: &[u8], stride1: usize, image2: &[u8], stride2: usize, bpp: usize, tile: Rect) -> bool {
    (tile.y..tile.y + tile.height).any(|row| {
        let start1 = row * stride1 + tile.x * bpp;
        let end1 = start1 + tile.width * bpp;
        let start2 = row * stride2 + tile.x * bpp;
        let end2 = start2 + tile.width * bpp;
        image1[start1..end1] != image2[start2..end2]
    })
}

/// `image1`/`image2` must both be at least `height * stride` bytes, with
/// `stride >= width * bpp`; `bpp` is bytes per pixel (4 for BGRX32).
/// Returns the changed regions, each a multiple of one tile except at the
/// frame's right/bottom edge.
pub fn find_dirty_rects(image1: &[u8], stride1: usize, image2: &[u8], stride2: usize, width: usize, height: usize, bpp: usize) -> Vec<Rect> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let tiles_x = width.div_ceil(TILE_SIZE);
    let tiles_y = height.div_ceil(TILE_SIZE);
    let mut dirty = vec![false; tiles_x * tiles_y];
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let tile = Rect::new(
                tx * TILE_SIZE,
                ty * TILE_SIZE,
                TILE_SIZE.min(width - tx * TILE_SIZE),
                TILE_SIZE.min(height - ty * TILE_SIZE),
            );
            dirty[ty * tiles_x + tx] = tile_differs(image1, stride1, image2, stride2, bpp, tile);
        }
    }

    let mod_width = width % TILE_SIZE;
    let mod_height = height % TILE_SIZE;
    let mut rects = Vec::new();
    let mut idx = 0;
    let total = tiles_x * tiles_y;
    while idx < total {
        if !dirty[idx] {
            idx += 1;
            continue;
        }
        let start_y = idx / tiles_x;
        let start_x = idx % tiles_x;

        let mut run_width = 1;
        while start_x + run_width < tiles_x && dirty[idx + run_width] {
            run_width += 1;
        }

        let mut run_height = 1;
        'grow: while start_y + run_height < tiles_y {
            for x in 0..run_width {
                if !dirty[(start_y + run_height) * tiles_x + start_x + x] {
                    break 'grow;
                }
            }
            run_height += 1;
        }

        let pixel_width = if start_x + run_width == tiles_x && mod_width > 0 {
            (run_width - 1) * TILE_SIZE + mod_width
        } else {
            run_width * TILE_SIZE
        };
        let pixel_height = if start_y + run_height == tiles_y && mod_height > 0 {
            (run_height - 1) * TILE_SIZE + mod_height
        } else {
            run_height * TILE_SIZE
        };

        rects.push(Rect::new(start_x * TILE_SIZE, start_y * TILE_SIZE, pixel_width, pixel_height));

        for y in 0..run_height {
            for x in 0..run_width {
                dirty[(start_y + y) * tiles_x + start_x + x] = false;
            }
        }
        idx += run_width;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, fill: u8) -> Vec<u8> {
        vec![fill; width * height * 4]
    }

    #[test]
    fn identical_frames_have_no_dirty_rects() {
        let a = frame(128, 128, 0);
        let b = frame(128, 128, 0);
        assert!(find_dirty_rects(&a, 128 * 4, &b, 128 * 4, 128, 128, 4).is_empty());
    }

    #[test]
    fn single_pixel_change_dirties_exactly_one_tile() {
        let a = frame(128, 128, 0);
        let mut b = a.clone();
        let idx = (65 * 128 + 65) * 4;
        b[idx] = 1;
        let rects = find_dirty_rects(&a, 128 * 4, &b, 128 * 4, 128, 128, 4);
        assert_eq!(rects, vec![Rect::new(64, 64, 64, 64)]);
    }

    #[test]
    fn adjacent_dirty_tiles_merge_horizontally() {
        let a = frame(256, 256, 0);
        let mut b = a.clone();
        b[(65 * 256 + 65) * 4] = 1;
        b[(65 * 256 + 129) * 4] = 1;
        let rects = find_dirty_rects(&a, 256 * 4, &b, 256 * 4, 256, 256, 4);
        assert_eq!(rects, vec![Rect::new(64, 64, 128, 64)]);
    }

    #[test]
    fn edge_tile_is_clipped_to_frame_bounds() {
        let a = frame(100, 100, 0);
        let mut b = a.clone();
        b[(65 * 100 + 65) * 4] = 1;
        let rects = find_dirty_rects(&a, 100 * 4, &b, 100 * 4, 100, 100, 4);
        assert_eq!(rects, vec![Rect::new(64, 64, 36, 36)]);
    }

    #[test]
    fn fully_different_frame_yields_full_rect_area() {
        let a = frame(128, 128, 0);
        let b = frame(128, 128, 0xFF);
        let rects = find_dirty_rects(&a, 128 * 4, &b, 128 * 4, 128, 128, 4);
        let covered: usize = rects.iter().map(|r| r.width * r.height).sum();
        assert_eq!(covered, 128 * 128);
    }

    #[test]
    fn zero_sized_frame_yields_no_rects() {
        assert!(find_dirty_rects(&[], 0, &[], 0, 0, 0, 4).is_empty());
    }
}
