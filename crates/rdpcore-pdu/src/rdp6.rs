//! RDP 6.0 "Planar" bitmap compression (MS-RDPEGDI 3.1.9): per-plane
//! vertical-delta filtering followed by a byte-oriented run-length
//! encoding, used to compress classic `TS_BITMAP_DATA` bitmap updates -
//! carried through the existing `BITMAP_COMPRESSION` flag path (see
//! `fastpath::BitmapRect`), not a separately-negotiated codec/capability.
//!
//! Scope: ARGB mode only (no YCoCg color transform, no chroma
//! subsampling, no alpha plane) - `FormatHeader = 0x30` (RLE enabled,
//! no-alpha, color-loss-level 0). This is the simplest configuration and
//! requires no capability beyond the classic compression flag every
//! RDP6-capable client already accepts unconditionally.
//!
//! This crate only ever needs to *encode* (this server never receives a
//! compressed bitmap from a client) - `decode` exists purely so the
//! encoder can be round-trip tested without a live client.

use crate::DecodeError;
use crate::cursor::ReadCursor;

pub const FORMAT_HEADER_RLE_NO_ALPHA_ARGB: u8 = 0x30;

const MAX_STACK_PIXELS: usize = 4096;

/// Encodes one BGRX32 tile (4 bytes/pixel, X ignored) into an RDP6 Planar
/// bitmap stream: `[FormatHeader][R-plane][G-plane][B-plane]`, writing into
/// the caller-provided `out` buffer (cleared before writing) to avoid
/// per-frame heap allocations on hot paths.
///
/// `bgrx` must already be in the same row order the caller intends the
/// decoder to reconstruct (this codec has no notion of "bottom-up" itself -
/// that's purely a `TS_BITMAP_DATA`-level convention the caller is
/// responsible for, see `rdpcore_server::encode_bitmap_update`).
pub fn encode_to(bgrx: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    let pixel_count = width * height;
    if pixel_count == 0 {
        // Nothing to encode — return just the format header.
        out.push(FORMAT_HEADER_RLE_NO_ALPHA_ARGB);
        return;
    }
    let needed_cap = 1 + pixel_count * 3 / 2;
    if out.capacity() < needed_cap {
        out.reserve(needed_cap - out.capacity());
    }
    out.push(FORMAT_HEADER_RLE_NO_ALPHA_ARGB);

    if pixel_count <= MAX_STACK_PIXELS {
        let mut r = [0u8; MAX_STACK_PIXELS];
        let mut g = [0u8; MAX_STACK_PIXELS];
        let mut b = [0u8; MAX_STACK_PIXELS];
        for (px, ((r_i, g_i), b_i)) in bgrx.chunks_exact(4).zip(
            r[..pixel_count]
                .iter_mut()
                .zip(g[..pixel_count].iter_mut())
                .zip(b[..pixel_count].iter_mut()),
        ) {
            *b_i = px[0];
            *g_i = px[1];
            *r_i = px[2];
        }
        encode_plane_stack(&r[..pixel_count], width, height, out);
        encode_plane_stack(&g[..pixel_count], width, height, out);
        encode_plane_stack(&b[..pixel_count], width, height, out);
    } else {
        let mut r = vec![0u8; pixel_count];
        let mut g = vec![0u8; pixel_count];
        let mut b = vec![0u8; pixel_count];
        for (px, ((r_i, g_i), b_i)) in bgrx
            .chunks_exact(4)
            .zip(r.iter_mut().zip(g.iter_mut()).zip(b.iter_mut()))
        {
            *b_i = px[0];
            *g_i = px[1];
            *r_i = px[2];
        }
        encode_plane_heap(&r, width, height, out);
        encode_plane_heap(&g, width, height, out);
        encode_plane_heap(&b, width, height, out);
    }
}

/// Convenience wrapper around [`encode_to`] returning a fresh `Vec<u8>`.
pub fn encode(bgrx: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + width * height * 3 / 2);
    encode_to(bgrx, width, height, &mut out);
    out
}

/// Inverse of [`encode`] - reconstructs a BGRX32 tile (X always 0).
pub fn decode(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, DecodeError> {
    let mut cursor = ReadCursor::new(data);
    let header = cursor.read_u8()?;
    if header != FORMAT_HEADER_RLE_NO_ALPHA_ARGB {
        return Err(DecodeError::InvalidValue {
            field: "rdp6.format_header",
            reason: "only RLE + no-alpha + ARGB (0x30) is supported",
        });
    }
    let r = decode_plane(&mut cursor, width, height)?;
    let g = decode_plane(&mut cursor, width, height)?;
    let b = decode_plane(&mut cursor, width, height)?;

    let pixel_count = width * height;
    let mut bgrx = vec![0u8; pixel_count * 4];
    for (px, ((&r_i, &g_i), &b_i)) in bgrx
        .chunks_exact_mut(4)
        .zip(r.iter().zip(g.iter()).zip(b.iter()))
    {
        px[0] = b_i;
        px[1] = g_i;
        px[2] = r_i;
    }
    Ok(bgrx)
}

// ---------------------------------------------------------------------
// Per-plane vertical delta filter (zigzag-packed)
// ---------------------------------------------------------------------

#[inline(always)]
fn zigzag_encode(raw_delta: u8) -> u8 {
    let signed = raw_delta as i8 as i16;
    ((signed << 1) ^ (signed >> 15)) as u8
}

#[inline(always)]
fn zigzag_decode(encoded: u8) -> u8 {
    let half = (encoded >> 1) as i16;
    let mask = (encoded as i16 & 1).wrapping_neg();
    (half ^ mask) as u8
}

fn encode_plane_stack(plane: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let mut delta = [0u8; MAX_STACK_PIXELS];
    delta[..width].copy_from_slice(&plane[..width]); // row 0: literal, unfiltered
    for row in 1..height {
        let (above_row, curr_row) = plane[(row - 1) * width..(row + 1) * width].split_at(width);
        let dst_row = &mut delta[row * width..(row + 1) * width];
        for ((dst, &curr), &above) in dst_row.iter_mut().zip(curr_row).zip(above_row) {
            *dst = zigzag_encode(curr.wrapping_sub(above));
        }
    }
    for row in 0..height {
        encode_scanline_rle(&delta[row * width..(row + 1) * width], out);
    }
}

fn encode_plane_heap(plane: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let mut delta = vec![0u8; width * height];
    delta[..width].copy_from_slice(&plane[..width]); // row 0: literal, unfiltered
    for row in 1..height {
        let (above_row, curr_row) = plane[(row - 1) * width..(row + 1) * width].split_at(width);
        let dst_row = &mut delta[row * width..(row + 1) * width];
        for ((dst, &curr), &above) in dst_row.iter_mut().zip(curr_row).zip(above_row) {
            *dst = zigzag_encode(curr.wrapping_sub(above));
        }
    }
    for row in 0..height {
        encode_scanline_rle(&delta[row * width..(row + 1) * width], out);
    }
}

fn decode_plane(
    cursor: &mut ReadCursor<'_>,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, DecodeError> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let mut delta = vec![0u8; width * height];
    for row in 0..height {
        decode_scanline_rle(cursor, &mut delta[row * width..(row + 1) * width])?;
    }
    let mut plane = vec![0u8; width * height];
    plane[..width].copy_from_slice(&delta[..width]);
    for row in 1..height {
        for col in 0..width {
            let idx = row * width + col;
            let transformed = zigzag_decode(delta[idx]);
            plane[idx] = plane[idx - width].wrapping_add(transformed);
        }
    }
    Ok(plane)
}

// ---------------------------------------------------------------------
// Per-scanline RLE: one control byte per segment,
// `control = (literal_count << 4) | run_field`.
//   run_field == 0        -> pure literal, no repeat (literal_count 0..15)
//   run_field in 3..=15   -> literal_count literal bytes (0..15, 0 means
//                            "continue the previous segment's last byte"),
//                            then repeat the last of those bytes run_field
//                            more times
//   run_field in {1, 2}   -> reserved "extended run" forms (16+extra /
//                            32+extra repeats, no literal bytes) - this
//                            encoder never emits them (any run needing
//                            them is instead expressed as a longer chain
//                            of run_field-3..=15 segments plus a final
//                            1-2-byte literal tail), but decode supports
//                            them for completeness/spec-compliance.
// A segment must never straddle a scanline boundary, and the "last byte"
// used for repeats/continuation resets to 0 at the start of every
// scanline.
// ---------------------------------------------------------------------

struct LiteralBuf {
    buf: [u8; 15],
    len: usize,
}

impl LiteralBuf {
    #[inline]
    const fn new() -> Self {
        Self {
            buf: [0u8; 15],
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, byte: u8, out: &mut Vec<u8>) {
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len == 15 {
            self.flush(out);
        }
    }

    #[inline]
    fn flush(&mut self, out: &mut Vec<u8>) {
        if self.len > 0 {
            out.push((self.len as u8) << 4); // run_field = 0
            out.extend_from_slice(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

fn encode_scanline_rle(scanline: &[u8], out: &mut Vec<u8>) {
    let mut pending = LiteralBuf::new();
    let mut i = 0;
    while i < scanline.len() {
        let byte = scanline[i];
        let mut count = 1;
        while i + count < scanline.len() && scanline[i + count] == byte {
            count += 1;
        }
        if count < 4 {
            for _ in 0..count {
                pending.push(byte, out);
            }
        } else {
            pending.flush(out);
            emit_run(byte, count, out);
        }
        i += count;
    }
    pending.flush(out);
}

/// `count` is always >= 4 here (smaller runs are handled as plain
/// literals by the caller) - see the module doc comment for why
/// `run_field` 1/2 are never used.
fn emit_run(byte: u8, count: usize, out: &mut Vec<u8>) {
    let mut remaining = count - 1;
    let first_run = remaining.min(15); // >= 3, since count >= 4
    out.push((1u8 << 4) | (first_run as u8)); // literal_count=1 (the byte itself), run_field=first_run
    out.push(byte);
    remaining -= first_run;

    while remaining > 0 {
        if remaining >= 3 {
            let chunk = remaining.min(15);
            out.push(chunk as u8); // literal_count=0 (continuation), run_field=chunk
            remaining -= chunk;
        } else {
            // 1 or 2 bytes left - run_field can't express that (reserved
            // for the extended form), so emit them as literals instead.
            out.push((remaining as u8) << 4); // run_field = 0
            out.extend(std::iter::repeat_n(byte, remaining));
            remaining = 0;
        }
    }
}

fn decode_scanline_rle(
    cursor: &mut ReadCursor<'_>,
    scanline: &mut [u8],
) -> Result<(), DecodeError> {
    let mut last_byte = 0u8;
    let mut pos = 0;
    while pos < scanline.len() {
        let control = cursor.read_u8()?;
        if control == 0 {
            return Err(DecodeError::InvalidValue {
                field: "rdp6.rle.control",
                reason: "0x00 control byte is invalid",
            });
        }
        let upper = control >> 4;
        let rle_field = control & 0x0F;
        let (run_length, literal_count): (usize, usize) = match rle_field {
            1 => (16 + usize::from(upper), 0),
            2 => (32 + usize::from(upper), 0),
            n => (usize::from(n), usize::from(upper)),
        };

        for _ in 0..literal_count {
            if pos >= scanline.len() {
                return Err(DecodeError::InvalidValue {
                    field: "rdp6.rle.scanline",
                    reason: "segment overruns the scanline",
                });
            }
            let byte = cursor.read_u8()?;
            scanline[pos] = byte;
            last_byte = byte;
            pos += 1;
        }
        for _ in 0..run_length {
            if pos >= scanline.len() {
                return Err(DecodeError::InvalidValue {
                    field: "rdp6.rle.scanline",
                    reason: "segment overruns the scanline",
                });
            }
            scanline[pos] = last_byte;
            pos += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tile(
        width: usize,
        height: usize,
        mut pixel: impl FnMut(usize, usize) -> (u8, u8, u8),
    ) -> Vec<u8> {
        let mut out = vec![0u8; width * height * 4];
        for row in 0..height {
            for col in 0..width {
                let (b, g, r) = pixel(col, row);
                let idx = (row * width + col) * 4;
                out[idx] = b;
                out[idx + 1] = g;
                out[idx + 2] = r;
                out[idx + 3] = 0;
            }
        }
        out
    }

    #[test]
    fn zigzag_round_trips_every_byte_value() {
        for raw in 0u8..=255 {
            assert_eq!(zigzag_decode(zigzag_encode(raw)), raw, "raw delta {raw}");
        }
    }

    #[test]
    fn solid_color_tile_round_trips_and_compresses_well() {
        let bgrx = make_tile(64, 64, |_, _| (10, 20, 30));
        let compressed = encode(&bgrx, 64, 64);
        assert!(
            compressed.len() < bgrx.len() / 10,
            "solid color should compress at least 10x, got {} bytes",
            compressed.len()
        );
        let decoded = decode(&compressed, 64, 64).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn gradient_tile_round_trips() {
        let bgrx = make_tile(64, 64, |x, y| {
            ((x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8)
        });
        let compressed = encode(&bgrx, 64, 64);
        let decoded = decode(&compressed, 64, 64).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn noisy_tile_round_trips() {
        // Pseudo-random via a simple LCG - no external `rand` dependency
        // in this crate, and determinism matters more than real entropy.
        let mut state: u32 = 0x12345678;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        let bgrx = make_tile(37, 29, |_, _| (next(), next(), next()));
        let compressed = encode(&bgrx, 37, 29);
        let decoded = decode(&compressed, 37, 29).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn heap_path_tile_round_trips() {
        // 128x96 = 12288 pixels, above MAX_STACK_PIXELS (4096) - exercises
        // `encode_plane_heap`/its heap-allocated delta buffer. Not just a
        // theoretical case: `rdpcore-server`'s raw-strip path (used for
        // compat clients without RDP6 Planar) already produces tiles this
        // large or larger for typical desktop widths (e.g. a 1920-wide
        // strip is 8 rows tall to fit the 16-bit length budget, 15360
        // pixels), so the stack-array fast path in `encode_to` is not the
        // only one that needs to stay correct.
        let mut state: u32 = 0xC0FFEE;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        let bgrx = make_tile(128, 96, |_, _| (next(), next(), next()));
        let compressed = encode(&bgrx, 128, 96);
        let decoded = decode(&compressed, 128, 96).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn runs_crossing_the_extended_length_thresholds_round_trip() {
        // Exercise run lengths right around 16/32 (the extended-form
        // boundaries this encoder deliberately avoids) and past it.
        for &run_len in &[3usize, 4, 15, 16, 17, 31, 32, 33, 60] {
            let width = run_len + 5;
            let bgrx = make_tile(width, 3, |x, _| {
                if x < run_len {
                    (7, 7, 7)
                } else {
                    (200, 200, 200)
                }
            });
            let compressed = encode(&bgrx, width, 3);
            let decoded = decode(&compressed, width, 3).unwrap();
            assert_eq!(decoded, bgrx, "run length {run_len}");
        }
    }

    #[test]
    fn single_pixel_tile_round_trips() {
        let bgrx = make_tile(4, 1, |_, _| (1, 2, 3));
        let compressed = encode(&bgrx, 4, 1);
        let decoded = decode(&compressed, 4, 1).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn format_header_is_rle_no_alpha_argb() {
        let bgrx = make_tile(4, 4, |_, _| (1, 2, 3));
        let compressed = encode(&bgrx, 4, 4);
        assert_eq!(compressed[0], FORMAT_HEADER_RLE_NO_ALPHA_ARGB);
    }

    #[test]
    fn decode_rejects_unsupported_format_header() {
        let err = decode(&[0x00, 0, 0, 0], 4, 1).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidValue {
                field: "rdp6.format_header",
                ..
            }
        ));
    }

    #[test]
    fn decode_rejects_zero_control_byte() {
        // FormatHeader + a lone 0x00 control byte for the R plane.
        let err = decode(&[FORMAT_HEADER_RLE_NO_ALPHA_ARGB, 0x00], 4, 1).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidValue {
                field: "rdp6.rle.control",
                ..
            }
        ));
    }

    #[test]
    fn zero_height_encode_does_not_panic() {
        let out = encode(&[], 64, 0);
        assert_eq!(out, vec![FORMAT_HEADER_RLE_NO_ALPHA_ARGB]);
    }

    #[test]
    fn zero_width_encode_does_not_panic() {
        let out = encode(&[], 0, 64);
        assert_eq!(out, vec![FORMAT_HEADER_RLE_NO_ALPHA_ARGB]);
    }

    proptest::proptest! {
        #[test]
        fn prop_rdp6_round_trip(
            w_blocks in 1usize..=8,
            h in 1usize..=8,
            seed: u32,
        ) {
            let w = w_blocks * 4;
            let bgrx = make_tile(w, h, |x, y| {
                let s = seed.wrapping_add((y * w + x) as u32);
                ((s & 0xFF) as u8, ((s >> 8) & 0xFF) as u8, ((s >> 16) & 0xFF) as u8)
            });
            let encoded = encode(&bgrx, w, h);
            let decoded = decode(&encoded, w, h).unwrap();
            proptest::prop_assert_eq!(decoded, bgrx);
        }

        #[test]
        fn prop_rdp6_arbitrary_bytes_do_not_panic(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..128),
            w_blocks in 1usize..=4,
            h in 1usize..=4,
        ) {
            let w = w_blocks * 4;
            let _ = decode(&data, w, h);
        }
    }
}
